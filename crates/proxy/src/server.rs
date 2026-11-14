//! HTTP server: routes, request handling, hot-reloadable state.

use crate::cache::ResponseCache;
use crate::errors::{openai_error, retry_after_seconds};
use crate::fallback::{normalize_max_tokens, to_i64, FallbackChain, Request};
use crate::middleware::{auth, logging, method_not_allowed, recovery};
use crate::sse::write_completion_as_sse;
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Method, StatusCode},
    middleware as mw,
    response::{IntoResponse, Response},
    routing::{any, get},
    Json, Router,
};
use proxy_fire_core::catalog::Catalog;
use proxy_fire_core::config::Config;
use proxy_fire_core::gemini::GeminiTracker;
use proxy_fire_core::ratelimit::GlobalTracker;
use proxy_fire_core::reliability::Tracker as ReliabilityTracker;
use proxy_fire_core::strategy::Registry;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

pub struct Server;

impl Server {
    /// Build shared state and routes.
    pub fn build(
        cfg: Arc<Config>,
        cat: Arc<Catalog>,
        registry: Arc<Registry>,
        rate_limiter: Arc<GlobalTracker>,
        gemini: Arc<GeminiTracker>,
        reliability: Arc<ReliabilityTracker>,
    ) -> Arc<AppState> {
        let cache_ttl = cfg.proxy.cache_ttl;
        Arc::new(AppState {
            cfg: RwLock::new(cfg),
            catalog: RwLock::new(cat),
            registry,
            rate_limiter,
            gemini,
            reliability,
            cache: ResponseCache::new(cache_ttl),
            http: reqwest::Client::builder()
                .pool_max_idle_per_host(16)
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .expect("upstream client"),
        })
    }

    /// Serve until `shutdown` resolves (graceful).
    pub async fn serve(
        state: Arc<AppState>,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let cfg = state.cfg.read().unwrap().clone();
        let state_for_auth = state.clone();
        let mux = Router::new()
            .route("/v1/chat/completions", any(chat_completions))
            .route("/v1/models", get(models))
            .route("/v1/completions", any(completions_stub))
            .route("/health", get(health))
            .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
            .layer(mw::from_fn(
                move |req: axum::extract::Request, next: mw::Next| {
                    let state = state_for_auth.clone();
                    async move { auth(state, req, next).await }
                },
            ))
            .layer(mw::from_fn(recovery))
            .layer(mw::from_fn(logging))
            .with_state(state);

        let addr: SocketAddr = format!("{}:{}", cfg.proxy.host, cfg.proxy.port)
            .parse()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        tracing::info!(
            "proxy-fire listening on {} (strategy: {})",
            addr,
            cfg.proxy.strategy
        );
        if cfg.proxy.auth_token.is_empty() && !is_loopback_host(&cfg.proxy.host) {
            tracing::warn!(
                "listening on {:?} without proxy.auth_token — anyone who can reach this port can spend your provider quotas",
                cfg.proxy.host
            );
        }

        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(
            listener,
            mux.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown)
        .await
    }
}

pub struct AppState {
    pub cfg: RwLock<Arc<Config>>,
    pub catalog: RwLock<Arc<Catalog>>,
    pub registry: Arc<Registry>,
    pub rate_limiter: Arc<GlobalTracker>,
    pub gemini: Arc<GeminiTracker>,
    pub reliability: Arc<ReliabilityTracker>,
    pub cache: ResponseCache,
    pub http: reqwest::Client,
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>()
        .map(|a| a.is_loopback())
        .unwrap_or(false)
}

async fn health() -> &'static str {
    "{\"status\":\"ok\"}"
}

// ---------- /v1/models ----------

async fn models(State(state): State<Arc<AppState>>) -> Response {
    let cat = state.catalog.read().unwrap().clone();
    let cfg = state.cfg.read().unwrap().clone();

    let mut seen = HashSet::new();
    let mut out: Vec<Value> = Vec::new();
    let mut add = |id: &str, owner: &str| {
        if id.is_empty() || !seen.insert(id.to_string()) {
            return;
        }
        out.push(json!({"id": id, "object": "model", "created": 0, "owned_by": owner}));
    };

    add("auto", "proxy-fire");
    for n in state.registry.names() {
        add(&n, "proxy-fire");
    }
    let mut agents: Vec<&String> = cfg.proxy.agents.keys().collect();
    agents.sort();
    for n in agents {
        add(n, "proxy-fire");
    }
    for e in cat.free_entries() {
        add(&e.model_id, &e.provider_id);
    }

    Json(json!({"object": "list", "data": out})).into_response()
}

async fn completions_stub(method: Method) -> Response {
    if method != Method::POST {
        return method_not_allowed();
    }
    openai_error(
        StatusCode::NOT_IMPLEMENTED,
        "invalid_request_error",
        "unsupported_endpoint",
        "legacy completions are not supported; use /v1/chat/completions",
    )
}

// ---------- /v1/chat/completions ----------

const MAX_REQUEST_BODY_BYTES: usize = 16 << 20;

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    method: Method,
    body: axum::body::Bytes,
) -> Response {
    if method != Method::POST {
        return method_not_allowed();
    }
    if body.len() > MAX_REQUEST_BODY_BYTES {
        return openai_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "request_too_large",
            &format!("request body exceeds {} MB", MAX_REQUEST_BODY_BYTES >> 20),
        );
    }
    let mut raw: Value = match serde_json::from_slice(&body) {
        Ok(v @ Value::Object(_)) => v,
        Ok(_) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_json",
                "request body is not a JSON object",
            )
        }
        Err(_) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_json",
                "request body is not valid JSON",
            )
        }
    };

    // Parse the bits the router cares about.
    let mut req = Request {
        raw: raw.clone(),
        ..Default::default()
    };
    if let Some(m) = raw.get("model").and_then(|m| m.as_str()) {
        req.model = m.to_string();
    }
    if let Some(msgs) = raw.get("messages").and_then(|m| m.as_array()) {
        req.messages = msgs.clone();
    }
    if let Some(s) = raw.get("stream").and_then(|s| s.as_bool()) {
        req.stream = s;
    }
    // Normalize before agent-profile defaults run so a profile default
    // cannot override the client's max_completion_tokens.
    if let Some(obj) = raw.as_object_mut() {
        normalize_max_tokens(obj);
    }
    if let Some(mt) = raw.get("max_tokens").and_then(to_i64) {
        req.max_tokens = Some(mt);
    }
    req.raw = raw.clone();

    let cfg = state.cfg.read().unwrap().clone();
    let cat = state.catalog.read().unwrap().clone();

    // Resolve agent profile.
    if let Some(profile) = cfg.proxy.agents.get(&req.model).cloned() {
        let obj = raw.as_object_mut().expect("object");
        // Defaults — only when client did not set the key
        for (k, v) in &profile.defaults {
            obj.entry(k.clone()).or_insert_with(|| v.clone());
        }
        // Overrides — always win
        for (k, v) in &profile.overrides {
            obj.insert(k.clone(), v.clone());
        }
        normalize_max_tokens(obj);
        req.max_tokens = obj.get("max_tokens").and_then(to_i64);

        let mut pinned = profile.model.clone();
        if !pinned.is_empty() && !catalog_has_model(&cat, &pinned) {
            tracing::info!(
                "agent profile {:?}: pinned model {:?} not in free catalog — using strategy instead",
                req.model,
                pinned
            );
            pinned.clear();
        }
        if !pinned.is_empty() {
            req.model = pinned.clone();
            obj.insert("model".to_string(), Value::String(pinned));
        } else if !profile.strategy.is_empty() {
            req.model = profile.strategy.clone();
            obj.insert("model".to_string(), Value::String(profile.strategy.clone()));
        } else {
            req.model.clear();
            obj.insert("model".to_string(), Value::String(String::new()));
        }
        tracing::info!(
            "agent profile resolved: model={:?} strategy={:?}",
            profile.model,
            profile.strategy
        );
        req.raw = raw.clone();
    }

    // Response cache for non-streaming requests.
    if !req.stream {
        if let Some(cached) = state.cache.get(&raw) {
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .header("X-Cache", "HIT")
                .body(Body::from(cached))
                .unwrap();
        }
    }

    // Route: strategy name, "auto"/"", or specific model ID.
    let is_strategy =
        req.model.is_empty() || req.model == "auto" || state.registry.get(&req.model).is_ok();
    if !is_strategy {
        return serve_direct_model(state, cfg, cat, req, raw).await;
    }
    execute_strategy_chain(state, cfg, cat, req, raw).await
}

fn catalog_has_model(cat: &Catalog, model: &str) -> bool {
    let (target_provider, target_model) = split_provider_prefix(model);
    cat.free_entries()
        .iter()
        .any(|e| match_entry(e, model, target_provider, target_model))
}

fn split_provider_prefix(model: &str) -> (&str, &str) {
    match model.find('/') {
        Some(idx) => (&model[..idx], &model[idx + 1..]),
        None => ("", model),
    }
}

fn match_entry(
    e: &proxy_fire_core::catalog::CatalogEntry,
    req_model: &str,
    target_provider: &str,
    target_model: &str,
) -> bool {
    if !e.is_free {
        return false;
    }
    if e.model_id == req_model {
        return true;
    }
    if !target_provider.is_empty() && e.provider_id == target_provider && e.model_id == target_model
    {
        return true;
    }
    false
}

async fn serve_direct_model(
    state: Arc<AppState>,
    cfg: Arc<Config>,
    cat: Arc<Catalog>,
    mut req: Request,
    mut raw: Value,
) -> Response {
    let (target_provider, target_model) = split_provider_prefix(&req.model);

    let entries: Vec<proxy_fire_core::catalog::CatalogEntry> = cat
        .free_entries()
        .into_iter()
        .filter(|e| match_entry(e, &req.model, target_provider, target_model))
        .cloned()
        .collect();

    if entries.is_empty() {
        return openai_error(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            "model_not_found",
            &format!("model {:?} not found in free catalog", req.model),
        );
    }

    let est = crate::fallback::estimate_tokens(&req.messages);
    for e in &entries {
        let chain = FallbackChain {
            cfg: cfg.clone(),
            strategy: None,
            catalog: cat.clone(),
            rate_limiter: state.rate_limiter.clone(),
            gemini: state.gemini.clone(),
            reliability: state.reliability.clone(),
            http: state.http.clone(),
        };
        let body = chain.build_body(&e.provider_id, &raw, &e.model_id);
        let res = chain.attempt(&e.provider_id, &e.model_id, body, est).await;
        match res.outcome {
            crate::fallback::Outcome::Success => {
                let resp = res.resp.expect("success has response");
                return finalize_direct_response(req.stream, resp);
            }
            crate::fallback::Outcome::SkipModel | crate::fallback::Outcome::SkipProvider => {
                continue;
            }
            _ => continue,
        }
    }
    // Model found but all direct attempts failed — fall back to chain.
    tracing::info!("direct route: all providers failed — falling back to strategy chain");
    req.model.clear();
    raw["model"] = Value::String(String::new());
    execute_strategy_chain(state, cfg, cat, req, raw).await
}

fn finalize_direct_response(stream: bool, resp: crate::fallback::Response) -> Response {
    if stream && resp.status == 200 {
        let mut builder = Response::builder().status(StatusCode::OK);
        builder = with_sse_headers(builder, &resp);
        if !resp.used_model.is_empty() {
            builder = builder.header("X-Used-Model", &resp.used_model);
        }
        return builder
            .body(Body::from(write_completion_as_sse(&resp.body)))
            .unwrap();
    }
    let builder = Response::builder()
        .status(StatusCode::from_u16(resp.status).unwrap_or(StatusCode::BAD_GATEWAY));
    let builder = with_upstream_headers(builder, &resp.headers);
    builder
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(resp.body))
        .unwrap()
}

async fn execute_strategy_chain(
    state: Arc<AppState>,
    cfg: Arc<Config>,
    cat: Arc<Catalog>,
    mut req: Request,
    raw: Value,
) -> Response {
    let mut strat_name = cfg.proxy.strategy.clone();
    if !req.model.is_empty() && req.model != "auto" && state.registry.get(&req.model).is_ok() {
        strat_name = req.model.clone();
    }
    let strat = match state.registry.get(&strat_name) {
        Ok(s) => s,
        Err(_) => {
            tracing::warn!(
                "unknown strategy {:?}, falling back to adaptive",
                strat_name
            );
            state.registry.get("adaptive").expect("adaptive registered")
        }
    };

    let chain = FallbackChain {
        cfg: cfg.clone(),
        strategy: Some(strat),
        catalog: cat.clone(),
        rate_limiter: state.rate_limiter.clone(),
        gemini: state.gemini.clone(),
        reliability: state.reliability.clone(),
        http: state.http.clone(),
    };

    req.raw = raw.clone();
    match chain.execute(req).await {
        Err(e) => {
            tracing::warn!("fallback chain exhausted: {e}");
            let retry = retry_after_seconds(state.rate_limiter.earliest_recovery());
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .header("Retry-After", retry.to_string())
                .header("X-Proxy-Exhausted", "true")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "error": {
                            "message": "All free providers are rate-limited or failing. Retry after the Retry-After interval.",
                            "type": "server_error",
                            "param": null,
                            "code": "all_providers_exhausted",
                        }
                    })
                    .to_string(),
                ))
                .unwrap()
        }
        Ok(resp) => {
            let stream = raw.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
            if stream && resp.status == 200 {
                let mut builder = Response::builder().status(StatusCode::OK);
                if !resp.used_model.is_empty() {
                    builder = builder.header("X-Used-Model", &resp.used_model);
                }
                builder = with_sse_headers(builder, &resp);
                return builder
                    .body(Body::from(write_completion_as_sse(&resp.body)))
                    .unwrap();
            }
            // Cache successful non-streaming, non-truncated responses.
            if !stream && resp.status == 200 && !resp.truncated {
                state.cache.set(&raw, &resp.body);
            }
            let mut builder = Response::builder()
                .status(StatusCode::from_u16(resp.status).unwrap_or(StatusCode::BAD_GATEWAY));
            builder = with_upstream_headers(builder, &resp.headers);
            if !resp.used_model.is_empty() {
                builder = builder.header("X-Used-Model", &resp.used_model);
            }
            builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(resp.body))
                .unwrap()
        }
    }
}

// ---------- header helpers ----------

const UPSTREAM_ONLY: &[&str] = &[
    "content-length",
    "content-encoding",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "set-cookie",
    // We re-set Content-Type ourselves (JSON or SSE) — one value only,
    // unlike Go's Header().Set overwrite semantics.
    "content-type",
];

fn with_upstream_headers(
    mut builder: axum::http::response::Builder,
    src: &HeaderMap,
) -> axum::http::response::Builder {
    for (k, v) in src.iter() {
        let name = k.as_str().to_ascii_lowercase();
        if UPSTREAM_ONLY.contains(&name.as_str()) {
            continue;
        }
        builder = builder.header(k.clone(), v.clone());
    }
    builder
}

fn with_sse_headers(
    mut builder: axum::http::response::Builder,
    _resp: &crate::fallback::Response,
) -> axum::http::response::Builder {
    builder = builder
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .header("X-Accel-Buffering", "no");
    builder
}
