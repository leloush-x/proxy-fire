//! Shared provider plumbing: the trait every provider implements, plus the
//! registry that maps provider IDs to implementations.

use futures::future::BoxFuture;
use proxy_fire_core::catalog::CatalogEntry;
use proxy_fire_core::config::ProviderConfig;
use serde_json::Value;
use std::sync::OnceLock;
use std::time::Duration;

mod cerebras;
mod gemini;
mod generic;
mod github_models;
mod groq;
mod huggingface;
mod openrouter;

pub mod llm_refresh;
pub mod prober;

pub use llm_refresh::{merge_diff, RefreshDiff, RemovedEntry};
pub use prober::{ProbeResult, Prober};

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("{0}")]
    Msg(String),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("parse: {0}")]
    Parse(String),
}

/// The provider plugin interface. The fallback chain talks only to this
/// trait; provider-specific behavior lives in the provider modules.
pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;

    /// Extra headers to send on chat requests (merged over config's
    /// extra_headers).
    fn extra_headers(&self, _cfg: &ProviderConfig) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Post-process a response body before the client sees it
    /// (e.g. Gemini strips <thought>…</thought>). Returns the possibly
    /// rewritten body.
    fn post_process_response(&self, _status: u16, body: Vec<u8>) -> Vec<u8> {
        body
    }

    /// Model is still loading (cold start): how long to wait before one
    /// retry of the same request, or None if it isn't a cold-start reply.
    fn cold_start_wait(&self, _status: u16, _body: &[u8]) -> Option<Duration> {
        None
    }

    /// Whether the provider speaks the models[] intra-provider fallback array.
    fn supports_models_array(&self) -> bool {
        false
    }

    /// Scan the provider's model API and return its free chat models.
    fn scan<'a>(
        &'a self,
        client: &'a reqwest::Client,
        cfg: &'a ProviderConfig,
    ) -> BoxFuture<'a, Result<Vec<CatalogEntry>, ProviderError>>;
}

/// All providers with any dedicated behavior, keyed by config ID.
/// Everything else (mistral, cohere, together, nvidia-nim, the long tail
/// of compatible APIs) uses the generic /models scanner via `get`'s fallback.
pub fn all() -> &'static [Box<dyn Provider>] {
    static ALL: OnceLock<Vec<Box<dyn Provider>>> = OnceLock::new();
    ALL.get_or_init(|| {
        vec![
            Box::new(groq::Groq),
            Box::new(gemini::Gemini),
            Box::new(cerebras::Cerebras),
            Box::new(openrouter::OpenRouter),
            Box::new(github_models::GitHubModels),
            Box::new(huggingface::HuggingFace),
        ]
    })
}

/// Look up a provider by config ID. Unknown IDs get the generic scanner,
/// matching the Go implementation's `scannerFor` default arm.
pub fn get(id: &str) -> &'static dyn Provider {
    all()
        .iter()
        .map(|b| b.as_ref())
        .find(|p| p.id().eq_ignore_ascii_case(id))
        .unwrap_or(&generic::DEFAULT)
}

/// Chat-completions endpoint for a provider config.
pub fn chat_endpoint(cfg: &ProviderConfig) -> String {
    format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'))
}

/// Build a GET request with the provider's auth/extra headers applied.
pub fn authorized_get(
    client: &reqwest::Client,
    url: &str,
    cfg: &ProviderConfig,
) -> reqwest::RequestBuilder {
    let mut req = client.get(url);
    let (h, v) = cfg.resolved_auth();
    if !v.is_empty() && v != "Bearer " {
        req = req.header(&h, v);
    }
    for (k, val) in &cfg.extra_headers {
        req = req.header(k, val);
    }
    req
}

/// Filter discovered models down to free chat models (shared by scanners).
pub fn chat_entries(provider_id: &str, tier_type: &str, raw: Vec<RawModel>) -> Vec<CatalogEntry> {
    let tier = if tier_type.is_empty() {
        "free"
    } else {
        tier_type
    };
    let now = chrono::Utc::now();
    raw.into_iter()
        .filter(|m| proxy_fire_core::catalog::is_chat_model(&m.model_id))
        .map(|m| CatalogEntry {
            provider_id: provider_id.to_string(),
            model_id: m.model_id,
            is_free: true,
            tier_type: tier.to_string(),
            context_window: m.context_window,
            discovered_at: Some(now),
            last_verified_at: Some(now),
            metadata: m.metadata,
            ..Default::default()
        })
        .collect()
}

/// Intermediate scan result before conversion to a CatalogEntry.
#[derive(Debug, Default)]
pub struct RawModel {
    pub model_id: String,
    pub context_window: i64,
    pub metadata: Option<serde_json::Map<String, Value>>,
}

/// Parse a standard OpenAI /models response body.
#[derive(serde::Deserialize)]
pub struct ModelsResponse {
    #[serde(default)]
    pub data: Vec<ModelItem>,
}

#[derive(serde::Deserialize)]
pub struct ModelItem {
    pub id: String,
    #[serde(default)]
    pub context_length: Option<i64>,
    #[serde(default)]
    pub context_window: Option<i64>,
    // Cohere lists supported operations; Together lists a model type.
    #[serde(default)]
    pub endpoints: Vec<String>,
    #[serde(default)]
    pub r#type: String,
}
