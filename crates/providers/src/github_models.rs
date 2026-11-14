//! Provider: GitHub Models
//! Docs: https://docs.github.com/en/github-models
//! Free tier: any GitHub account. Discovery: https://models.github.ai/catalog/models
//! Catalog id is "publisher/slug"; the inference API uses just the slug.

use crate::{chat_entries, Provider, ProviderError, RawModel};
use futures::future::BoxFuture;
use proxy_fire_core::catalog::CatalogEntry;
use proxy_fire_core::config::ProviderConfig;
use serde::Deserialize;

pub struct GitHubModels;

const CATALOG_URL: &str = "https://models.github.ai/catalog/models";

/// Catalog IDs not available on the inference endpoint — blocklisted at scan time.
const SKIP_CATALOG_IDS: &[&str] = &[
    "ai21-labs/ai21-jamba-1.5-large",
    "meta/meta-llama-3.1-405b-instruct",
];

#[derive(Deserialize)]
struct CatalogModel {
    id: String,
    #[serde(default)]
    publisher: String,
    #[serde(default)]
    registry: String,
    #[serde(default)]
    rate_limit_tier: String,
    #[serde(default)]
    supported_input_modalities: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
}

impl Provider for GitHubModels {
    fn id(&self) -> &'static str {
        "github-models"
    }

    fn scan<'a>(
        &'a self,
        client: &'a reqwest::Client,
        cfg: &'a ProviderConfig,
    ) -> BoxFuture<'a, Result<Vec<CatalogEntry>, ProviderError>> {
        Box::pin(async move {
            let mut req = client.get(CATALOG_URL).header("Accept", "application/json");
            let api_key = cfg.resolved_api_key();
            if !api_key.is_empty() {
                req = req.header("Authorization", format!("Bearer {api_key}"));
            }
            let resp = req.send().await?;
            let status = resp.status();
            let body = resp.bytes().await?;
            if !status.is_success() {
                return Err(ProviderError::Msg(format!(
                    "github catalog: status {status}: {}",
                    String::from_utf8_lossy(&body)
                )));
            }
            let models: Vec<CatalogModel> = match serde_json::from_slice(&body) {
                Ok(v) => v,
                Err(_) => {
                    #[derive(Deserialize)]
                    struct Wrapped {
                        models: Vec<CatalogModel>,
                    }
                    serde_json::from_slice::<Wrapped>(&body)
                        .map(|w| w.models)
                        .map_err(|e| ProviderError::Parse(format!("github catalog: {e}")))?
                }
            };

            let raw: Vec<RawModel> = models
                .into_iter()
                .filter_map(|m| {
                    if m.rate_limit_tier == "embeddings" {
                        return None;
                    }
                    if SKIP_CATALOG_IDS.contains(&m.id.as_str()) {
                        return None;
                    }
                    let model_id = api_model_id(&m.id);
                    let mut meta = serde_json::Map::new();
                    meta.insert("catalog_id".into(), serde_json::Value::String(m.id));
                    if !m.publisher.is_empty() {
                        meta.insert("publisher".into(), serde_json::Value::String(m.publisher));
                    }
                    if !m.registry.is_empty() {
                        meta.insert("registry".into(), serde_json::Value::String(m.registry));
                    }
                    if !m.rate_limit_tier.is_empty() {
                        meta.insert(
                            "rate_limit_tier".into(),
                            serde_json::Value::String(m.rate_limit_tier),
                        );
                    }
                    let vision = m.supported_input_modalities.iter().any(|s| s == "image")
                        || m.tags.iter().any(|t| {
                            let l = t.to_lowercase();
                            l.contains("multimodal") || l.contains("vision")
                        });
                    let audio = m.supported_input_modalities.iter().any(|s| s == "audio");
                    if vision {
                        meta.insert("supports_vision".into(), serde_json::Value::Bool(true));
                    }
                    if audio {
                        meta.insert("supports_audio_input".into(), serde_json::Value::Bool(true));
                    }
                    Some(RawModel {
                        model_id,
                        context_window: 0,
                        metadata: Some(meta),
                    })
                })
                .collect();
            Ok(chat_entries("github-models", "free", raw))
        })
    }
}

/// Derive the API model name from the catalog id: strip "publisher/".
pub fn api_model_id(catalog_id: &str) -> String {
    match catalog_id.find('/') {
        Some(idx) => catalog_id[idx + 1..].to_string(),
        None => catalog_id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_publisher_prefix() {
        assert_eq!(api_model_id("openai/gpt-4.1"), "gpt-4.1");
        assert_eq!(
            api_model_id("meta/llama-3.3-70b-instruct"),
            "llama-3.3-70b-instruct"
        );
        assert_eq!(api_model_id("noprefix"), "noprefix");
    }
}
