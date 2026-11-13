//! The models elorus offers, in the shape `GET /v1/models` returns them.

use serde::{Deserialize, Serialize};

use crate::api::{Api, Call};
use crate::config::limits;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    /// Who serves it: `elorus` for the built-in gateway, else a provider id (src/providers).
    #[serde(default = "builtin")]
    pub provider: String,
    /// The id the provider itself knows it by, when `id` is qualified (`provider/model`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_id: Option<String>,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub pricing: Pricing,
    #[serde(default)]
    pub capabilities: Capabilities,
}

/// The provider every model in the bundled list and in elorus's own replies belongs to.
pub const BUILTIN: &str = "elorus";

fn builtin() -> String {
    BUILTIN.to_owned()
}

/// USD per million tokens.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Pricing {
    pub input_per_1m_usd: f64,
    #[serde(default)]
    pub cached_input_per_1m_usd: f64,
    pub output_per_1m_usd: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub streaming: bool,
    pub reasoning: bool,
    pub tool_calling: bool,
    pub temperature: bool,
    pub caching: bool,
}

impl Pricing {
    /// What these token counts cost; tokens times USD-per-million is exactly micro-dollars.
    pub fn cost_micros(&self, uncached: u64, cached: u64, output: u64) -> u64 {
        let micros = uncached as f64 * self.input_per_1m_usd
            + cached as f64 * self.cached_input_per_1m_usd
            + output as f64 * self.output_per_1m_usd;
        micros.round() as u64
    }
}

impl Model {
    /// The effort sent, recorded and shown: none for a model that does not reason.
    pub fn effort<'a>(&self, requested: Option<&'a str>) -> Option<&'a str> {
        requested.filter(|_| self.capabilities.reasoning)
    }

    /// The id to put in a request: the provider's own name, else the catalog id.
    pub fn wire_id(&self) -> &str {
        self.wire_id.as_deref().unwrap_or(&self.id)
    }

    /// Whether its prices are known; a discovered model often has none.
    pub fn priced(&self) -> bool {
        self.pricing.input_per_1m_usd > 0.0
            || self.pricing.cached_input_per_1m_usd > 0.0
            || self.pricing.output_per_1m_usd > 0.0
    }
}

/// The reply to `GET /v1/models`, also the format of data/models.json.
#[derive(Serialize, Deserialize)]
pub struct Listing {
    pub data: Vec<Model>,
}

/// The live list; `Err` says why it could not be had, an empty list included.
pub async fn fetch(api: &Api) -> Result<Vec<Model>, String> {
    let timeout = limits::get().models.fetch_timeout_ms.ms();
    let listing: Listing = api
        .call_json(
            "models",
            Call {
                timeout: Some(timeout),
                ..Call::default()
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    if listing.data.is_empty() {
        return Err("elorus returned an empty model list".into());
    }
    Ok(listing.data)
}
