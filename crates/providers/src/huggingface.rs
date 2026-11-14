//! Provider: Hugging Face Inference API
//! Docs: https://huggingface.co/docs/api-inference/quicktour
//! Discovery: HF Hub API (NOT /v1/models) — warm, non-gated text-generation models.
//! Special: 503 + "loading" = cold start; wait estimated_time and retry once.

use crate::{chat_entries, Provider, ProviderError, RawModel};
use futures::future::BoxFuture;
use proxy_fire_core::catalog::CatalogEntry;
use proxy_fire_core::config::ProviderConfig;
use serde::Deserialize;
use std::time::Duration;

pub struct HuggingFace;

const HF_HUB_BASE: &str = "https://huggingface.co/api/models";

#[derive(Deserialize)]
struct HubModel {
    #[serde(rename = "modelId")]
    model_id: Option<String>,
    id: Option<String>,
    #[serde(default)]
    gated: bool,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(rename = "pipeline_tag", default)]
    pipeline_tag: String,
}

impl Provider for HuggingFace {
    fn id(&self) -> &'static str {
        "huggingface"
    }

    /// 503 with body containing "loading" means the model is cold-starting.
    /// Wait estimated_time (default 30s, capped at 60s), then retry once.
    fn cold_start_wait(&self, status: u16, body: &[u8]) -> Option<Duration> {
        if status != 503 {
            return None;
        }
        let text = String::from_utf8_lossy(body);
        if !text.contains("loading") {
            return None;
        }
        #[derive(Deserialize)]
        struct HfErr {
            estimated_time: Option<f64>,
        }
        let secs = serde_json::from_slice::<HfErr>(body)
            .ok()
            .and_then(|e| e.estimated_time)
            .filter(|t| *t > 0.0)
            .map(|t| (t * 1000.0) as u64)
            .unwrap_or(30_000)
            .min(60_000);
        Some(Duration::from_millis(secs))
    }

    fn scan<'a>(
        &'a self,
        client: &'a reqwest::Client,
        cfg: &'a ProviderConfig,
    ) -> BoxFuture<'a, Result<Vec<CatalogEntry>, ProviderError>> {
        Box::pin(async move {
            let filters = if cfg.discovery.hub_api_filters.is_empty() {
                "pipeline_tag=text-generation&inference=warm&gated=false&limit=50".to_string()
            } else {
                cfg.discovery.hub_api_filters.clone()
            };
            let url = format!("{HF_HUB_BASE}?{filters}");
            let mut req = client.get(&url);
            let api_key = cfg.resolved_api_key();
            if !api_key.is_empty() {
                req = req.header("Authorization", format!("Bearer {api_key}"));
            }
            let resp = req.send().await?;
            let status = resp.status();
            let body = resp.bytes().await?;
            if !status.is_success() {
                return Err(ProviderError::Msg(format!(
                    "hf hub: status {status}: {}",
                    String::from_utf8_lossy(&body)
                )));
            }
            let models: Vec<HubModel> = serde_json::from_slice(&body)
                .map_err(|e| ProviderError::Parse(format!("hf hub: {e}")))?;
            let raw: Vec<RawModel> = models
                .into_iter()
                .filter_map(|m| {
                    if m.gated {
                        return None;
                    }
                    let has_tag = m.tags.iter().any(|t| {
                        t.eq_ignore_ascii_case("text-generation")
                            || t.eq_ignore_ascii_case("conversational")
                    });
                    if !has_tag && !m.pipeline_tag.eq_ignore_ascii_case("text-generation") {
                        return None;
                    }
                    let id = m.model_id.or(m.id)?;
                    let metadata = {
                        let mut meta = serde_json::Map::new();
                        if !m.pipeline_tag.is_empty() {
                            meta.insert(
                                "pipeline_tag".to_string(),
                                serde_json::Value::String(m.pipeline_tag),
                            );
                        }
                        Some(meta)
                    };
                    Some(RawModel {
                        model_id: id,
                        context_window: 0,
                        metadata,
                    })
                })
                .collect();
            Ok(chat_entries("huggingface", "free", raw))
        })
    }
}
