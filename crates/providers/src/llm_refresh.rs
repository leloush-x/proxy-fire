//! LLM-powered catalog refresh: ask a model to diff the catalog against live
//! provider information. The LLM returns a diff, never a full replacement.

use proxy_fire_core::catalog::{Catalog, CatalogEntry};
use proxy_fire_core::config::{ProviderConfig, RefreshConfig};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RefreshDiff {
    /// Entries whose free status changed.
    #[serde(default)]
    pub changed: Vec<CatalogEntry>,
    /// Newly discovered free models.
    #[serde(default)]
    pub added: Vec<CatalogEntry>,
    /// Models the LLM thinks may no longer be free — flagged, NOT deleted.
    #[serde(default)]
    pub removed_confidence: Vec<RemovedEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemovedEntry {
    pub provider_id: String,
    pub model_id: String,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default)]
    pub reason: String,
}

/// Apply a diff conservatively: update changed, add new, flag removals.
pub fn merge_diff(cat: &mut Catalog, diff: RefreshDiff) {
    for changed in diff.changed {
        match cat
            .entries
            .iter()
            .position(|e| e.provider_id == changed.provider_id && e.model_id == changed.model_id)
        {
            Some(i) => cat.entries[i] = changed,
            None => cat.entries.push(changed),
        }
    }
    for mut added in diff.added {
        if cat.find(&added.provider_id, &added.model_id).is_none() {
            added.discovered_at = Some(chrono::Utc::now());
            cat.entries.push(added);
        }
    }
    for rc in diff.removed_confidence {
        cat.mark_needs_reverification(&rc.provider_id, &rc.model_id);
    }
}

pub fn catalog_summary(cat: &Catalog) -> String {
    let mut out = format!("Current catalog has {} entries:\n", cat.entries.len());
    for e in &cat.entries {
        out.push_str(&format!(
            "- {}/{} (free={}, tier={})\n",
            e.provider_id, e.model_id, e.is_free, e.tier_type
        ));
    }
    out
}

/// Call the configured LLM with the refresh prompt and parse the diff.
pub async fn refresh(
    client: &reqwest::Client,
    refresh_cfg: &RefreshConfig,
    providers: &[ProviderConfig],
    cat: &Catalog,
) -> Result<RefreshDiff, String> {
    let provider = providers
        .iter()
        .find(|p| p.id == refresh_cfg.provider)
        .ok_or_else(|| {
            format!(
                "refresh provider {:?} not found in config",
                refresh_cfg.provider
            )
        })?;

    let template = std::fs::read_to_string(&refresh_cfg.prompt_file)
        .map_err(|e| format!("reading prompt file {}: {e}", refresh_cfg.prompt_file))?;
    let prompt = template.replace("{{CATALOG_SUMMARY}}", &catalog_summary(cat));

    let content = call_llm(client, provider, &refresh_cfg.model, &prompt).await?;
    parse_diff(&content)
}

async fn call_llm(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    model: &str,
    prompt: &str,
) -> Result<String, String> {
    let url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );
    // Strip "provider/" prefix if present.
    let model = model.rsplit('/').next().unwrap_or(model);

    let payload = json!({
        "model": model,
        "max_tokens": 4096,
        "messages": [{"role": "user", "content": prompt}],
    });

    let mut req = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(payload.to_string());
    let (h, v) = provider.resolved_auth();
    if !v.is_empty() && v != "Bearer " {
        req = req.header(&h, v);
    }
    for (k, val) in &provider.extra_headers {
        req = req.header(k, val);
    }

    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.bytes().await.map_err(|e| e.to_string())?;
    if status != reqwest::StatusCode::OK {
        return Err(format!(
            "LLM call failed: status {status}: {}",
            String::from_utf8_lossy(&body)
        ));
    }

    #[derive(Deserialize)]
    struct LlmResp {
        choices: Vec<Choice>,
    }
    #[derive(Deserialize)]
    struct Choice {
        message: Msg,
    }
    #[derive(Deserialize)]
    struct Msg {
        content: String,
    }
    let parsed: LlmResp =
        serde_json::from_slice(&body).map_err(|e| format!("parsing LLM response: {e}"))?;
    parsed
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .ok_or_else(|| "no choices in LLM response".to_string())
}

/// Extract a RefreshDiff from an LLM response. The LLM is expected to return
/// JSON, but we scan for the first '{' … last '}'.
pub fn parse_diff(content: &str) -> Result<RefreshDiff, String> {
    let start = content.find('{').ok_or("no JSON found in LLM response")?;
    let end = content.rfind('}').ok_or("no JSON found in LLM response")?;
    if end < start {
        return Err("no JSON found in LLM response".to_string());
    }
    serde_json::from_str(&content[start..=end]).map_err(|e| format!("parsing diff JSON: {e}"))
}

/// Convenience for the scan CLI: service tier header passthrough for the probe.
pub fn probe_service_tier(cfg: &ProviderConfig) -> Option<String> {
    if cfg.native_features.service_tier.is_empty() {
        None
    } else {
        Some(cfg.native_features.service_tier.clone())
    }
}

/// Group a diff by change kind for the summary printed by the CLI.
pub fn diff_summary(diff: &RefreshDiff) -> HashMap<&'static str, usize> {
    HashMap::from([
        ("changed", diff.changed.len()),
        ("added", diff.added.len()),
        ("removed_confidence", diff.removed_confidence.len()),
    ])
}
