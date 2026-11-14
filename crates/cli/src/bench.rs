//! proxy-fire-bench: benchmark models through the same strategies the proxy uses.

use proxy_fire_core::catalog::Catalog;
use proxy_fire_core::config::{Config, ProviderConfig};
use proxy_fire_core::strategy::{Registry, StrategyRequest};
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Instant;

#[derive(Debug, Clone, Serialize)]
pub struct BenchMetrics {
    pub provider_id: String,
    pub model_id: String,
    pub strategy_name: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    #[serde(skip_serializing_if = "is_zero")]
    pub reasoning_tokens: i64,
    pub ttft_ms: u128,
    pub total_duration_ms: u128,
    pub success: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(skip_serializing_if = "is_zero_u16")]
    pub status_code: u16,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}
fn is_zero_u16(n: &u16) -> bool {
    *n == 0
}

impl BenchMetrics {
    pub fn tokens_per_second(&self) -> f64 {
        if self.total_duration_ms == 0 || self.completion_tokens == 0 {
            return 0.0;
        }
        self.completion_tokens as f64 / (self.total_duration_ms as f64 / 1000.0)
    }
}

pub struct BenchPrompt {
    pub name: &'static str,
    pub message: &'static str,
}

pub const DEFAULT_PROMPTS: &[BenchPrompt] = &[
    BenchPrompt {
        name: "simple_qa",
        message: "What is the capital of France?",
    },
    BenchPrompt {
        name: "math",
        message: "What is 23 * 47?",
    },
    BenchPrompt {
        name: "code",
        message: "Write a Go function that reverses a string.",
    },
    BenchPrompt {
        name: "reasoning",
        message:
            "If a train travels at 60mph for 2 hours, how far does it go? Show your reasoning.",
    },
];

pub async fn run(
    client: &reqwest::Client,
    cfg: &Config,
    cat: &Catalog,
    registry: &Registry,
    strategy_names: &[String],
) -> Vec<BenchMetrics> {
    let mut results = Vec::new();
    let free = cat.free_entries();
    for strat_name in strategy_names {
        let Ok(strat) = registry.get(strat_name) else {
            tracing::warn!("unknown strategy {strat_name}, skipping");
            continue;
        };
        tracing::info!("bench: running strategy {strat_name}");
        let strat_req = StrategyRequest {
            messages: vec![serde_json::json!({"role": "user", "content": "benchmark"})],
            max_tokens: 256,
            ..Default::default()
        };
        let ranked = strat.rank(&strat_req, &free, None);
        if ranked.is_empty() {
            tracing::warn!("bench: no models for strategy {strat_name}");
            continue;
        }
        for r in ranked.iter().take(3) {
            let Some(prov_cfg) = find_provider(cfg, &r.provider_id) else {
                continue;
            };
            for prompt in DEFAULT_PROMPTS {
                results.push(bench_one(client, &prov_cfg, &r.model_id, strat_name, prompt).await);
            }
        }
    }
    results
}

fn find_provider(cfg: &Config, id: &str) -> Option<ProviderConfig> {
    cfg.providers
        .iter()
        .find(|p| p.id == id && p.enabled)
        .cloned()
}

async fn bench_one(
    client: &reqwest::Client,
    cfg: &ProviderConfig,
    model_id: &str,
    strat_name: &str,
    prompt: &BenchPrompt,
) -> BenchMetrics {
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let mut payload = serde_json::json!({
        "model": model_id,
        "max_tokens": 256,
        "messages": [{"role": "user", "content": prompt.message}],
    });
    if !cfg.native_features.service_tier.is_empty() {
        payload["service_tier"] =
            serde_json::Value::String(cfg.native_features.service_tier.clone());
    }

    let mut req = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(payload.to_string());
    let (h, v) = cfg.resolved_auth();
    if !v.is_empty() && v != "Bearer " {
        req = req.header(&h, v);
    }
    for (k, val) in &cfg.extra_headers {
        req = req.header(k, val);
    }

    let start = Instant::now();
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            return BenchMetrics {
                provider_id: cfg.id.clone(),
                model_id: model_id.to_string(),
                strategy_name: strat_name.to_string(),
                prompt_tokens: 0,
                completion_tokens: 0,
                reasoning_tokens: 0,
                ttft_ms: start.elapsed().as_millis(),
                total_duration_ms: start.elapsed().as_millis(),
                success: false,
                error: e.to_string(),
                status_code: 0,
            };
        }
    };
    let ttft = start.elapsed();
    let status = resp.status().as_u16();
    let body = resp.bytes().await.unwrap_or_default();
    let total = start.elapsed();

    let mut m = BenchMetrics {
        provider_id: cfg.id.clone(),
        model_id: model_id.to_string(),
        strategy_name: strat_name.to_string(),
        prompt_tokens: 0,
        completion_tokens: 0,
        reasoning_tokens: 0,
        ttft_ms: ttft.as_millis(),
        total_duration_ms: total.as_millis(),
        success: status == 200,
        error: if status == 200 {
            String::new()
        } else {
            format!(
                "status {status}: {}",
                String::from_utf8_lossy(&body)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )
        },
        status_code: status,
    };
    if status == 200 {
        #[derive(serde::Deserialize)]
        struct Usage {
            prompt_tokens: Option<i64>,
            completion_tokens: Option<i64>,
        }
        #[derive(serde::Deserialize)]
        struct Resp {
            usage: Option<Usage>,
            choices: Option<Vec<Choice>>,
        }
        #[derive(serde::Deserialize)]
        struct Choice {
            message: Message,
        }
        #[derive(serde::Deserialize)]
        struct Message {
            reasoning_content: Option<String>,
        }
        if let Ok(parsed) = serde_json::from_slice::<Resp>(&body) {
            if let Some(u) = parsed.usage {
                m.prompt_tokens = u.prompt_tokens.unwrap_or(0);
                m.completion_tokens = u.completion_tokens.unwrap_or(0);
            }
            if let Some(ch) = parsed.choices.and_then(|c| c.into_iter().next()) {
                if let Some(rc) = ch.message.reasoning_content {
                    m.reasoning_tokens = (rc.len() / 4) as i64;
                }
            }
        }
    }
    m
}

#[derive(Serialize)]
pub struct Report {
    pub by_strategy: BTreeMap<String, Vec<BenchMetrics>>,
}

pub fn generate_report(metrics: Vec<BenchMetrics>) -> Report {
    let mut by_strategy: BTreeMap<String, Vec<BenchMetrics>> = BTreeMap::new();
    for m in metrics {
        by_strategy
            .entry(m.strategy_name.clone())
            .or_default()
            .push(m);
    }
    for ms in by_strategy.values_mut() {
        ms.sort_by(|a, b| {
            b.success.cmp(&a.success).then(
                b.tokens_per_second()
                    .partial_cmp(&a.tokens_per_second())
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });
    }
    Report { by_strategy }
}

pub fn write_human(report: &Report, mut out: impl std::io::Write) {
    let line = "=".repeat(70);
    let dash = "-".repeat(60);
    writeln!(out, "{line}").unwrap();
    writeln!(out, "Free LLM Proxy Router Benchmark Report").unwrap();
    writeln!(out, "{line}").unwrap();
    for (strat, metrics) in &report.by_strategy {
        let success = metrics.iter().filter(|m| m.success).count();
        writeln!(
            out,
            "\nStrategy: {strat:<20} ({success}/{} successful)",
            metrics.len()
        )
        .unwrap();
        writeln!(out, "{dash}").unwrap();
        for (i, m) in metrics.iter().enumerate() {
            let status = if m.success { "✓" } else { "✗" };
            writeln!(
                out,
                "  {}. {} {}/{}",
                i + 1,
                status,
                m.provider_id,
                m.model_id
            )
            .unwrap();
            write!(
                out,
                "     TTFT: {}ms | Total: {}ms | {:.0} tok/s",
                m.ttft_ms,
                m.total_duration_ms,
                m.tokens_per_second()
            )
            .unwrap();
            if m.reasoning_tokens > 0 {
                write!(out, " | reasoning: {} tokens", m.reasoning_tokens).unwrap();
            }
            if !m.success {
                write!(out, " | error: {}", m.error).unwrap();
            }
            writeln!(out).unwrap();
        }
    }
    writeln!(out, "{line}").unwrap();
}
