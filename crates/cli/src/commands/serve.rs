//! `proxy-fire serve` (default) — run the OpenAI-compatible proxy.

use crate::{load_model_data, provider_readiness, rescan_catalog, save_state};
use anyhow::{Context, Result};
use proxy_fire_core::catalog::{self, Catalog};
use proxy_fire_core::config::{load as load_config, Config};
use proxy_fire_core::gemini::GeminiTracker;
use proxy_fire_core::paths::{catalog_read_path, data_dir, expand_home};
use proxy_fire_core::ratelimit::GlobalTracker;
use proxy_fire_core::reliability::{self, Tracker as ReliabilityTracker};
use proxy_fire_core::strategy::Registry;
use proxy_fire_core::watch;
use proxy_fire_proxy::Server;
use std::sync::Arc;
use std::time::Duration;

pub struct ServeArgs {
    pub config: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub strategy: Option<String>,
}

pub async fn run(args: ServeArgs) -> Result<()> {
    let mut cfg = load_config(&args.config).context("loading config")?;
    if let Some(h) = args.host {
        cfg.proxy.host = h;
    }
    if let Some(p) = args.port {
        cfg.proxy.port = p;
    }
    if let Some(s) = args.strategy {
        cfg.proxy.strategy = s;
    }
    run_inner(cfg, args.config).await
}

async fn run_inner(cfg: Config, cfg_file: String) -> Result<()> {
    // Load catalog (bundled catalog.json until the first scan writes catalog.path)
    let cat_path_str = cfg.catalog.path.clone();
    let cat_read = catalog_read_path(&cat_path_str, &cfg_file);
    let cat = Catalog::load(&cat_read)
        .with_context(|| format!("loading catalog {}", cat_read.display()))?;
    let free_count = cat.free_entries().len();
    tracing::info!(
        "catalog: {free_count} free model(s) loaded from {}",
        cat_read.display()
    );
    if free_count == 0 {
        tracing::warn!("catalog is empty — run 'proxy-fire scan update' to discover free models");
    }

    let (ready, no_key) = provider_readiness(&cfg);
    tracing::info!("providers ready: {}", ready.join(", "));
    if !no_key.is_empty() {
        tracing::info!("providers skipped (no API key): {}", no_key.join(", "));
    }
    if ready.is_empty() {
        tracing::warn!("no provider has an API key — add one to .env (see .env.example)");
    }

    if catalog::is_stale(
        &cat_path_str,
        Duration::from_secs(cfg.catalog.max_age_hours * 3600),
    ) {
        tracing::warn!(
            "catalog is older than {}h — run 'proxy-fire scan update'",
            cfg.catalog.max_age_hours
        );
    }

    load_model_data(&cfg_file);

    // Rate limiters + reliability tracker, restored from disk
    let rate_limiter = Arc::new(GlobalTracker::new());
    let gemini = Arc::new(GeminiTracker::new());
    let usage_path = data_dir().join("usage.json").to_string_lossy().into_owned();
    match proxy_fire_core::ratelimit::load_usage(&usage_path) {
        Ok(usage) => {
            rate_limiter.restore(&usage);
            if let Some(g) = &usage.gemini {
                gemini.restore(g);
            }
        }
        Err(e) => tracing::warn!("usage.json unreadable (starting fresh): {e}"),
    }
    let reliability = Arc::new(ReliabilityTracker::new());
    let reliability_path = data_dir()
        .join("reliability.json")
        .to_string_lossy()
        .into_owned();
    if let Err(e) = reliability::load(&reliability, &reliability_path) {
        tracing::warn!("reliability.json not found (starting fresh): {e}");
    }

    // Strategy registry
    let similar_family = cfg
        .proxy
        .similar
        .get("model_family")
        .cloned()
        .unwrap_or_default();
    let gemini_preferred = cfg
        .providers
        .iter()
        .find(|p| p.id == "gemini")
        .map(|p| p.preferred_volume_model.clone())
        .unwrap_or_default();
    let fan_out = cfg
        .proxy
        .strategy_overrides
        .get("parallel")
        .and_then(|o| o.get("fan_out"))
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(3);
    let registry = Arc::new(Registry::new(
        Some(reliability.clone()),
        Some(rate_limiter.clone()),
        similar_family,
        gemini_preferred,
        fan_out,
        5,
    ));

    let cfg = Arc::new(cfg);
    let cat = Arc::new(cat);
    let state = Server::build(
        cfg.clone(),
        cat.clone(),
        registry,
        rate_limiter.clone(),
        gemini.clone(),
        reliability.clone(),
    );

    // Hot-reload the catalog
    let state_for_catalog = state.clone();
    let _catalog_watch = watch::watch_file(
        &expand_home(&cat_path_str),
        Duration::from_millis(300),
        move |path| match Catalog::load(&path) {
            Ok(new_cat) => {
                let n = new_cat.free_entries().len();
                *state_for_catalog.catalog.write().unwrap() = Arc::new(new_cat);
                tracing::info!("catalog: hot-reloaded ({n} models)");
            }
            Err(e) => tracing::warn!("catalog reload error: {e}"),
        },
    )
    .map(Some)
    .unwrap_or_else(|e| {
        tracing::warn!("catalog watch error (hot-reload disabled): {e}");
        None
    });

    // Catalog git-sync (scanner or replica role)
    {
        let cfg_gs = (*cfg).clone();
        let cat_path_gs = cat_path_str.clone();
        tokio::spawn(async move {
            git_sync_loop(&cfg_gs, &cat_path_gs).await;
        });
    }

    // Hot-reload the config
    let state_for_config = state.clone();
    let _config_watch = if cfg_file.is_empty() {
        None
    } else {
        match watch::watch_file(
            &expand_home(&cfg_file),
            Duration::from_millis(200),
            move |path| match load_config(&path.to_string_lossy()) {
                Ok(new_cfg) => {
                    *state_for_config.cfg.write().unwrap() = Arc::new(new_cfg);
                    tracing::info!("proxy: config hot-reloaded");
                }
                Err(e) => tracing::warn!("config reload error: {e}"),
            },
        ) {
            Ok(w) => Some(w),
            Err(e) => {
                tracing::warn!("config watch error (hot-reload disabled): {e}");
                None
            }
        }
    };

    // Persist state periodically and on shutdown
    let usage_path_save = usage_path.clone();
    let reliability_path_save = reliability_path.clone();
    let state_for_save = state.clone();
    let save_fn = move || {
        save_state(
            &state_for_save.rate_limiter,
            &state_for_save.gemini,
            &state_for_save.reliability,
            &usage_path_save,
            &reliability_path_save,
        )
    };
    {
        let save = save_fn.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(300));
            interval.tick().await; // skip the immediate tick
            loop {
                interval.tick().await;
                save();
            }
        });
    }

    // Total kill on SIGINT / SIGTSTP / SIGTERM — no suspension, no zombie.
    // State is saved and the process exits.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    {
        let save = save_fn.clone();
        tokio::spawn(async move {
            // Unix: SIGINT / SIGTERM / SIGTSTP all kill totally (no suspension).
            #[cfg(unix)]
            {
                let mut sigint =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                        .expect("SIGINT handler");
                let mut sigterm =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("SIGTERM handler");
                let mut sigtstp = tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::from_raw(libc::SIGTSTP),
                )
                .expect("SIGTSTP handler");
                tokio::select! {
                    _ = sigint.recv() => {},
                    _ = sigterm.recv() => {},
                    _ = sigtstp.recv() => {},
                }
            }
            #[cfg(not(unix))]
            {
                tokio::signal::ctrl_c().await.expect("ctrl_c handler");
            }
            tracing::info!("proxy: killed (signal) — saving state and exiting");
            save();
            let _ = shutdown_tx.send(());
        });
    }

    Server::serve(state, async {
        let _ = shutdown_rx.await;
    })
    .await
    .context("server error")
}

/// Scanner role: rescan every scan_interval, then push. Replica: pull remote_url periodically.
async fn git_sync_loop(cfg: &proxy_fire_core::config::Config, cat_path: &str) {
    use proxy_fire_core::config::parse_duration;
    use proxy_fire_core::gitsync::{
        self, decide_role, git_commit_push, write_remote_catalog, SyncRole,
    };

    let gs = &cfg.catalog.git_sync;
    let Some(role) = decide_role(gs) else {
        if gs.enabled {
            tracing::warn!(
                "gitsync: enabled but no valid role configured — set scan_interval+auto_push (scanner) or remote_url+pull_interval (replica)"
            );
        }
        return;
    };

    match role {
        SyncRole::Scanner => {
            let interval =
                parse_duration(&gs.scan_interval).unwrap_or(Duration::from_secs(7 * 24 * 3600));
            let first = interval.saturating_sub(gitsync::catalog_age(cat_path));
            tracing::info!(
                "gitsync: scanner role — next scan in {:?}, then every {:?}",
                first,
                interval
            );
            tokio::time::sleep(first).await;
            loop {
                tracing::info!("gitsync: starting scheduled scan...");
                match rescan_catalog(cfg, cat_path, true).await {
                    Ok(cat) => {
                        if let Err(e) = cat.save(&expand_home(cat_path)) {
                            tracing::error!("gitsync: saving catalog: {e}");
                        } else if let Err(e) = git_commit_push(cat_path, gs) {
                            tracing::error!("gitsync: push failed: {e}");
                        }
                    }
                    Err(e) => tracing::error!("gitsync: scan failed: {e}"),
                }
                tokio::time::sleep(interval).await;
            }
        }
        SyncRole::Replica => {
            let interval =
                parse_duration(&gs.pull_interval).unwrap_or(Duration::from_secs(7 * 24 * 3600));
            let first = interval.saturating_sub(gitsync::catalog_age(cat_path));
            tracing::info!(
                "gitsync: replica role — next pull in {:?}, then every {:?}",
                first,
                interval
            );
            tokio::time::sleep(first).await;
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .expect("replica client");
            loop {
                match client.get(&gs.remote_url).send().await {
                    Ok(resp) if resp.status() == reqwest::StatusCode::OK => {
                        if let Ok(bytes) = resp.bytes().await {
                            match write_remote_catalog(&bytes, cat_path) {
                                Ok(n) => tracing::info!(
                                    "gitsync: pulled {n} models from {}",
                                    gs.remote_url
                                ),
                                Err(e) => tracing::error!("gitsync: pull error: {e}"),
                            }
                        }
                    }
                    Ok(resp) => tracing::error!("gitsync: pull error: status {}", resp.status()),
                    Err(e) => tracing::error!("gitsync: pull error: {e}"),
                }
                tokio::time::sleep(interval).await;
            }
        }
    }
}
