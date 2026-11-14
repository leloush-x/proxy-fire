//! Prober: sends a minimal chat request to verify a model's free access.

use proxy_fire_core::catalog::CatalogEntry;
use proxy_fire_core::config::ProviderConfig;
use serde_json::json;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub provider_id: String,
    pub model_id: String,
    pub is_accessible: bool,
    pub status_code: i64,
    pub err_msg: String,
}

pub struct Prober {
    client: reqwest::Client,
}

impl Prober {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// Send a minimal chat completion (max_tokens=1) to verify free access.
    /// 200 = accessible. 429 = maybe rate-limited, not necessarily paid.
    pub async fn probe(&self, cfg: &ProviderConfig, entry: &CatalogEntry) -> ProbeResult {
        let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
        let mut payload = json!({
            "model": entry.model_id,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "Hi"}],
        });
        if let Some(tier) = crate::llm_refresh::probe_service_tier(cfg) {
            payload["service_tier"] = serde_json::Value::String(tier);
        }

        let mut req = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(payload.to_string())
            .timeout(Duration::from_secs(15));
        let (h, v) = cfg.resolved_auth();
        if !v.is_empty() && v != "Bearer " {
            req = req.header(&h, v);
        }
        for (k, val) in &cfg.extra_headers {
            req = req.header(k, val);
        }

        match req.send().await {
            Err(e) => ProbeResult {
                provider_id: cfg.id.clone(),
                model_id: entry.model_id.clone(),
                is_accessible: false,
                status_code: 0,
                err_msg: e.to_string(),
            },
            Ok(resp) => {
                let status = resp.status().as_u16() as i64;
                let body = resp.bytes().await.unwrap_or_default();
                ProbeResult {
                    provider_id: cfg.id.clone(),
                    model_id: entry.model_id.clone(),
                    is_accessible: status == 200,
                    status_code: status,
                    err_msg: if status == 200 {
                        String::new()
                    } else {
                        String::from_utf8_lossy(&body).chars().take(300).collect()
                    },
                }
            }
        }
    }
}

impl Default for Prober {
    fn default() -> Self {
        Self::new()
    }
}
