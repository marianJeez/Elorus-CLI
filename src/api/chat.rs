//! One model turn: send the conversation, stream the reply, retry what is safe to retry.
//! Never fails: every outcome, including errors and cancellation, is an `Assistant` message.

use std::time::Duration;

use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::api::assemble::{Assembler, Delta};
use crate::api::error::ApiError;
use crate::api::models::{BUILTIN, Model};
use crate::api::sse;
use crate::api::wire::{self, WireUsage};
use crate::api::{Call, retry};
use crate::app::App;
use crate::config::limits;
use crate::conversation::error::ErrorInfo;
use crate::conversation::message::{Assistant, Finish, Message, Usage};
use crate::io;
use crate::providers::{self, Provider, Wire};

pub struct Turn<'a> {
    pub model: &'a Model,
    pub effort: Option<&'a str>,
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [serde_json::Value],
}

impl Turn<'_> {
    /// The effort sent and recorded: none for a model that does not reason.
    pub fn effort(&self) -> Option<&str> {
        self.model.effort(self.effort)
    }
}

pub enum Event<'a> {
    Delta(Delta<'a>),
    Retry {
        attempt: u32,
        delay: Duration,
        error: &'a ErrorInfo,
    },
}

pub async fn stream(
    app: &App,
    turn: &Turn<'_>,
    cancel: &CancellationToken,
    on: &mut (dyn FnMut(Event<'_>) + Send),
) -> Assistant {
    let route = match Route::of(app, turn) {
        Ok(route) => route,
        Err(info) => return failed(turn, info),
    };
    let mut attempt = 1;
    loop {
        let outcome = attempt_once(app, &route, turn, cancel, on).await;
        if !outcome.emitted
            && let Some(error) = &outcome.message.error
            && let Some(delay) = retry::delay(error, outcome.retry_after, attempt)
        {
            on(Event::Retry {
                attempt,
                delay,
                error,
            });
            tokio::select! {
                () = io::clock::sleep(delay) => {}
                () = cancel.cancelled() => return reply(turn, Finish::Aborted),
            }
            attempt += 1;
            continue;
        }
        return outcome.message;
    }
}

/// Where this turn goes, its body built once so a retry sends exactly what the first try did.
struct Route {
    target: Target,
    body: serde_json::Value,
    wire: Wire,
}

enum Target {
    Elorus,
    Provider {
        provider: Box<Provider>,
        key: Option<String>,
    },
}

impl Route {
    fn of(app: &App, turn: &Turn<'_>) -> Result<Self, ErrorInfo> {
        if turn.model.provider == BUILTIN {
            return Ok(Self {
                target: Target::Elorus,
                body: serde_json::to_value(wire::Request::from(turn))
                    .expect("chat requests serialize"),
                wire: Wire::OpenAi,
            });
        }
        let provider = providers::find(&app.paths, &turn.model.provider).ok_or_else(|| {
            ErrorInfo::local(
                "provider_unknown",
                format!(
                    "no provider `{}`; see `elorus providers`",
                    turn.model.provider
                ),
            )
        })?;
        let key = provider
            .key(&app.paths)
            .map_err(|e| ErrorInfo::local("provider_key_unreadable", e.to_string()))?;
        if provider.needs_key() && key.is_none() {
            let env = provider.key_env.as_deref().unwrap_or_default();
            return Err(ErrorInfo::local(
                "provider_key_missing",
                format!(
                    "no key for {}: set {env} or run `elorus providers set-key {}`",
                    provider.label, provider.id
                ),
            ));
        }
        Ok(Self {
            body: providers::request(&provider, turn),
            wire: provider.wire,
            target: Target::Provider {
                provider: Box::new(provider),
                key,
            },
        })
    }
}

struct Outcome {
    message: Assistant,
    emitted: bool,
    retry_after: Option<Duration>,
}

async fn attempt_once(
    app: &App,
    route: &Route,
    turn: &Turn<'_>,
    cancel: &CancellationToken,
    on: &mut (dyn FnMut(Event<'_>) + Send),
) -> Outcome {
    let limits = &limits::get().chat;
    let timeout = limits.headers_timeout_secs.secs();
    let opened = match &route.target {
        Target::Elorus => {
            let call = Call {
                body: Some(&route.body),
                timeout: Some(timeout),
                ..Call::default()
            };
            tokio::select! {
                r = app.api.open("chat", call) => r,
                () = cancel.cancelled() => return Outcome {
                    message: reply(turn, Finish::Aborted),
                    emitted: false,
                    retry_after: None,
                },
            }
        }
        Target::Provider { provider, key } => {
            tokio::select! {
                r = providers::open(&app.http, provider, &route.body, key.as_deref(), timeout) => r,
                () = cancel.cancelled() => return Outcome {
                    message: reply(turn, Finish::Aborted),
                    emitted: false,
                    retry_after: None,
                },
            }
        }
    };
    let resp = match opened {
        Ok(resp) => resp,
        Err(ApiError { info, retry_after }) => {
            return Outcome {
                message: failed(turn, info),
                emitted: false,
                retry_after,
            };
        }
    };

    let events = sse::events(
        resp.body,
        limits.idle_timeout_secs.secs(),
        limits.stall_timeout_secs.secs(),
        limits.max_event_bytes.value,
    );
    let mut events = std::pin::pin!(events);
    let mut decoder = providers::Decoder::new(route.wire);
    let mut asm = Assembler::default();
    let mut cut = None;
    let mut aborted = false;
    loop {
        let next = tokio::select! {
            next = events.next() => next,
            () = cancel.cancelled() => { aborted = true; break; }
        };
        match next {
            None => break,
            Some(Err(error)) => {
                cut = Some(error);
                break;
            }
            Some(Ok(data)) => match decoder.apply(&data) {
                Ok(Some(chunk)) => asm.apply(chunk, &mut |d| on(Event::Delta(d))),
                Ok(None) => {}
                Err(error) => {
                    cut = Some(error);
                    break;
                }
            },
        }
    }

    let emitted = asm.emitted;
    let request_id = resp.head.request_id.clone();
    let done = asm.finish(cut);
    let (finish, error) = if aborted {
        (Finish::Aborted, None)
    } else {
        (
            done.finish,
            done.error.map(|e| with_request_id(e, &request_id)),
        )
    };
    let usage = done.usage.map(|u| cost(turn.model, &u));
    Outcome {
        message: Assistant {
            parts: done.parts,
            usage,
            error,
            request_id,
            ..reply(turn, finish)
        },
        emitted,
        retry_after: None,
    }
}

fn cost(model: &Model, usage: &WireUsage) -> Usage {
    let cached = usage.cached().min(usage.prompt_tokens);
    let uncached = usage.prompt_tokens - cached;
    Usage {
        input: usage.prompt_tokens,
        cached,
        output: usage.completion_tokens,
        reasoning: usage.reasoning(),
        cost_micros: model
            .pricing
            .cost_micros(uncached, cached, usage.completion_tokens),
    }
}

fn with_request_id(mut error: ErrorInfo, request_id: &Option<String>) -> ErrorInfo {
    if error.request_id.is_none() {
        error.request_id.clone_from(request_id);
    }
    error
}

/// A reply with nothing in it, as a turn records one.
fn reply(turn: &Turn<'_>, finish: Finish) -> Assistant {
    Assistant {
        parts: Vec::new(),
        model: turn.model.id.clone(),
        effort: turn.effort().map(str::to_owned),
        usage: None,
        finish,
        error: None,
        request_id: None,
    }
}

/// A reply that never came, recording why.
fn failed(turn: &Turn<'_>, info: ErrorInfo) -> Assistant {
    Assistant {
        request_id: info.request_id.clone(),
        error: Some(info),
        ..reply(turn, Finish::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::model;

    #[test]
    fn cost_splits_cached_and_uncached_input() {
        let usage: WireUsage = serde_json::from_str(
            r#"{"prompt_tokens":1000,"completion_tokens":100,"prompt_tokens_details":{"cached_tokens":400}}"#,
        )
        .unwrap();
        let u = cost(&model(true), &usage);
        // 600 * 6 + 400 * 0.6 + 100 * 10 micro-dollars
        assert_eq!(u.cost_micros, 3600 + 240 + 1000);
        assert_eq!(u.cached, 400);
    }

    #[test]
    fn effort_is_sent_and_recorded_only_for_reasoning_models() {
        let m = model(false);
        let turn = Turn {
            model: &m,
            effort: Some("high"),
            system: "sys",
            messages: &[],
            tools: &[],
        };
        let recorded = |turn: &Turn<'_>| reply(turn, Finish::Done).effort;
        let sent = |turn: &Turn<'_>| wire::Request::from(turn).reasoning_effort;
        assert_eq!((sent(&turn), recorded(&turn)), (None, None));
        let m = model(true);
        let turn = Turn { model: &m, ..turn };
        assert_eq!(sent(&turn).as_deref(), Some("high"));
        assert_eq!(recorded(&turn).as_deref(), Some("high"));
    }
}
