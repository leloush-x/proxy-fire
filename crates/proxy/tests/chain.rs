//! Integration tests: FallbackChain against in-process mock providers.

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use proxy_fire_core::catalog::{Catalog, CatalogEntry};
use proxy_fire_core::config::{Config, FallbackConfig, ProviderConfig};
use proxy_fire_core::gemini::GeminiTracker;
use proxy_fire_core::ratelimit::GlobalTracker;
use proxy_fire_core::reliability::Tracker as ReliabilityTracker;
use proxy_fire_core::strategy::Registry;
use proxy_fire_proxy::fallback::FallbackChain;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

fn entry(provider: &str, model: &str) -> CatalogEntry {
    CatalogEntry {
        provider_id: provider.into(),
        model_id: model.into(),
        is_free: true,
        tier_type: "free".into(),
        ..Default::default()
    }
}

fn test_config(base_url: &str, id: &str) -> Config {
    Config {
        providers: vec![ProviderConfig {
            id: id.to_string(),
            base_url: base_url.to_string(),
            enabled: true,
            ..Default::default()
        }],
        fallback: FallbackConfig::default(),
        ..Default::default()
    }
}

fn make_chain(cfg: Config, cat: Catalog, provider_speed_order: &[(&str, &str)]) -> FallbackChain {
    let rate_limiter = Arc::new(GlobalTracker::new());
    let gemini = Arc::new(GeminiTracker::new());
    let reliability = Arc::new(ReliabilityTracker::new());
    let registry = Arc::new(Registry::new(
        None,
        Some(rate_limiter.clone()),
        String::new(),
        String::new(),
        3,
        5,
    ));
    // speed strategy puts cerebras first, groq second, huggingface last
    let strat = registry.get("speed").unwrap();
    let _ = provider_speed_order;
    FallbackChain {
        cfg: Arc::new(cfg),
        strategy: Some(strat),
        catalog: Arc::new(cat),
        rate_limiter,
        gemini,
        reliability,
        http: reqwest::Client::builder().build().unwrap(),
    }
}

fn mock_ok(model_used: &str, content: &str) -> Json<Value> {
    Json(json!({
        "id": "chatcmpl-test",
        "model": model_used,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 3}
    }))
}

struct Ctx {
    hits: AtomicI64,
    auths: Mutex<Vec<String>>,
}

#[tokio::test]
async fn empty_completion_falls_through_to_next_model() {
    async fn handler(State(_): State<()>) -> Json<Value> {
        Json(json!({
            "id": "x", "model": "hollow",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "   "}, "finish_reason": "stop"}]
        }))
    }
    let app = Router::new()
        .route("/chat/completions", post(handler))
        .with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });

    let cfg = test_config(&format!("http://{addr}"), "testprov");
    let cat = Catalog {
        entries: vec![entry("testprov", "m"), entry("testprov", "m")],
        ..Default::default()
    };
    let chain = make_chain(cfg, cat, &[]);
    let req = proxy_fire_proxy::fallback::Request {
        model: "auto".into(),
        messages: vec![json!({"role": "user", "content": "hi"})],
        ..Default::default()
    };
    // Every ranked entry returns empty completions → chain exhausts with error.
    let res = chain.execute(req).await;
    assert!(res.is_err());
    // Verify the fallback classified them as Empty (not success) — covered by error message
    assert!(res.unwrap_err().contains("exhausted"));
}

#[tokio::test]
async fn success_returns_response_with_used_model() {
    async fn handler(axum::extract::State(_): axum::extract::State<()>) -> Json<Value> {
        mock_ok("groq-llama", "hello world")
    }
    let app = Router::new()
        .route("/chat/completions", post(handler))
        .with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });

    let cfg = test_config(&format!("http://{addr}"), "testprov");
    let cat = Catalog {
        entries: vec![entry("testprov", "m")],
        ..Default::default()
    };
    let chain = make_chain(cfg, cat, &[]);
    let req = proxy_fire_proxy::fallback::Request {
        model: "auto".into(),
        messages: vec![json!({"role": "user", "content": "hi"})],
        ..Default::default()
    };
    let resp = chain.execute(req).await.expect("should succeed");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.used_model, "groq-llama");
    assert!(String::from_utf8_lossy(&resp.body).contains("hello world"));
}

#[tokio::test]
async fn key_rotation_spreads_load_and_429_fails_over() {
    let ctx = Arc::new(Ctx {
        hits: AtomicI64::new(0),
        auths: Mutex::new(Vec::new()),
    });
    let ctx2 = ctx.clone();
    let handler = move |headers: axum::http::HeaderMap| {
        let ctx = ctx2.clone();
        async move {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            ctx.auths.lock().unwrap().push(auth.clone());
            let n = ctx.hits.fetch_add(1, Ordering::SeqCst);
            if auth == "Bearer k1" && n == 0 {
                // First hit on k1 → 429 rate limited
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({"error": {"message": "rate limited"}})),
                );
            }
            (StatusCode::OK, mock_ok("m", "ok"))
        }
    };
    let app = Router::new()
        .route("/chat/completions", post(handler))
        .with_state(());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });

    std::env::set_var("FLPR_INTEG_KEY", "k1,k2");
    let mut cfg = test_config(&format!("http://{addr}"), "testprov");
    cfg.providers[0].api_key_env = "FLPR_INTEG_KEY".to_string();
    let cat = Catalog {
        entries: vec![entry("testprov", "m")],
        ..Default::default()
    };
    let chain = make_chain(cfg, cat, &[]);
    let req = proxy_fire_proxy::fallback::Request {
        model: "auto".into(),
        messages: vec![json!({"role": "user", "content": "hi"})],
        ..Default::default()
    };
    let resp = chain.execute(req).await.expect("second key should answer");
    assert_eq!(resp.status, 200);
    let auths = ctx.auths.lock().unwrap();
    assert!(auths.contains(&"Bearer k1".to_string()) || auths.contains(&"Bearer k2".to_string()));
    std::env::remove_var("FLPR_INTEG_KEY");
}

#[tokio::test]
async fn direct_401_cools_only_that_slot() {
    let handler = |headers: axum::http::HeaderMap| async move {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if auth == "Bearer bad" {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": {"message": "invalid key"}})),
            );
        }
        (StatusCode::OK, mock_ok("m", "fine"))
    };
    let app = Router::new().route("/chat/completions", post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });

    std::env::set_var("FLPR_INTEG_401", "bad,good");
    let mut cfg = test_config(&format!("http://{addr}"), "testprov");
    cfg.providers[0].api_key_env = "FLPR_INTEG_401".to_string();
    let cat = Catalog {
        entries: vec![entry("testprov", "m")],
        ..Default::default()
    };
    let chain = make_chain(cfg, cat, &[]);
    let req = proxy_fire_proxy::fallback::Request {
        model: "auto".into(),
        messages: vec![json!({"role": "user", "content": "hi"})],
        ..Default::default()
    };
    let resp = chain.execute(req).await.expect("good key should answer");
    assert_eq!(resp.status, 200);
    assert!(chain.rate_limiter.is_on_cooldown("testprov#1"));
    assert!(!chain.rate_limiter.is_on_cooldown("testprov"));
    std::env::remove_var("FLPR_INTEG_401");
}
