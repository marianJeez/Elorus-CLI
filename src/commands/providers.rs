use std::fmt::Write as _;
use std::sync::LazyLock;

use anyhow::{anyhow, bail};
use futures_util::future::BoxFuture;
use serde::Deserialize;

use super::{Command, Ctx, Outcome, Scope, Spec};
use crate::auth;
use crate::data;
use crate::io::{self, term};
use crate::providers::{self, KeyState, Wire, keys};
use crate::text::styled::{Line, Style};
use crate::text::table::{self, Align};
use crate::text::template::fill;

static SPEC: LazyLock<Spec> = super::spec!("providers");
static TEXT: LazyLock<Text> = data::load!("commands/providers.toml", "text");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Text {
    added: String,
    exists: String,
    key_prompt: String,
    key_set: String,
    key_env_shadow: String,
    key_removed: String,
    key_missing: String,
    no_terminal: String,
    unknown: String,
    bad_wire: String,
    bad_base: String,
    session: String,
    note: String,
}

pub struct Providers;

impl Command for Providers {
    fn spec(&self) -> &Spec {
        &SPEC
    }

    fn run<'a>(
        &'a self,
        cx: &'a Ctx<'a>,
        args: &'a [String],
    ) -> BoxFuture<'a, anyhow::Result<Outcome>> {
        Box::pin(async move {
            let (action, rest) = match args.split_first() {
                Some((action, rest)) => (action.as_str(), rest),
                None => ("list", &[][..]),
            };
            if cx.scope == Scope::Session && action != "list" {
                bail!(TEXT.session.as_str());
            }
            Ok(match action {
                "list" if rest.is_empty() => list(cx),
                "add" => add(cx, rest)?,
                "set-key" => set_key(cx, rest)?,
                "remove-key" => remove_key(cx, rest)?,
                _ => return Err(cx.usage(&SPEC)),
            }
            .into())
        })
    }
}

fn list(cx: &Ctx<'_>) -> String {
    let (providers, config_note) = providers::registry::load(&cx.app.paths);
    let mut rows = vec![vec![
        Line::styled("provider", Style::fg("muted").bold()),
        Line::styled("wire", Style::fg("muted").bold()),
        Line::styled("key", Style::fg("muted").bold()),
        Line::styled("base", Style::fg("muted").bold()),
    ]];
    for provider in &providers {
        rows.push(vec![
            Line::plain(&provider.id),
            Line::plain(match provider.wire {
                Wire::OpenAi => "openai",
                Wire::Anthropic => "anthropic",
            }),
            Line::plain(key_text(&provider.key_state(&cx.app.paths))),
            Line::plain(&provider.base_url),
        ]);
    }
    let aligns = vec![Align::Left; 4];
    let mut out = table::plain_text(&rows, &aligns, &[3], term::out_width());
    let _ = writeln!(
        out,
        "\n{}",
        fill(
            &TEXT.note,
            &[("path", &cx.app.paths.provider_keys().display().to_string())]
        )
    );
    if let Some(why) = config_note {
        let _ = writeln!(out, "{why}");
    }
    out
}

fn key_text(state: &KeyState) -> String {
    match state {
        KeyState::Env(name) => format!("env:{name}"),
        KeyState::Stored => "stored".into(),
        KeyState::Missing => "missing".into(),
        KeyState::NotNeeded => "not needed".into(),
    }
}

fn add(cx: &Ctx<'_>, args: &[String]) -> anyhow::Result<String> {
    let Some((id, rest)) = args.split_first() else {
        return Err(cx.usage(&SPEC));
    };
    let mut base = None;
    let mut label = None;
    let mut key_env = None;
    let mut wire = Wire::OpenAi;
    let mut at = 0;
    while at < rest.len() {
        let value = || rest.get(at + 1).cloned();
        match rest[at].as_str() {
            "--base" => base = value(),
            "--label" => label = value(),
            "--key-env" => key_env = value(),
            "--wire" => {
                let name = value().unwrap_or_default();
                wire = match name.as_str() {
                    "openai" => Wire::OpenAi,
                    "anthropic" => Wire::Anthropic,
                    _ => bail!(fill(&TEXT.bad_wire, &[("wire", &name)])),
                };
            }
            _ => return Err(cx.usage(&SPEC)),
        }
        at += 2;
    }
    let base = base.ok_or_else(|| cx.usage(&SPEC))?;
    if !(base.starts_with("http://") || base.starts_with("https://")) {
        bail!(TEXT.bad_base.as_str());
    }
    let path = cx.app.paths.providers();
    if providers::all(&cx.app.paths).iter().any(|p| p.id == *id) {
        bail!(fill(
            &TEXT.exists,
            &[("id", id), ("path", &path.display().to_string())]
        ));
    }
    let mut block = String::new();
    let _ = writeln!(block, "[[provider]]");
    let _ = writeln!(block, "id = {}", quote(id));
    let _ = writeln!(block, "label = {}", quote(label.as_deref().unwrap_or(id)));
    let _ = writeln!(
        block,
        "wire = {}",
        quote(match wire {
            Wire::OpenAi => "openai",
            Wire::Anthropic => "anthropic",
        })
    );
    let _ = writeln!(block, "base_url = {}", quote(&base));
    let _ = writeln!(block, "discover = true");
    if let Some(env) = &key_env {
        let _ = writeln!(block, "key_env = {}", quote(env));
    }
    let mut text = io::fs::read_string(&path)?.unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push('\n');
    text.push_str(&block);
    io::fs::write_atomic(&path, text.as_bytes(), io::fs::Access::Shared)?;
    Ok(fill(
        &TEXT.added,
        &[("id", id), ("path", &path.display().to_string())],
    ))
}

fn set_key(cx: &Ctx<'_>, args: &[String]) -> anyhow::Result<String> {
    let (id, given) = match args {
        [id] => (id, None),
        [id, key] => (id, Some(key.clone())),
        _ => return Err(cx.usage(&SPEC)),
    };
    let provider = providers::find(&cx.app.paths, id)
        .ok_or_else(|| anyhow!(fill(&TEXT.unknown, &[("id", id)])))?;
    let key = match given {
        Some(key) => key,
        None => {
            if !term::stdin_is_terminal() {
                bail!(fill(&TEXT.no_terminal, &[("id", id)]));
            }
            term::read_secret(&fill(&TEXT.key_prompt, &[("id", id)]))?
                .ok_or_else(|| anyhow!("cancelled"))?
        }
    };
    let key = key.trim();
    if key.is_empty() {
        bail!("the key is empty");
    }
    keys::set(&cx.app.paths, id, key)?;
    let mut out = fill(&TEXT.key_set, &[("id", id), ("hint", &auth::hint(key))]);
    if let Some(env) = &provider.key_env
        && io::env::var(env).is_some()
    {
        out.push_str(&fill(&TEXT.key_env_shadow, &[("env", env)]));
    }
    Ok(out)
}

fn remove_key(cx: &Ctx<'_>, args: &[String]) -> anyhow::Result<String> {
    let [id] = args else {
        return Err(cx.usage(&SPEC));
    };
    if providers::find(&cx.app.paths, id).is_none() {
        bail!(fill(&TEXT.unknown, &[("id", id)]));
    }
    Ok(match keys::remove(&cx.app.paths, id)? {
        true => fill(&TEXT.key_removed, &[("id", id)]),
        false => fill(&TEXT.key_missing, &[("id", id)]),
    })
}

/// A TOML string with the two characters that would otherwise end it escaped.
fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;

    #[tokio::test]
    async fn add_writes_a_provider_the_catalog_then_lists() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::for_tests(dir.path());
        let cx = Ctx {
            app: &app,
            scope: Scope::Cli,
            session: None,
        };
        let words = |s: &str| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        let out = Providers
            .run(
                &cx,
                &words("add local --base http://127.0.0.1:9/v1 --label Local"),
            )
            .await
            .unwrap();
        assert!(out.text.contains("Added `local`"), "{}", out.text);
        let listed = Providers.run(&cx, &words("list")).await.unwrap();
        assert!(listed.text.contains("local"), "{}", listed.text);
        assert!(listed.text.contains("missing"), "{}", listed.text);
        let err = Providers
            .run(&cx, &words("add local --base http://127.0.0.1:9/v1"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already a provider"), "{err}");
    }

    #[tokio::test]
    async fn a_session_only_lists() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::for_tests(dir.path());
        let cx = Ctx {
            app: &app,
            scope: Scope::Session,
            session: None,
        };
        let words = |s: &str| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
        let err = Providers
            .run(&cx, &words("set-key openai sk-x"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("only lists"), "{err}");
    }
}
