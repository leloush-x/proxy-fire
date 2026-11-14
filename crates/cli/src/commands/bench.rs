//! `proxy-fire bench` — benchmark strategies across free models.

use crate::bench::{generate_report, run, write_human};
use anyhow::{Context, Result};
use proxy_fire_core::catalog::Catalog;
use proxy_fire_core::paths::catalog_read_path;
use proxy_fire_core::strategy::Registry;

pub async fn run_cmd(
    cfg_file: &str,
    strategies: Vec<String>,
    json_out: Option<String>,
) -> Result<()> {
    let cfg = proxy_fire_core::config::load(cfg_file).context("loading config")?;

    let cat_read = catalog_read_path(&cfg.catalog.path, cfg_file);
    let cat = Catalog::load(&cat_read)
        .with_context(|| format!("loading catalog {}", cat_read.display()))?;
    if cat.free_entries().is_empty() {
        anyhow::bail!("catalog is empty — run 'proxy-fire scan update' first");
    }

    let registry = Registry::new(None, None, String::new(), String::new(), 3, 5);
    let strategy_names: Vec<String> = if strategies.is_empty() {
        vec![
            "adaptive".into(),
            "speed".into(),
            "balanced".into(),
            "coding".into(),
        ]
    } else {
        strategies
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?;
    let metrics = run(&client, &cfg, &cat, &registry, &strategy_names).await;
    let report = generate_report(metrics);

    if let Some(p) = json_out {
        std::fs::write(&p, serde_json::to_vec_pretty(&report)?).context("writing JSON report")?;
        tracing::info!("wrote raw metrics to {p}");
    }
    write_human(&report, std::io::stdout());
    Ok(())
}
