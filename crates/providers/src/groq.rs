//! Provider: Groq
//! Docs: https://console.groq.com/docs/models
//! Free tier: all listed models are free on the developer tier.
//! Auth: Authorization: Bearer. x-ratelimit-reset-* headers use Go durations.
//! No Retry-After; the chain uses header-derived durations instead.

use crate::{chat_entries, ModelsResponse, Provider, ProviderError, RawModel};
use futures::future::BoxFuture;
use proxy_fire_core::catalog::CatalogEntry;
use proxy_fire_core::config::ProviderConfig;

pub struct Groq;

impl Provider for Groq {
    fn id(&self) -> &'static str {
        "groq"
    }

    fn scan<'a>(
        &'a self,
        client: &'a reqwest::Client,
        cfg: &'a ProviderConfig,
    ) -> BoxFuture<'a, Result<Vec<CatalogEntry>, ProviderError>> {
        Box::pin(async move {
            let endpoint = if cfg.discovery.models_endpoint.is_empty() {
                "/openai/v1/models".to_string()
            } else {
                cfg.discovery.models_endpoint.clone()
            };
            // If the endpoint is absolute-style (/openai/v1/models), Groq's
            // models path is not under the inference base URL.
            let url = if endpoint.contains("/openai/v1") {
                format!("https://api.groq.com{endpoint}")
            } else {
                format!("{}/models", cfg.base_url.trim_end_matches('/'))
            };
            let resp = crate::authorized_get(client, &url, cfg).send().await?;
            let status = resp.status();
            let body = resp.bytes().await?;
            if !status.is_success() {
                return Err(ProviderError::Msg(format!(
                    "groq models: status {status}: {}",
                    String::from_utf8_lossy(&body)
                )));
            }
            let parsed: ModelsResponse = serde_json::from_slice(&body)
                .map_err(|e| ProviderError::Parse(format!("groq models: {e}")))?;
            let raw: Vec<RawModel> = parsed
                .data
                .into_iter()
                .map(|m| RawModel {
                    model_id: m.id,
                    context_window: m.context_length.or(m.context_window).unwrap_or(0),
                    metadata: None,
                })
                .collect();
            Ok(chat_entries("groq", "free", raw))
        })
    }
}
