//! The provider catalog: built-in entries from data/providers.toml, then the user's additions
//! and overrides from ~/.elorus/providers.toml.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::api::models::{Capabilities, Model, Pricing};
use crate::config::paths::Paths;
use crate::data;
use crate::io;
use crate::providers::keys;

const BUILTIN: &str = data::raw!("providers.toml");

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wire {
    #[default]
    #[serde(rename = "openai")]
    OpenAi,
    Anthropic,
}

/// A model a provider is known to serve when it cannot be asked.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seed {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub context: u64,
    #[serde(default)]
    pub max_output: u64,
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub cached: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default = "yes")]
    pub tools: bool,
    #[serde(default = "yes")]
    pub temperature: bool,
    #[serde(default)]
    pub caching: bool,
}

fn yes() -> bool {
    true
}

impl Seed {
    pub fn model(&self, provider: &str) -> Model {
        Model {
            id: format!("{provider}/{}", self.id),
            provider: provider.to_owned(),
            wire_id: Some(self.id.clone()),
            display_name: if self.name.is_empty() {
                self.id.clone()
            } else {
                self.name.clone()
            },
            description: String::new(),
            context_window: self.context,
            max_output_tokens: self.max_output,
            pricing: Pricing {
                input_per_1m_usd: self.input,
                cached_input_per_1m_usd: self.cached,
                output_per_1m_usd: self.output,
            },
            capabilities: Capabilities {
                streaming: true,
                reasoning: self.reasoning,
                tool_calling: self.tools,
                temperature: self.temperature,
                caching: self.caching,
            },
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    provider: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    id: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    wire: Wire,
    base_url: String,
    #[serde(default)]
    key_env: Option<String>,
    #[serde(default = "yes")]
    discover: bool,
    #[serde(default)]
    quiet: bool,
    #[serde(default)]
    chat_path: Option<String>,
    #[serde(default)]
    models_path: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    model: Vec<Seed>,
}

#[derive(Clone, Debug)]
pub struct Provider {
    pub id: String,
    pub label: String,
    pub wire: Wire,
    pub base_url: String,
    pub key_env: Option<String>,
    pub discover: bool,
    /// Whether a failed discovery is left out of the model list's notes (a local server that is
    /// simply not running).
    pub quiet: bool,
    chat_path: Option<String>,
    models_path: Option<String>,
    pub headers: Vec<(String, String)>,
    pub seeds: Vec<Seed>,
}

impl Provider {
    fn from(entry: Entry) -> Self {
        let label = entry.label.unwrap_or_else(|| entry.id.clone());
        Self {
            id: entry.id,
            label,
            wire: entry.wire,
            base_url: entry.base_url,
            key_env: entry.key_env,
            discover: entry.discover,
            quiet: entry.quiet,
            chat_path: entry.chat_path,
            models_path: entry.models_path,
            headers: entry.headers.into_iter().collect(),
            seeds: entry.model,
        }
    }

    pub fn chat_url(&self) -> String {
        let path = self.chat_path.as_deref().unwrap_or(match self.wire {
            Wire::OpenAi => "/chat/completions",
            Wire::Anthropic => "/messages",
        });
        self.url(path)
    }

    pub fn models_url(&self) -> String {
        self.url(self.models_path.as_deref().unwrap_or("/models"))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    /// The key to use: the environment's first, else a stored one. An unreadable store is an
    /// error rather than "no key": the fix is to set it again.
    pub fn key(&self, paths: &Paths) -> anyhow::Result<Option<String>> {
        if let Some(name) = &self.key_env
            && let Some(key) = io::env::var(name)
        {
            return Ok(Some(key));
        }
        keys::get(paths, &self.id)
    }

    pub fn key_state(&self, paths: &Paths) -> KeyState {
        if let Some(name) = &self.key_env
            && io::env::var(name).is_some()
        {
            return KeyState::Env(name.clone());
        }
        match keys::has(paths, &self.id) {
            Ok(true) => KeyState::Stored,
            _ if self.key_env.is_some() => KeyState::Missing,
            _ => KeyState::NotNeeded,
        }
    }

    /// Whether it cannot be called without a key.
    pub fn needs_key(&self) -> bool {
        self.key_env.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyState {
    /// Set in this variable.
    Env(String),
    /// Sealed in ~/.elorus/provider_keys.json.
    Stored,
    /// `key_env` is named, and neither source has one.
    Missing,
    /// The provider runs without one (a local server).
    NotNeeded,
}

/// Every provider in catalog order, and why a user file was left out, if it was.
pub fn load(paths: &Paths) -> (Vec<Provider>, Option<String>) {
    let builtin: File = data::parse(BUILTIN, "providers.toml");
    let mut providers: Vec<Provider> = builtin.provider.into_iter().map(Provider::from).collect();
    let text = match io::fs::read_string(&paths.providers()) {
        Ok(Some(text)) => text,
        Ok(None) => return (providers, None),
        Err(e) => {
            return (
                providers,
                Some(format!("{}: {e}", paths.providers().display())),
            );
        }
    };
    let file: File = match toml::from_str(&text) {
        Ok(file) => file,
        Err(e) => {
            return (
                providers,
                Some(format!("{}: {e}", paths.providers().display())),
            );
        }
    };
    for entry in file.provider {
        if entry.id.trim().is_empty() || entry.base_url.trim().is_empty() {
            continue;
        }
        let provider = Provider::from(entry);
        match providers.iter().position(|p| p.id == provider.id) {
            Some(at) => providers[at] = provider,
            None => providers.push(provider),
        }
    }
    (providers, None)
}

pub fn all(paths: &Paths) -> Vec<Provider> {
    load(paths).0
}

pub fn find(paths: &Paths, id: &str) -> Option<Provider> {
    all(paths).into_iter().find(|p| p.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builtin_catalog_parses_and_every_provider_has_a_url() {
        let builtin: File = data::parse(BUILTIN, "providers.toml");
        assert!(builtin.provider.len() >= 2);
        for entry in builtin.provider {
            let provider = Provider::from(entry);
            assert!(!provider.id.is_empty());
            assert!(provider.chat_url().starts_with("http"));
            assert!(provider.models_url().ends_with("/models"));
        }
    }

    #[test]
    fn a_user_file_overrides_by_id_and_adds_new_ones() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::at(home.path().into(), home.path().into());
        io::fs::write_atomic(
            &paths.providers(),
            br#"[[provider]]
id = "openai"
label = "Mine"
base_url = "https://example.test/v1"
wire = "openai"

[[provider]]
id = "local"
base_url = "http://127.0.0.1:9/v1"
"#,
            io::fs::Access::Shared,
        )
        .unwrap();
        let (providers, note) = load(&paths);
        assert_eq!(note, None);
        let openai = providers.iter().find(|p| p.id == "openai").unwrap();
        assert_eq!(openai.label, "Mine");
        assert!(providers.iter().any(|p| p.id == "local"));
    }

    #[test]
    fn a_broken_user_file_leaves_the_builtins_and_says_why() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::at(home.path().into(), home.path().into());
        io::fs::write_atomic(&paths.providers(), b"not toml", io::fs::Access::Shared).unwrap();
        let (providers, note) = load(&paths);
        assert!(providers.iter().any(|p| p.id == "openai"));
        assert!(note.unwrap().contains("providers.toml"));
    }
}
