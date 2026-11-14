//! proxy-fire — one binary, everything for the LLM proxy API:
//! the proxy server, the free-model catalog scan, provider login, and the
//! benchmark.
//!
//!   proxy-fire                       run the proxy (binds 127.0.0.1:8080)
//!   proxy-fire serve                 same, explicit
//!   proxy-fire login                 interactive provider login (validates keys)
//!   proxy-fire scan update           rescan providers, refresh the catalog
//!   proxy-fire scan probe            re-verify flagged models
//!   proxy-fire scan refresh-llm      LLM-powered catalog diff
//!   proxy-fire scan export-md        render FREE_MODELS.md
//!   proxy-fire bench                 benchmark strategies across models

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "proxy-fire",
    version,
    about = "OpenAI-compatible proxy for every free LLM tier, in one binary"
)]
struct Cli {
    /// Config file path (default: search . then ~/.proxy-fire/)
    #[arg(short, long, global = true)]
    config: Option<String>,
    /// Override listen address (default 127.0.0.1; use 0.0.0.0 to expose)
    #[arg(long)]
    host: Option<String>,
    /// Override proxy port (default 8080)
    #[arg(short, long)]
    port: Option<u16>,
    /// Override strategy name
    #[arg(short, long)]
    strategy: Option<String>,
    /// Skip the diffused LED strip start-up animation
    #[arg(long, global = true)]
    no_anim: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the proxy server (default when no subcommand is given)
    Serve,
    /// List providers and configure API keys interactively
    Login,
    /// Free-model catalog: scan, probe, refresh, export
    Scan {
        /// catalog.json path (overrides catalog.path in config)
        #[arg(long, global = true)]
        catalog: Option<String>,
        #[command(subcommand)]
        command: proxy_fire::commands::scan::ScanCommand,
    },
    /// Benchmark strategies across free models
    Bench {
        /// Strategies to benchmark (comma-separated)
        #[arg(short, long, value_delimiter = ',')]
        strategies: Vec<String>,
        /// Write raw metrics JSON to this file
        #[arg(short, long)]
        json: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".parse().unwrap()),
        )
        .init();

    let cli = Cli::parse();
    let cfg_file = cli.config.unwrap_or_default();

    // The LED strip plays once, just before serving.
    if cli.command.is_none() && !cli.no_anim {
        proxy_fire::boot::play();
    }

    match cli.command {
        None | Some(Command::Serve) => {
            proxy_fire::commands::serve::run(proxy_fire::commands::serve::ServeArgs {
                config: cfg_file,
                host: cli.host,
                port: cli.port,
                strategy: cli.strategy,
            })
            .await
        }
        Some(Command::Login) => proxy_fire::login::run(&cfg_file).await,
        Some(Command::Scan { catalog, command }) => {
            let mut cfg = proxy_fire_core::config::load(&cfg_file)?;
            if let Some(c) = catalog {
                cfg.catalog.path = c;
            }
            proxy_fire::commands::scan::run(command, cfg, &cfg_file).await
        }
        Some(Command::Bench { strategies, json }) => {
            proxy_fire::commands::bench::run_cmd(&cfg_file, strategies, json).await
        }
    }
}
