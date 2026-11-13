//! Provider model lists: the seeds each provider is known by, merged with what its `GET /models`
//! reports, cached so a session's start does not wait on every provider.

use std::collections::BTreeMap;

use futures_util::future::join_all;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::api::error::ApiError;
use crate::api::models::{Capabilities, Model, Pricing};
use crate::config::limits;
use crate::config::paths::Paths;
use crate::io;
use crate::io::fs::Access;
use crate::io::http::{Http, Method, Request};
use crate::providers::registry::{self, Provider};

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    #[serde(default)]
    providers: BTreeMap<String, Entry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    fetched_at: Timestamp,
    #[serde(default)]
    models: Vec<Model>,
    /// Why the last discovery failed; a fresh failure is not retried until the cache expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failed: Option<String>,
}

#[derive(Deserialize)]
struct Listing {
    #[serde(default)]
    data: Vec<WireModel>,
    #[serde(default)]
    models: Vec<WireModel>,
}

/// OpenAI, Anthropic and most compatible servers answer with objects; a few with bare ids.
#[derive(Deserialize)]
#[serde(untagged)]
enum WireModel {
    Id(String),
    Full {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        display_name: Option<String>,
    },
}

struct Plan {
    key: Option<String>,
    cached: Option<Entry>,
    fetch: bool,
    /// A provider that cannot work without a key is left out until it has one.
    skip: bool,
}

/// Every model the configured providers offer, and what went wrong for those that could not be
/// asked. Providers without a key are quiet: `elorus providers` is where that is said.
pub async fn models(http: &Http, paths: &Paths, refresh: bool) -> (Vec<Model>, Vec<String>) {
    let (providers, config_note) = registry::load(paths);
    let mut notes: Vec<String> = config_note.into_iter().collect();
    let cache_path = paths.providers_cache();
    let mut cache: Cache = io::fs::read_json(&cache_path)
        .ok()
        .flatten()
        .unwrap_or_default();
    let now = io::clock::now();
    let ttl = limits::get().models.cache_ttl_secs.signed_secs();

    let mut plans = Vec::with_capacity(providers.len());
    for provider in &providers {
        let key = match provider.key(paths) {
            Ok(key) => key,
            Err(e) => {
                notes.push(format!("{}: {e}", provider.label));
                plans.push(Plan {
                    key: None,
                    cached: None,
                    fetch: false,
                    skip: true,
                });
                continue;
            }
        };
        let missing = provider.needs_key() && key.is_none();
        let cached = cache.providers.get(&provider.id).cloned();
        let fresh = cached
            .as_ref()
            .is_some_and(|entry| io::clock::fresh(entry.fetched_at, now, ttl));
        plans.push(Plan {
            key,
            cached,
            fetch: provider.discover && !missing && (refresh || !fresh),
            skip: missing,
        });
    }

    let fetches = providers
        .iter()
        .zip(&plans)
        .enumerate()
        .filter(|(_, (_, plan))| plan.fetch)
        .map(|(i, (provider, plan))| {
            let key = plan.key.clone();
            async move { (i, fetch(http, provider, key.as_deref()).await) }
        });
    let results: BTreeMap<usize, Result<Vec<Model>, String>> =
        join_all(fetches).await.into_iter().collect();

    let mut out = Vec::new();
    let mut changed = false;
    for (i, provider) in providers.iter().enumerate() {
        let plan = &plans[i];
        if plan.skip {
            continue;
        }
        let mut discovered = plan
            .cached
            .as_ref()
            .map(|entry| entry.models.clone())
            .unwrap_or_default();
        if let Some(result) = results.get(&i) {
            match result {
                Ok(models) => {
                    cache.providers.insert(
                        provider.id.clone(),
                        Entry {
                            fetched_at: now,
                            models: models.clone(),
                            failed: None,
                        },
                    );
                    changed = true;
                    discovered = models.clone();
                }
                // A stale list still beats none: the reason is said, the models stay.
                Err(why) => {
                    if !provider.quiet {
                        notes.push(format!("{}: {why}", provider.label));
                    }
                    cache.providers.insert(
                        provider.id.clone(),
                        Entry {
                            fetched_at: now,
                            models: discovered.clone(),
                            failed: Some(why.clone()),
                        },
                    );
                    changed = true;
                }
            }
        } else if !plan.fetch
            && !provider.quiet
            && let Some(why) = plan
                .cached
                .as_ref()
                .and_then(|entry| entry.failed.as_deref())
        {
            notes.push(format!("{}: {why}", provider.label));
        }
        out.extend(merge(provider, discovered));
    }
    if changed {
        let _ = io::fs::write_json(&cache_path, &cache, Access::Shared);
    }
    (out, notes)
}

/// The seeds first, with their known prices and limits, then whatever discovery added.
fn merge(provider: &Provider, discovered: Vec<Model>) -> Vec<Model> {
    let mut out: Vec<Model> = provider
        .seeds
        .iter()
        .map(|s| s.model(&provider.id))
        .collect();
    for model in discovered {
        if !out.iter().any(|known| known.wire_id() == model.wire_id()) {
            out.push(model);
        }
    }
    out
}

async fn fetch(http: &Http, provider: &Provider, key: Option<&str>) -> Result<Vec<Model>, String> {
    let headers = super::headers(provider, key);
    let borrowed: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let request = Request {
        method: Method::Get,
        url: provider.models_url(),
        bearer: None,
        headers: &borrowed,
        json: None,
        timeout: limits::get().models.fetch_timeout_ms.ms(),
    };
    let resp = http
        .send(request)
        .await
        .map_err(|e| ApiError::from(e).to_string())?;
    if resp.head.status >= 400 {
        return Err(ApiError::from_response(&resp).to_string());
    }
    let listing: Listing =
        serde_json::from_slice(&resp.body).map_err(|e| format!("unexpected model list: {e}"))?;
    let mut models: Vec<Model> = Vec::new();
    for wire in listing.data.into_iter().chain(listing.models) {
        let Some(model) = model(provider, wire) else {
            continue;
        };
        if !models.iter().any(|m| m.wire_id() == model.wire_id()) {
            models.push(model);
        }
    }
    if models.is_empty() {
        return Err("returned no models".into());
    }
    Ok(models)
}

fn model(provider: &Provider, wire: WireModel) -> Option<Model> {
    let (id, name) = match wire {
        WireModel::Id(id) => {
            let name = id.clone();
            (id, name)
        }
        WireModel::Full {
            id,
            name,
            display_name,
        } => {
            let id = id.or_else(|| name.clone())?;
            let name = display_name.or(name).unwrap_or_else(|| id.clone());
            (id, name)
        }
    };
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    Some(Model {
        id: format!("{}/{}", provider.id, id),
        provider: provider.id.clone(),
        wire_id: Some(id.to_owned()),
        display_name: name,
        description: String::new(),
        context_window: 0,
        max_output_tokens: 0,
        pricing: Pricing::default(),
        capabilities: Capabilities {
            streaming: true,
            tool_calling: true,
            temperature: true,
            ..Capabilities::default()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, routing::get};

    #[tokio::test]
    async fn a_discovered_list_is_qualified_cached_and_merged_with_seeds() {
        let base = crate::testing::serve(|_| {
            Router::new().route(
                "/models",
                get(|| async {
                    r#"{"data":[{"id":"m1","display_name":"One"},{"id":"seed-model"}]}"#
                }),
            )
        })
        .await;
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::at(home.path().into(), home.path().into());
        io::fs::write_atomic(
            &paths.providers(),
            format!(
                r#"[[provider]]
id = "fake"
base_url = "{base}"
discover = true

[[provider.model]]
id = "seed-model"
name = "Seed"
context = 1000
"#
            )
            .as_bytes(),
            Access::Shared,
        )
        .unwrap();
        let http = Http::new(std::time::Duration::from_secs(1)).unwrap();
        let (found, notes) = models(&http, &paths, true).await;
        assert!(notes.is_empty(), "{notes:?}");
        let ids: Vec<&str> = found.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["fake/seed-model", "fake/m1"]);
        assert_eq!(found[0].context_window, 1000);
        assert!(!found[1].priced());
        assert!(io::fs::exists(&paths.providers_cache()));
        // A second load needs no server: the cache holds the list.
        let (again, _) = models(&http, &paths, false).await;
        assert_eq!(again.len(), 2);
    }
}
