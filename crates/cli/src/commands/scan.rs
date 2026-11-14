//! `proxy-fire scan …` — discover/probe/refresh/export the free-model catalog.

use crate::rescan_catalog;
use anyhow::{Context, Result};
use proxy_fire_core::catalog::{self, Catalog};
use proxy_fire_core::config::Config;
use proxy_fire_core::paths::{expand_home, find_data_file};
use proxy_fire_scan::correlate;

#[derive(Debug, Clone, clap::Subcommand)]
pub enum ScanCommand {
    /// Scan all enabled providers and update catalog.json
    Update {
        /// Keep previous entries for providers that returned nothing
        #[arg(long)]
        keep_unscanned: bool,
        /// Also scan providers disabled in the config (discovery is read-only)
        #[arg(long)]
        all_providers: bool,
    },
    /// Probe models flagged as needing reverification
    Probe,
    /// Use an LLM to check for free-tier changes (writes a diff into the catalog)
    RefreshLlm,
    /// Render the catalog as a Markdown list of free models
    ExportMd {
        /// Write to file instead of stdout
        #[arg(short, long)]
        output: Option<std::path::PathBuf>,
    },
}

pub async fn run(cmd: ScanCommand, cfg: Config, cfg_file: &str) -> Result<()> {
    match cmd {
        ScanCommand::Update {
            keep_unscanned,
            all_providers,
        } => update(cfg, keep_unscanned, all_providers).await,
        ScanCommand::Probe => probe(cfg).await,
        ScanCommand::RefreshLlm => refresh_llm(cfg, cfg_file).await,
        ScanCommand::ExportMd { output } => export_md(&cfg, output),
    }
}

fn check_scan_targets(providers: &[proxy_fire_core::config::ProviderConfig]) -> Result<()> {
    if providers.is_empty() {
        anyhow::bail!(
            "no providers configured: no config.yaml was found in ., \
             ~/.proxy-fire or configs/, and no bundled config.example.yaml next to the binary. \
             Run `bash scripts/setup.sh`, or pass `-c path/to/config.yaml`"
        );
    }
    if !providers.iter().any(|p| p.enabled) {
        anyhow::bail!(
            "all {} providers are disabled in the config: enable some, \
             or run `proxy-fire scan update --all-providers` (discovery is read-only)",
            providers.len()
        );
    }
    Ok(())
}

async fn update(cfg: Config, keep_unscanned: bool, all_providers: bool) -> Result<()> {
    let providers: Vec<_> = cfg
        .providers
        .iter()
        .map(|p| {
            let mut p = p.clone();
            if all_providers {
                p.enabled = true;
            }
            p
        })
        .collect();
    check_scan_targets(&providers)?;

    let mut scan_cfg = cfg.clone();
    scan_cfg.providers = providers;
    let cat = rescan_catalog(&scan_cfg, &cfg.catalog.path, keep_unscanned)
        .await
        .map_err(|e| anyhow::anyhow!("scanning providers: {e}"))?;

    let correlated = correlate(&cat.entries);
    tracing::info!(
        "scan: found {} model families across {} entries",
        correlated.len(),
        cat.entries.len()
    );

    cat.save(&expand_home(&cfg.catalog.path))
        .with_context(|| format!("saving catalog {}", cfg.catalog.path))?;
    tracing::info!(
        "scan: wrote {} entries to {}",
        cat.entries.len(),
        cfg.catalog.path
    );

    if let Err(e) =
        proxy_fire_core::gitsync::push_after_scan(&cfg.catalog.path, &cfg.catalog.git_sync)
    {
        tracing::warn!("gitsync: push failed (non-fatal): {e}");
    }
    Ok(())
}

async fn probe(cfg: Config) -> Result<()> {
    let mut cat = Catalog::load(&expand_home(&cfg.catalog.path))
        .with_context(|| format!("loading catalog {}", cfg.catalog.path))?;
    let pending: Vec<_> = catalog::pending_reverification(&cat)
        .into_iter()
        .cloned()
        .collect();
    if pending.is_empty() {
        tracing::info!("probe: no models need reverification");
        return Ok(());
    }
    tracing::info!("probe: checking {} models...", pending.len());

    let prober = proxy_fire_providers::Prober::new();
    let mut updated = 0usize;
    for e in &pending {
        let Some(prov_cfg) = cfg.providers.iter().find(|p| p.id == e.provider_id) else {
            continue;
        };
        let result = prober.probe(prov_cfg, e).await;
        let Some(entry) = cat
            .entries
            .iter_mut()
            .find(|c| c.provider_id == e.provider_id && c.model_id == e.model_id)
        else {
            continue;
        };
        if result.is_accessible {
            entry.needs_reverification = false;
            entry.last_verified_at = Some(chrono::Utc::now());
            tracing::info!("probe: {}/{} ✓ still free", e.provider_id, e.model_id);
        } else if result.status_code == 402 || result.status_code == 403 {
            entry.is_free = false;
            tracing::info!(
                "probe: {}/{} ✗ no longer free (status {})",
                e.provider_id,
                e.model_id,
                result.status_code
            );
        } else {
            tracing::info!(
                "probe: {}/{} ? status {} (keeping as free)",
                e.provider_id,
                e.model_id,
                result.status_code
            );
        }
        updated += 1;
    }
    cat.save(&expand_home(&cfg.catalog.path))
        .with_context(|| format!("saving catalog {}", cfg.catalog.path))?;
    tracing::info!("probe: updated {updated} entries");
    Ok(())
}

async fn refresh_llm(cfg: Config, cfg_file: &str) -> Result<()> {
    let cat = Catalog::load(&expand_home(&cfg.catalog.path))
        .with_context(|| format!("loading catalog {}", cfg.catalog.path))?;
    let prompt_file = if cfg.refresh.prompt_file.is_empty() {
        let found = find_data_file("refresh-prompt.txt", cfg_file);
        if found.is_file() {
            found.to_string_lossy().into_owned()
        } else {
            // Materialize the embedded prompt to a temp file
            let tmp = std::env::temp_dir().join("proxy-fire-refresh-prompt.txt");
            std::fs::create_dir_all(tmp.parent().unwrap()).ok();
            std::fs::write(&tmp, proxy_fire_core::embedded::REFRESH_PROMPT)?;
            tmp.to_string_lossy().into_owned()
        }
    } else {
        cfg.refresh.prompt_file.clone()
    };
    let mut refresh = cfg.refresh.clone();
    refresh.prompt_file = prompt_file;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let diff = proxy_fire_providers::llm_refresh::refresh(&client, &refresh, &cfg.providers, &cat)
        .await
        .map_err(|e| anyhow::anyhow!("LLM refresh: {e}"))?;

    tracing::info!(
        "refresh: changed={}, added={}, removed_confidence={}",
        diff.changed.len(),
        diff.added.len(),
        diff.removed_confidence.len()
    );

    let mut cat = cat;
    proxy_fire_providers::merge_diff(&mut cat, diff);
    cat.save(&expand_home(&cfg.catalog.path))
        .with_context(|| format!("saving catalog {}", cfg.catalog.path))?;
    tracing::info!("refresh: catalog updated at {}", cfg.catalog.path);
    Ok(())
}

fn export_md(cfg: &Config, output: Option<std::path::PathBuf>) -> Result<()> {
    let cat = Catalog::load(&expand_home(&cfg.catalog.path))
        .with_context(|| format!("loading catalog {}", cfg.catalog.path))?;
    match output {
        None => cat.write_markdown(&mut std::io::stdout())?,
        Some(p) => {
            let mut f = std::fs::File::create(&p)?;
            cat.write_markdown(&mut f)?;
        }
    }
    Ok(())
}
