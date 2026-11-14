//! proxy-fire — the all-in-one binary: OpenAI-compatible proxy plus
//! catalog maintenance and benchmarking subcommands. Everything in one binary.

pub mod bench;
pub mod boot;
pub mod commands;
pub mod login;

use proxy_fire_core::catalog::Catalog;
use proxy_fire_core::config::Config;
use proxy_fire_scan::scan_all;
use std::sync::Arc;

/// Rescan all enabled providers; carry forward the blocklist and any provider
/// whose scan failed (a flaky scan must never wipe its models fleet-wide).
pub async fn rescan_catalog(
    cfg: &Config,
    catalog_path: &str,
    keep_unscanned: bool,
) -> Result<Catalog, String> {
    let existing = Catalog::load(&proxy_fire_core::paths::expand_home(catalog_path)).ok();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;
    let (mut cat, failed_providers) = scan_all(&client, &cfg.providers).await;
    if let Some(e) = &existing {
        cat.blocklist = e.blocklist.clone();
    }
    cat.filter_blocklisted();
    if keep_unscanned {
        if let Some(e) = &existing {
            let kept = cat.keep_unscanned(e);
            if !kept.is_empty() {
                tracing::info!(
                    "scan: no results from {:?} — kept their previous entries",
                    kept
                );
            }
        }
    }
    let _ = failed_providers;
    Ok(cat)
}

/// Load families/capabilities YAML (optional; logs on failure).
pub fn load_model_data(cfg_file: &str) {
    let find = |name: &str| proxy_fire_core::paths::find_data_file(name, cfg_file);
    if let Err(e) = proxy_fire_core::capabilities::load_families(&find("families.yaml")) {
        tracing::warn!("families.yaml not found or invalid: {e} (family detection disabled)");
    }
    if let Err(e) = proxy_fire_core::capabilities::load_capabilities(&find("capabilities.yaml")) {
        tracing::warn!(
            "capabilities.yaml not found or invalid: {e} (capability detection disabled)"
        );
    }
}

/// Save usage + reliability state (called periodically and on shutdown).
pub fn save_state(
    rate_limiter: &Arc<proxy_fire_core::ratelimit::GlobalTracker>,
    gemini: &Arc<proxy_fire_core::gemini::GeminiTracker>,
    reliability: &Arc<proxy_fire_core::reliability::Tracker>,
    usage_path: &str,
    reliability_path: &str,
) {
    let mut usage = rate_limiter.snapshot();
    usage.gemini = Some(gemini.snapshot());
    if let Err(e) = proxy_fire_core::ratelimit::save_usage(&usage, usage_path) {
        tracing::error!("saving usage: {e}");
    }
    if let Err(e) = proxy_fire_core::reliability::save(reliability, reliability_path) {
        tracing::error!("saving reliability: {e}");
    }
}

/// The providers ready to serve (enabled + key present) and those skipped.
pub fn provider_readiness(cfg: &Config) -> (Vec<String>, Vec<String>) {
    let mut ready = Vec::new();
    let mut no_key = Vec::new();
    for p in &cfg.providers {
        if !p.enabled {
            continue;
        }
        if p.missing_key() {
            no_key.push(p.id.clone());
        } else {
            ready.push(p.id.clone());
        }
    }
    (ready, no_key)
}
