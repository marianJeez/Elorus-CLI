//! Third-party model providers: where they live, what they speak, and how their keys are kept.
//! The built-in catalog is data/providers.toml; users add or override entries in
//! ~/.elorus/providers.toml.

pub mod anthropic;
pub mod discover;
pub mod keys;
pub mod registry;

use std::time::Duration;

pub use discover::models;
pub use registry::{KeyState, Provider, Wire, all, find};

use crate::api::chat::Turn;
use crate::api::error::ApiError;
use crate::api::wire::{self, Chunk};
use crate::conversation::error::ErrorInfo;
use crate::io::http::{Http, Method, Request, Streaming};

/// The request a turn becomes, in the provider's own wire.
pub fn request(provider: &Provider, turn: &Turn<'_>) -> serde_json::Value {
    match provider.wire {
        Wire::OpenAi => {
            serde_json::to_value(wire::Request::from(turn)).expect("a chat request serializes")
        }
        Wire::Anthropic => anthropic::request(turn),
    }
}

/// One connection attempt to `provider`, with its key and any headers it asks for.
pub async fn open(
    http: &Http,
    provider: &Provider,
    body: &serde_json::Value,
    key: Option<&str>,
    timeout: Duration,
) -> Result<Streaming, ApiError> {
    let headers = headers(provider, key);
    let borrowed: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let request = Request {
        method: Method::Post,
        url: provider.chat_url(),
        bearer: None,
        headers: &borrowed,
        json: Some(body),
        timeout,
    };
    crate::api::open(http, request).await
}

/// The provider's own headers, then whatever its wire needs to authenticate.
pub fn headers(provider: &Provider, key: Option<&str>) -> Vec<(String, String)> {
    let mut headers = provider.headers.clone();
    if let Some(key) = key {
        match provider.wire {
            Wire::OpenAi => headers.push(("Authorization".into(), format!("Bearer {key}"))),
            Wire::Anthropic => {
                headers.push(("x-api-key".into(), key.to_owned()));
                headers.push(("anthropic-version".into(), anthropic::VERSION.into()));
            }
        }
    }
    headers
}

/// Turns one SSE data frame into the OpenAI chunks the assembler folds.
pub enum Decoder {
    OpenAi,
    Anthropic(anthropic::Chunks),
}

impl Decoder {
    pub fn new(wire: Wire) -> Self {
        match wire {
            Wire::OpenAi => Self::OpenAi,
            Wire::Anthropic => Self::Anthropic(anthropic::Chunks::default()),
        }
    }

    pub fn apply(&mut self, data: &str) -> Result<Option<Chunk>, ErrorInfo> {
        match self {
            Self::OpenAi => serde_json::from_str(data)
                .map(Some)
                .map_err(|e| ErrorInfo::local("bad_stream", e.to_string())),
            Self::Anthropic(chunks) => chunks.apply(data),
        }
    }
}
