//! proxy-fire-scan — free-model discovery: provider scanning, correlation, merging.

use proxy_fire_core::capabilities::detect_family;
use proxy_fire_core::catalog::{is_chat_model, Catalog, CatalogEntry};
use proxy_fire_core::config::ProviderConfig;
use std::time::Duration;

/// Scan all enabled providers, merging results into one catalog.
/// A provider whose scan fails is skipped — never wipes its entries
/// (callers use Catalog::keep_unscanned to carry them forward).
pub async fn scan_all(
    client: &reqwest::Client,
    providers: &[ProviderConfig],
) -> (Catalog, Vec<String>) {
    let mut cat = Catalog {
        version: 1,
        updated_at: chrono::Utc::now(),
        entries: Vec::new(),
        blocklist: Vec::new(),
    };
    let mut failed = Vec::new();
    for p in providers {
        if !p.enabled {
            continue;
        }
        tracing::info!("scan: scanning provider {} ...", p.id);
        let provider = proxy_fire_providers::get(&p.id);
        match tokio::time::timeout(Duration::from_secs(60), provider.scan(client, p)).await {
            Ok(Ok(entries)) => {
                let n = entries.len();
                cat.entries
                    .extend(entries.into_iter().filter(|e| is_chat_model(&e.model_id)));
                tracing::info!("scan: {} → {} entries", p.id, n);
            }
            Ok(Err(e)) => {
                tracing::warn!("scan: {} error: {} — continuing", p.id, e);
                failed.push(p.id.clone());
            }
            Err(_) => {
                tracing::warn!("scan: {} timed out — continuing", p.id);
                failed.push(p.id.clone());
            }
        }
    }
    (cat, failed)
}

/// Group entries by detected family (loose key when detection fails).
/// Models in one group share architecture and are interchangeable for routing.
pub fn correlate(entries: &[CatalogEntry]) -> Vec<(String, Vec<CatalogEntry>)> {
    let mut groups: std::collections::HashMap<String, Vec<CatalogEntry>> =
        std::collections::HashMap::new();
    for e in entries {
        let family = detect_family(&e.model_id);
        let family = if family.is_empty() {
            loose_family_key(&e.model_id)
        } else {
            family
        };
        groups.entry(family).or_default().push(e.clone());
    }
    groups.into_iter().collect()
}

/// Rough family key: strip provider prefix, version markers, common suffixes.
pub fn loose_family_key(model_id: &str) -> String {
    let mut s = model_id.to_lowercase();
    if let Some(idx) = s.rfind('/') {
        s = s[idx + 1..].to_string();
    }
    for pat in ["-instruct", "-chat", ":free", ":nitro", "-latest"] {
        s = s.replace(pat, "");
    }
    s
}
