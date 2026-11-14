//! Provider: Google AI Studio (Gemini)
//! Docs: https://ai.google.dev/gemini-api/docs/models
//! Discovery: GET /v1beta/models (different host from inference).
//! Auth: x-goog-api-key header (NOT Authorization: Bearer).
//! Thinking models emit reasoning inline as "<thought>…</thought>answer" —
//! stripped here so clients see the plain answer.

use crate::{chat_entries, Provider, ProviderError, RawModel};
use futures::future::BoxFuture;
use proxy_fire_core::catalog::CatalogEntry;
use proxy_fire_core::config::ProviderConfig;
use serde::Deserialize;

pub struct Gemini;

const DISCOVERY_BASE: &str = "https://generativelanguage.googleapis.com";

#[derive(Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    models: Vec<Model>,
    #[serde(rename = "nextPageToken", default)]
    next_page_token: String,
}

#[derive(Deserialize)]
struct Model {
    #[serde(default)]
    name: String, // "models/gemini-2.0-flash"
    #[serde(rename = "inputTokenLimit", default)]
    input_token_limit: i64,
    #[serde(rename = "supportedGenerationMethods", default)]
    supported_generation_methods: Vec<String>,
}

impl Provider for Gemini {
    fn id(&self) -> &'static str {
        "gemini"
    }

    fn post_process_response(&self, status: u16, body: Vec<u8>) -> Vec<u8> {
        if status != 200 {
            return body;
        }
        strip_gemini_thoughts(&body)
    }

    fn scan<'a>(
        &'a self,
        client: &'a reqwest::Client,
        cfg: &'a ProviderConfig,
    ) -> BoxFuture<'a, Result<Vec<CatalogEntry>, ProviderError>> {
        Box::pin(async move {
            let discovery_url = if cfg.discovery.models_endpoint.is_empty() {
                format!("{DISCOVERY_BASE}/v1beta/models")
            } else {
                cfg.discovery.models_endpoint.clone()
            };
            let api_key = cfg.resolved_api_key();

            let mut entries_raw: Vec<RawModel> = Vec::new();
            let mut page_token = String::new();
            loop {
                let url = if page_token.is_empty() {
                    discovery_url.clone()
                } else {
                    format!("{discovery_url}?pageToken={page_token}")
                };
                let mut req = client.get(&url);
                // The key goes in the header, never the URL (scan errors are logged).
                if !api_key.is_empty() {
                    req = req.header("x-goog-api-key", &api_key);
                }
                let resp = req.send().await?;
                let status = resp.status();
                let body = resp.bytes().await?;
                if !status.is_success() {
                    return Err(ProviderError::Msg(format!(
                        "gemini models: status {status}: {}",
                        String::from_utf8_lossy(&body)
                    )));
                }
                let page: ModelsResponse = serde_json::from_slice(&body)
                    .map_err(|e| ProviderError::Parse(format!("gemini models: {e}")))?;
                for m in page.models {
                    if !m
                        .supported_generation_methods
                        .iter()
                        .any(|s| s == "generateContent")
                    {
                        continue;
                    }
                    let model_id = m.name.trim_start_matches("models/").to_string();
                    if model_id.is_empty() || is_gemini_paid_only(&model_id) {
                        continue;
                    }
                    entries_raw.push(RawModel {
                        model_id,
                        context_window: m.input_token_limit,
                        metadata: None,
                    });
                }
                if page.next_page_token.is_empty() {
                    break;
                }
                page_token = page.next_page_token;
            }
            Ok(chat_entries("gemini", "free", entries_raw))
        })
    }
}

/// Paid-only / non-chat model IDs (conservative heuristic; `proxy-fire-scan probe` confirms).
fn is_gemini_paid_only(model_id: &str) -> bool {
    let lower = model_id.to_lowercase();
    ["ultra", "bison", "gecko"]
        .iter()
        .any(|s| lower.contains(s))
}

/// Remove leading <thought>…</thought> blocks from each choice's message
/// content so clients receive only the answer. Unparseable bodies pass through.
pub fn strip_gemini_thoughts(body: &[u8]) -> Vec<u8> {
    if !body.windows(9).any(|w| w == b"<thought>") {
        return body.to_vec();
    }
    let Ok(mut parsed) = serde_json::from_slice::<serde_json::Value>(body) else {
        return body.to_vec();
    };
    let Some(choices) = parsed.get_mut("choices").and_then(|c| c.as_array_mut()) else {
        return body.to_vec();
    };
    let mut changed = false;
    for choice in choices.iter_mut() {
        let Some(msg) = choice.get_mut("message").and_then(|m| m.as_object_mut()) else {
            continue;
        };
        let Some(content) = msg
            .get("content")
            .and_then(|c| c.as_str())
            .map(str::to_string)
        else {
            continue;
        };
        let mut trimmed = content.trim_start().to_string();
        while let Some(rest) = trimmed.strip_prefix("<thought>") {
            match rest.find("</thought>") {
                Some(end) => {
                    trimmed = rest[end + "</thought>".len()..].trim_start().to_string();
                }
                None => break,
            }
        }
        if trimmed != content {
            msg.insert("content".to_string(), serde_json::Value::String(trimmed));
            changed = true;
        }
    }
    if !changed {
        return body.to_vec();
    }
    serde_json::to_vec(&parsed).unwrap_or_else(|_| body.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_thought_prefix() {
        let body = br#"{"choices":[{"message":{"role":"assistant","content":"<thought>hidden reasoning</thought>The answer is 4."},"finish_reason":"stop"}]}"#;
        let out = strip_gemini_thoughts(body);
        let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            parsed["choices"][0]["message"]["content"].as_str().unwrap(),
            "The answer is 4."
        );
    }

    #[test]
    fn leaves_content_without_thought_alone() {
        let body = br#"{"choices":[{"message":{"content":"plain"}}]}"#;
        let out = strip_gemini_thoughts(body);
        assert_eq!(out, body);
    }
}
