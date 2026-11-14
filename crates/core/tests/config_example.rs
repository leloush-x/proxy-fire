//! The bundled example config must parse and contain the expected providers.
//! This is the byte-compat guarantee for existing deployments.

use proxy_fire_core::config::Config;
use std::path::Path;

#[test]
fn example_config_parses() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../configs/config.example.yaml");
    let data = std::fs::read_to_string(&path).unwrap();
    let cfg: Config = serde_yaml_ng::from_str(&data).unwrap();

    assert!(cfg.providers.len() >= 8, "expected bundled providers");
    let openrouter = cfg.providers.iter().find(|p| p.id == "openrouter").unwrap();
    assert_eq!(openrouter.api_key_env, "OPENROUTER_API_KEY");
    assert!(openrouter.native_features.enabled);
    assert_eq!(
        openrouter.native_features.free_router_model,
        "openrouter/free"
    );
    assert_eq!(
        openrouter
            .extra_headers
            .get("HTTP-Referer")
            .map(String::as_str),
        Some("https://github.com/leloush-x/proxy-fire")
    );

    let groq = cfg.providers.iter().find(|p| p.id == "groq").unwrap();
    assert_eq!(groq.timeout, "15s");

    assert_eq!(cfg.proxy.strategy, "adaptive");
    assert_eq!(cfg.proxy.port, 8080);
    assert_eq!(cfg.fallback.max_attempts, 5);
    assert_eq!(cfg.fallback.request_timeout, "90s");
    assert!(cfg.fallback.retry_on_429);

    // Agent profiles from the example config
    assert!(cfg.proxy.agents.contains_key("coding"));
    let precise = cfg.proxy.agents.get("precise").unwrap();
    assert_eq!(precise.model, "groq/openai/gpt-oss-120b");

    // Similar block round-trips
    assert_eq!(
        cfg.proxy.similar.get("model_family").map(String::as_str),
        Some("llama-3.1-70b")
    );
}
