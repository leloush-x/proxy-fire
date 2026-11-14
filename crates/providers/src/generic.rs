//! Generic provider: standard OpenAI /models endpoint.
//! Used for: Mistral, Cerebras, Cohere, NVIDIA NIM, Together AI, DeepSeek,
//! and every other OpenAI-compatible free tier in the config.

use crate::{authorized_get, chat_entries, ModelsResponse, Provider, ProviderError, RawModel};
use futures::future::BoxFuture;
use proxy_fire_core::catalog::CatalogEntry;
use proxy_fire_core::config::ProviderConfig;

pub static DEFAULT: Generic = Generic;

pub struct Generic;

impl Provider for Generic {
    fn id(&self) -> &'static str {
        "generic"
    }

    fn scan<'a>(
        &'a self,
        client: &'a reqwest::Client,
        cfg: &'a ProviderConfig,
    ) -> BoxFuture<'a, Result<Vec<CatalogEntry>, ProviderError>> {
        Box::pin(async move { scan_openai_models(client, cfg, None).await })
    }
}

/// Standard OpenAI /models scan. `base_suffix` overrides the endpoint path
/// (default: cfg.discovery.models_endpoint or "/models").
pub async fn scan_openai_models(
    client: &reqwest::Client,
    cfg: &ProviderConfig,
    endpoint_override: Option<&str>,
) -> Result<Vec<CatalogEntry>, ProviderError> {
    let endpoint = endpoint_override
        .map(str::to_string)
        .or_else(|| {
            if cfg.discovery.models_endpoint.is_empty() {
                None
            } else {
                Some(cfg.discovery.models_endpoint.clone())
            }
        })
        .unwrap_or_else(|| "/models".to_string());
    let url = format!("{}{}", cfg.base_url.trim_end_matches('/'), endpoint);

    let resp = authorized_get(client, &url, cfg).send().await?;
    let status = resp.status();
    let body = resp.bytes().await?;
    if !status.is_success() {
        return Err(ProviderError::Msg(format!(
            "{} models: status {}: {}",
            cfg.id,
            status,
            String::from_utf8_lossy(&body)
        )));
    }
    let parsed: ModelsResponse = serde_json::from_slice(&body)
        .map_err(|e| ProviderError::Parse(format!("{} models: {e}", cfg.id)))?;

    let tier_type = if cfg.tier_type.is_empty() {
        "free".to_string()
    } else {
        cfg.tier_type.clone()
    };
    let raw: Vec<RawModel> = parsed
        .data
        .into_iter()
        .filter(|m| eligible_model(&cfg.id, &m.id, &m.r#type, &m.endpoints))
        .map(|m| RawModel {
            model_id: m.id,
            context_window: m.context_length.or(m.context_window).unwrap_or(0),
            metadata: None,
        })
        .collect();
    Ok(chat_entries(&cfg.id, &tier_type, raw))
}

/// Filter out non-chat and paid-only models.
pub fn eligible_model(
    provider_id: &str,
    model_id: &str,
    model_type: &str,
    endpoints: &[String],
) -> bool {
    let lower = model_id.to_lowercase();
    match provider_id {
        "together" => {
            if !model_type.is_empty()
                && !model_type.eq_ignore_ascii_case("chat")
                && !model_type.eq_ignore_ascii_case("language")
            {
                return false;
            }
        }
        "cohere"
            if !endpoints.is_empty()
                && !endpoints.iter().any(|e| e.eq_ignore_ascii_case("chat")) =>
        {
            return false;
        }
        _ => {}
    }
    proxy_fire_core::catalog::is_chat_model(&lower)
}
