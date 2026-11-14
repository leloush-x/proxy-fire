//! Provider: OpenRouter
//! Docs: https://openrouter.ai/docs/quickstart
//! Free tier: 50 req/day (1000 with $10+ balance).
//! Native fallbacks: models[] array; the ultimate "openrouter/free" router.
//! Auth: Authorization: Bearer. HTTP-Referer / X-Title required (via extra_headers).

use crate::{chat_entries, Provider, ProviderError, RawModel};
use futures::future::BoxFuture;
use proxy_fire_core::catalog::CatalogEntry;
use proxy_fire_core::config::ProviderConfig;
use serde::Deserialize;

pub struct OpenRouter;

#[derive(Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<Model>,
}

#[derive(Deserialize)]
struct Model {
    id: String,
    #[serde(default)]
    context_length: i64,
    #[serde(default)]
    pricing: Pricing,
}

#[derive(Deserialize, Default)]
struct Pricing {
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    completion: String,
}

impl Provider for OpenRouter {
    fn id(&self) -> &'static str {
        "openrouter"
    }

    fn supports_models_array(&self) -> bool {
        true
    }

    fn scan<'a>(
        &'a self,
        client: &'a reqwest::Client,
        cfg: &'a ProviderConfig,
    ) -> BoxFuture<'a, Result<Vec<CatalogEntry>, ProviderError>> {
        Box::pin(async move {
            let endpoint = if cfg.discovery.models_endpoint.is_empty() {
                "/models".to_string()
            } else {
                cfg.discovery.models_endpoint.clone()
            };
            let url = format!("{}{}", cfg.base_url.trim_end_matches('/'), endpoint);
            let resp = crate::authorized_get(client, &url, cfg).send().await?;
            let status = resp.status();
            let body = resp.bytes().await?;
            if !status.is_success() {
                return Err(ProviderError::Msg(format!(
                    "openrouter models: status {status}: {}",
                    String::from_utf8_lossy(&body)
                )));
            }
            let parsed: ModelsResponse = serde_json::from_slice(&body)
                .map_err(|e| ProviderError::Parse(format!("openrouter models: {e}")))?;
            let raw: Vec<RawModel> = parsed
                .data
                .into_iter()
                .filter(|m| {
                    (m.pricing.prompt == "0" && m.pricing.completion == "0")
                        || m.id.ends_with(":free")
                })
                .map(|m| RawModel {
                    model_id: m.id,
                    context_window: m.context_length,
                    metadata: None,
                })
                .collect();
            Ok(chat_entries("openrouter", "free", raw))
        })
    }
}
