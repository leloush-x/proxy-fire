//! FallbackChain — the single path every upstream chat call goes through.
//!
//! Order:
//!  1. OpenRouter with models[] (native intra-provider fallback)
//!  2. remaining providers in strategy order
//!  3. openrouter/free (ultimate fallback, even past max_attempts)
//!
//! Special cases:
//!  - HuggingFace 503+loading: wait estimated_time, retry the SAME provider
//!  - Cerebras: pre-emptive slowdown when remaining-requests < 2
//!  - key pooling: round-robin slot rotation, 429 → next key, 401/403 → slot cooldown
//!  - Gemini: 4-dimensional quota reservation
//!  - 200 with no usable content is a failure, not an answer

use proxy_fire_core::catalog::{is_chat_model, Catalog, CatalogEntry};
use proxy_fire_core::config::{Config, ProviderConfig};
use proxy_fire_core::gemini::GeminiTracker;
use proxy_fire_core::headers::extract_rate_limit_info;
use proxy_fire_core::ratelimit::GlobalTracker;
use proxy_fire_core::reliability::Tracker as ReliabilityTracker;
use proxy_fire_core::strategy::{RankedEntry, Strategy, StrategyRequest};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

pub const MAX_MODELS_PER_PROVIDER: usize = 3;
pub const ZERO_QUOTA_COOLDOWN: Duration = Duration::from_secs(24 * 3600);
pub const MISSING_MODEL_COOLDOWN: Duration = Duration::from_secs(3600);
pub const BAD_REQUEST_COOLDOWN: Duration = Duration::from_secs(3600);
pub const AUTH_KEY_COOLDOWN: Duration = Duration::from_secs(10 * 60);
pub const PROVIDER_AUTH_COOLDOWN: Duration = Duration::from_secs(10 * 60);
pub const MAX_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// Cooldown key for one model on one key slot.
pub fn cooldown_key(provider_id: &str, model_id: &str, slot: usize, num_keys: usize) -> String {
    if num_keys <= 1 {
        format!("{provider_id}/{model_id}")
    } else {
        format!("{provider_id}/{model_id}#{}", slot + 1)
    }
}

/// Cooldown key for one API key slot of a provider, model-independent.
pub fn slot_cooldown_key(provider_id: &str, slot: usize, num_slots: usize) -> String {
    if num_slots <= 1 {
        return provider_id.to_string();
    }
    format!("{provider_id}#{}", slot + 1)
}

#[derive(Debug, Clone, Default)]
pub struct Request {
    pub model: String,
    pub messages: Vec<Value>,
    pub stream: bool,
    pub max_tokens: Option<i64>,
    pub raw: Value, // object
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub body: Vec<u8>,
    pub used_model: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    SkipModel,
    SkipProvider,
    Success,
    Empty,
    ModelFail,
    ProviderFail,
    KeyFail,
}

#[derive(Debug)]
pub(crate) struct AttemptResult {
    pub(crate) resp: Option<Response>,
    pub(crate) outcome: Outcome,
    pub(crate) calls: u32,
    #[allow(dead_code)]
    pub(crate) err: Option<String>,
}

pub struct FallbackChain {
    pub cfg: Arc<Config>,
    pub strategy: Option<Arc<dyn Strategy>>,
    pub catalog: Arc<Catalog>,
    pub rate_limiter: Arc<GlobalTracker>,
    pub gemini: Arc<GeminiTracker>,
    pub reliability: Arc<ReliabilityTracker>,
    pub http: reqwest::Client,
}

impl FallbackChain {
    pub async fn execute(&self, req: Request) -> Result<Response, String> {
        let timeout = self
            .cfg
            .fallback
            .request_timeout_duration()
            .unwrap_or(Duration::from_secs(90));
        match tokio::time::timeout(timeout, self.execute_inner(req)).await {
            Ok(r) => r,
            Err(_) => Err("request deadline exceeded".to_string()),
        }
    }

    async fn execute_inner(&self, req: Request) -> Result<Response, String> {
        let free_entries: Vec<CatalogEntry> =
            self.catalog.free_entries().into_iter().cloned().collect();
        let est = estimate_tokens(&req.messages);
        let strat_req = StrategyRequest {
            messages: req.messages.clone(),
            estimated_prompt_tokens: est,
            max_tokens: req.max_tokens.unwrap_or(0),
            stream: req.stream,
            model: req.model.clone(),
        };
        let strategy = self
            .strategy
            .clone()
            .ok_or_else(|| "no strategy configured".to_string())?;
        let refs: Vec<&CatalogEntry> = free_entries.iter().collect();
        let ranked = strategy.rank(&strat_req, &refs, None);

        let max_attempts = if self.cfg.fallback.max_attempts == 0 {
            5
        } else {
            self.cfg.fallback.max_attempts as usize
        };

        let mut attempts = 0usize;
        let mut open_router_tried = false;
        let mut failed_providers: HashSet<String> = HashSet::new();
        let mut provider_tries: HashMap<String, usize> = HashMap::new();
        let mut last_truncated: Option<Response> = None;

        for r in &ranked {
            if attempts >= max_attempts {
                break;
            }
            if failed_providers.contains(&r.provider_id) {
                continue;
            }
            let mut model_id = r.model_id.clone();
            let mut body;
            if r.provider_id == "openrouter" {
                if open_router_tried {
                    continue;
                }
                open_router_tried = true;
                let or_models = self.available_models("openrouter", &ranked);
                if or_models.is_empty() {
                    continue;
                }
                model_id = or_models[0].clone();
                body = self.build_body("openrouter", &req.raw, &model_id);
                body["models"] = serde_json::json!(or_models);
            } else {
                body = self.build_body(&r.provider_id, &req.raw, &model_id);
            }

            let res = self.attempt(&r.provider_id, &model_id, body, est).await;
            attempts += res.calls as usize;
            if let Some(err) = res.err {
                return Err(err); // deadline or client gone
            }
            match res.outcome {
                Outcome::Success => return Ok(res.resp.expect("success has a response")),
                Outcome::SkipModel => continue,
                Outcome::SkipProvider | Outcome::ProviderFail | Outcome::KeyFail => {
                    failed_providers.insert(r.provider_id.clone());
                    continue;
                }
                Outcome::Empty => {
                    if let Some(resp) = &res.resp {
                        if is_empty_truncated(&resp.body) {
                            last_truncated = Some(Response {
                                status: resp.status,
                                headers: resp.headers.clone(),
                                body: resp.body.clone(),
                                used_model: resp.used_model.clone(),
                                truncated: resp.truncated,
                            });
                        }
                    }
                }
                Outcome::ModelFail => {}
            }
            *provider_tries.entry(r.provider_id.clone()).or_insert(0) += 1;
            if r.provider_id == "openrouter"
                || provider_tries[&r.provider_id] >= MAX_MODELS_PER_PROVIDER
            {
                failed_providers.insert(r.provider_id.clone());
            }
        }

        // Ultimate fallback: openrouter/free (even past max_attempts).
        if self.find_provider("openrouter").is_some() && !failed_providers.contains("openrouter") {
            tracing::info!("fallback: trying openrouter/free ultimate fallback");
            let body = self.build_body("openrouter", &req.raw, "openrouter/free");
            let body = open_router_free_router_request(body);
            let res = self
                .attempt("openrouter", "openrouter/free", body, est)
                .await;
            if let Some(err) = res.err {
                return Err(err);
            }
            if res.outcome == Outcome::Success {
                return Ok(res.resp.expect("success has a response"));
            }
        }

        if let Some(mut t) = last_truncated {
            t.truncated = true;
            return Ok(t);
        }
        Err(format!("all providers exhausted after {attempts} attempts"))
    }

    /// Ranked models of a provider not on cooldown.
    fn available_models(&self, provider_id: &str, ranked: &[RankedEntry]) -> Vec<String> {
        ranked
            .iter()
            .filter(|r| r.provider_id == provider_id)
            .map(|r| r.model_id.clone())
            .filter(|m| {
                !self
                    .rate_limiter
                    .is_on_cooldown(&format!("{provider_id}/{m}"))
            })
            .collect()
    }

    fn find_provider(&self, id: &str) -> Option<&ProviderConfig> {
        find_provider_cfg(&self.cfg, id)
    }

    /// Single path every upstream call goes through.
    pub(crate) async fn attempt(
        &self,
        provider_id: &str,
        model_id: &str,
        body: Value,
        est_tokens: i64,
    ) -> AttemptResult {
        let Some(provider_cfg) = self.find_provider(provider_id).cloned() else {
            return AttemptResult {
                resp: None,
                outcome: Outcome::SkipProvider,
                calls: 0,
                err: None,
            };
        };
        if self.rate_limiter.is_on_cooldown(provider_id) {
            tracing::info!("fallback: {provider_id} on cooldown — skipping provider");
            return AttemptResult {
                resp: None,
                outcome: Outcome::SkipProvider,
                calls: 0,
                err: None,
            };
        }
        if self
            .rate_limiter
            .is_on_cooldown(&format!("{provider_id}/{model_id}"))
        {
            tracing::info!("fallback: {provider_id}/{model_id} on cooldown — skipping model");
            return AttemptResult {
                resp: None,
                outcome: Outcome::SkipModel,
                calls: 0,
                err: None,
            };
        }

        let keys = provider_cfg.all_keys();
        let num_slots = if keys.is_empty() { 1 } else { keys.len() };
        let start_slot = self
            .rate_limiter
            .provider(provider_id)
            .rotate_slot(num_slots);

        let mut calls = 0u32;
        let mut last = AttemptResult {
            resp: None,
            outcome: Outcome::SkipModel,
            calls: 0,
            err: None,
        };
        let mut tried_slot = false;
        for i in 0..num_slots {
            let slot = (start_slot + i) % num_slots;
            if keys.len() > 1
                && self.rate_limiter.is_on_cooldown(&slot_cooldown_key(
                    provider_id,
                    slot,
                    keys.len(),
                ))
            {
                tracing::info!(
                    "fallback: {provider_id} key slot {} cooling down (auth) — skipping",
                    slot + 1
                );
                continue;
            }
            if self.rate_limiter.is_on_cooldown(&cooldown_key(
                provider_id,
                model_id,
                slot,
                keys.len(),
            )) {
                continue;
            }
            tried_slot = true;
            let mut call_cfg = provider_cfg.clone();
            if !keys.is_empty() {
                call_cfg.api_key = keys[slot].clone();
                call_cfg.api_key_env.clear();
            }
            let res = self
                .call_once(
                    call_cfg,
                    provider_id,
                    model_id,
                    &body,
                    est_tokens,
                    slot,
                    keys.len().max(1),
                )
                .await;
            calls += res.calls;
            let mut res = res;
            res.calls = calls;

            if res.outcome == Outcome::KeyFail {
                self.rate_limiter.set_cooldown(
                    &slot_cooldown_key(provider_id, slot, keys.len()),
                    chrono::Utc::now() + chrono::Duration::from_std(AUTH_KEY_COOLDOWN).unwrap(),
                );
                tracing::info!(
                    "fallback: {provider_id} key slot {} rejected (auth) — key cooldown {:?}",
                    slot + 1,
                    AUTH_KEY_COOLDOWN
                );
                last = res;
                continue;
            }
            if res.outcome != Outcome::ModelFail || res.resp.as_ref().map(|r| r.status) != Some(429)
            {
                return res;
            }
            // 429: cool down only this key slot; try the next key.
            self.cooldown_429(
                provider_id,
                model_id,
                slot,
                keys.len(),
                res.resp.as_ref().unwrap(),
            );
            if keys.len() > 1 {
                tracing::info!(
                    "fallback: {provider_id}/{model_id} rate limited on key slot {} — trying next key",
                    slot + 1
                );
            }
            last = res;
        }
        if !tried_slot {
            if keys.len() > 1 {
                let all_auth = (0..keys.len()).all(|i| {
                    self.rate_limiter
                        .is_on_cooldown(&slot_cooldown_key(provider_id, i, keys.len()))
                });
                if all_auth {
                    tracing::info!("fallback: {provider_id} — every key slot cooling down (auth), skipping provider");
                    return AttemptResult {
                        resp: None,
                        outcome: Outcome::SkipProvider,
                        calls,
                        err: None,
                    };
                }
            }
            tracing::info!(
                "fallback: {provider_id}/{model_id} rate limited on every key — skipping model"
            );
            return AttemptResult {
                resp: None,
                outcome: Outcome::SkipModel,
                calls,
                err: None,
            };
        }
        last
    }

    fn cooldown_429(
        &self,
        provider_id: &str,
        model_id: &str,
        slot: usize,
        num_keys: usize,
        resp: &Response,
    ) {
        let info = extract_rate_limit_info(provider_id, &headers_to_map(&resp.headers), &resp.body);
        let mut wait = info.wait_duration(chrono::Utc::now());
        if is_zero_quota(&resp.body) {
            wait = chrono::Duration::from_std(ZERO_QUOTA_COOLDOWN).unwrap();
        }
        self.rate_limiter.set_cooldown(
            &cooldown_key(provider_id, model_id, slot, num_keys),
            chrono::Utc::now() + wait,
        );
        tracing::info!(
            "fallback: {provider_id}/{model_id} rate limited — cooldown {}s",
            wait.num_seconds()
        );
    }

    // 8 args mirror the Go call structure; bundling them would obscure the chain.
    #[allow(clippy::too_many_arguments)]
    async fn call_once(
        &self,
        call_cfg: ProviderConfig,
        provider_id: &str,
        model_id: &str,
        body: &Value,
        est_tokens: i64,
        slot: usize,
        num_slots: usize,
    ) -> AttemptResult {
        let _ = (slot, num_slots);
        // Cerebras: pre-emptive spacing and slowdown.
        if provider_id == "cerebras" {
            let tracker = self.rate_limiter.provider("cerebras");
            if !tracker.can_request(model_id, 0, 0) {
                tracing::info!("fallback: cerebras pre-emptive skip (rate limited)");
                return AttemptResult {
                    resp: None,
                    outcome: Outcome::SkipProvider,
                    calls: 0,
                    err: None,
                };
            }
            let spacing = self.cfg.fallback.cerebras_request_spacing_ms;
            if spacing > 0 {
                let wait = tracker.next_slot(Duration::from_millis(spacing));
                if wait > Duration::ZERO {
                    tokio::time::sleep(wait).await;
                }
            }
        }

        // Gemini: check and count in one step. Reservation releases on drop
        // unless we keep() it (model answered).
        let mut reservation = None;
        if provider_id == "gemini" {
            match self.gemini.reserve(model_id, est_tokens) {
                Some(r) => reservation = Some(r),
                None => {
                    tracing::info!("fallback: gemini {model_id} over local quota — skipping model");
                    return AttemptResult {
                        resp: None,
                        outcome: Outcome::SkipModel,
                        calls: 0,
                        err: None,
                    };
                }
            }
        }

        let mut calls = 1u32;
        let mut resp = self.call_provider(&call_cfg, body).await;
        // HuggingFace cold start: wait, retry the same model once.
        if let Ok(r) = &resp {
            if let Some(wait) =
                proxy_fire_providers::get(provider_id).cold_start_wait(r.status, &r.body)
            {
                tracing::info!(
                    "fallback: {provider_id} model loading, waiting {}ms",
                    wait.as_millis()
                );
                tokio::time::sleep(wait).await;
                resp = self.call_provider(&call_cfg, body).await;
                calls = 2;
            }
        }

        let (status, headers, body_bytes) = match resp {
            Ok(r) => (r.status, r.headers, r.body),
            Err(e) => {
                // Transport failure: provider-level, no response to classify.
                tracing::info!(
                    "fallback: {provider_id}/{model_id} transport error — skipping provider ({e})"
                );
                self.reliability.record(provider_id, false);
                if self.reliability.should_cooldown(provider_id) {
                    self.rate_limiter.set_cooldown(
                        provider_id,
                        chrono::Utc::now() + chrono::Duration::seconds(30),
                    );
                    tracing::info!(
                        "fallback: {provider_id} reliability below 50% — short cooldown"
                    );
                }
                return AttemptResult {
                    resp: None,
                    outcome: Outcome::ProviderFail,
                    calls,
                    err: None,
                };
            }
        };

        let used_model_str = used_model(&body_bytes).unwrap_or_default();
        if status == 200 {
            if let Some(r) = reservation.take() {
                r.keep();
            }
            let record_model = if provider_id == "openrouter" && !model_id.eq("openrouter/free") {
                if used_model_str.is_empty() {
                    model_id.to_string()
                } else {
                    used_model_str.clone()
                }
            } else {
                model_id.to_string()
            };
            self.rate_limiter
                .provider(provider_id)
                .record_request(&record_model);
            if provider_id == "cerebras" {
                let info =
                    extract_rate_limit_info(provider_id, &headers_to_map(&headers), &body_bytes);
                if info.remaining_requests >= 0 && info.remaining_requests < 2 {
                    self.rate_limiter.provider("cerebras").preemptive_slowdown(
                        info.wait_duration(chrono::Utc::now())
                            .to_std()
                            .unwrap_or(Duration::from_secs(60)),
                    );
                }
            }
            if is_empty_completion(&body_bytes) {
                tracing::info!("fallback: {provider_id}/{model_id} returned an empty completion — trying next model");
                return AttemptResult {
                    resp: Some(Response {
                        status,
                        headers,
                        body: body_bytes,
                        used_model: used_model_str,
                        truncated: false,
                    }),
                    outcome: Outcome::Empty,
                    calls,
                    err: None,
                };
            }
            self.reliability.record(provider_id, true);
            return AttemptResult {
                resp: Some(Response {
                    status,
                    headers,
                    body: body_bytes,
                    used_model: used_model_str,
                    truncated: false,
                }),
                outcome: Outcome::Success,
                calls,
                err: None,
            };
        }

        let resp = Response {
            status,
            headers,
            body: body_bytes,
            used_model: String::new(),
            truncated: false,
        };
        match status {
            401 | 403 => {
                self.reliability.record(provider_id, false);
                if num_slots > 1 {
                    // Key-specific failure — caller cools the slot.
                    return AttemptResult {
                        resp: Some(resp),
                        outcome: Outcome::KeyFail,
                        calls,
                        err: None,
                    };
                }
                self.rate_limiter.set_cooldown(
                    provider_id,
                    chrono::Utc::now()
                        + chrono::Duration::from_std(PROVIDER_AUTH_COOLDOWN).unwrap(),
                );
                tracing::info!("fallback: {provider_id} auth error {status} — cooldown 10min");
                if self.reliability.should_cooldown(provider_id) {
                    self.rate_limiter.set_cooldown(
                        provider_id,
                        chrono::Utc::now() + chrono::Duration::seconds(30),
                    );
                    tracing::info!(
                        "fallback: {provider_id} reliability below 50% — short cooldown"
                    );
                }
                AttemptResult {
                    resp: Some(resp),
                    outcome: Outcome::ProviderFail,
                    calls,
                    err: None,
                }
            }
            400 | 413 | 422 => {
                if status == 400 {
                    self.rate_limiter.set_cooldown(
                        &format!("{provider_id}/{model_id}"),
                        chrono::Utc::now()
                            + chrono::Duration::from_std(BAD_REQUEST_COOLDOWN).unwrap(),
                    );
                }
                tracing::info!(
                    "fallback: {provider_id}/{model_id} status {status} — provider rejects this request, skipping provider{}",
                    upstream_err_snippet(Some(&resp), None)
                );
                AttemptResult {
                    resp: Some(resp),
                    outcome: Outcome::ProviderFail,
                    calls,
                    err: None,
                }
            }
            404 => {
                self.rate_limiter.set_cooldown(
                    &format!("{provider_id}/{model_id}"),
                    chrono::Utc::now()
                        + chrono::Duration::from_std(MISSING_MODEL_COOLDOWN).unwrap(),
                );
                tracing::info!(
                    "fallback: {provider_id} model {model_id} not found — model cooldown {}s",
                    MISSING_MODEL_COOLDOWN.as_secs()
                );
                AttemptResult {
                    resp: Some(resp),
                    outcome: Outcome::ModelFail,
                    calls,
                    err: None,
                }
            }
            _ => {
                if status != 429 {
                    tracing::info!(
                        "fallback: {provider_id}/{model_id} status {status} — trying next model{}",
                        upstream_err_snippet(Some(&resp), None)
                    );
                }
                AttemptResult {
                    resp: Some(resp),
                    outcome: Outcome::ModelFail,
                    calls,
                    err: None,
                }
            }
        }
    }

    /// Cline upstream: refresh-token slot → short-lived access token, always
    /// stream upstream, reassemble SSE into a chat.completion body so the
    /// rest of the chain sees a normal buffered response.
    async fn call_cline(&self, cfg: &ProviderConfig, body: &Value) -> Result<Response, String> {
        let refresh = cfg.resolved_api_key();
        let refresh = refresh.trim();
        if refresh.is_empty() {
            return Err("cline: missing refresh token (run `proxy-fire login`)".to_string());
        }
        let access = proxy_fire_providers::cline::ensure_access_token(&self.http, refresh).await?;
        let url = chat_endpoint_of(cfg);
        let session = body
            .get("session_id")
            .and_then(|s| s.as_str())
            .unwrap_or("sess_0")
            .to_string();
        let data = serde_json::to_vec(body).map_err(|e| format!("encoding request: {e}"))?;
        let mut req = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Authorization", access) // already carries the workos: prefix
            .header("X-Task-ID", &session)
            .body(data);
        if let Some(d) = proxy_fire_core::config::parse_duration(&cfg.timeout) {
            req = req.timeout(d);
        }
        let resp = req.send().await.map_err(|e| format!("cline: {e}"))?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let raw = resp
            .bytes()
            .await
            .map_err(|e| format!("reading response: {e}"))?
            .to_vec();
        if status != 200 {
            let used = used_model(&raw).unwrap_or_default();
            return Ok(Response {
                status,
                headers,
                body: raw,
                used_model: used,
                truncated: false,
            });
        }
        // Aggregate the SSE stream into a chat.completion body.
        match proxy_fire_providers::cline::aggregate_stream(&raw) {
            Ok(json_bytes) => {
                let used = used_model(&json_bytes).unwrap_or_default();
                Ok(Response {
                    status,
                    headers,
                    body: json_bytes,
                    used_model: used,
                    truncated: false,
                })
            }
            Err(e) => {
                // Upstream answered 200 with a non-SSE body (already JSON) — pass it through.
                if serde_json::from_slice::<Value>(&raw).is_ok() {
                    let used = used_model(&raw).unwrap_or_default();
                    return Ok(Response {
                        status,
                        headers,
                        body: raw,
                        used_model: used,
                        truncated: false,
                    });
                }
                Err(format!("cline: aggregating upstream stream failed: {e}"))
            }
        }
    }

    async fn call_provider(&self, cfg: &ProviderConfig, body: &Value) -> Result<Response, String> {
        if cfg.id == "cline" {
            return self.call_cline(cfg, body).await;
        }
        let url = chat_endpoint_of(cfg);
        let data = serde_json::to_vec(body).map_err(|e| format!("encoding request: {e}"))?;

        let mut req = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .body(data);
        if let Some(d) = proxy_fire_core::config::parse_duration(&cfg.timeout) {
            req = req.timeout(d);
        }
        let (h, v) = cfg.resolved_auth();
        if !v.is_empty() && v != "Bearer " {
            req = req.header(&h, v);
        }
        for (k, val) in &cfg.extra_headers {
            req = req.header(k, val);
        }

        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body_bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("reading response: {e}"))?
            .to_vec();

        let body_bytes = if cfg.id == "gemini" {
            proxy_fire_providers::get("gemini").post_process_response(status, body_bytes)
        } else {
            body_bytes
        };
        let used = used_model(&body_bytes).unwrap_or_default();

        Ok(Response {
            status,
            headers,
            body: body_bytes,
            used_model: used,
            truncated: false,
        })
    }

    pub(crate) fn build_body(&self, provider_id: &str, raw: &Value, model_id: &str) -> Value {
        let mut body = raw.clone();
        if let Some(obj) = body.as_object_mut() {
            obj.insert("model".to_string(), Value::String(model_id.to_string()));
            obj.remove("stream");
            for f in ["promptcachekey", "prompt_cache_key", "thinking", "betas"] {
                obj.remove(f);
            }
            normalize_max_tokens(obj);
            // Per-strategy max_tokens_cap
            if let Some(strategy) = &self.strategy {
                if let Some(overrides) = self.cfg.proxy.strategy_overrides.get(strategy.name()) {
                    if let Some(cap) = overrides.get("max_tokens_cap").and_then(to_i64) {
                        if cap > 0 {
                            let current = obj.get("max_tokens").and_then(to_i64).unwrap_or(0);
                            if current == 0 || current > cap {
                                obj.insert("max_tokens".to_string(), serde_json::json!(cap));
                            }
                        }
                    }
                }
            }
            // Provider-native service tier (Groq), only when explicitly configured.
            if let Some(pc) = self.find_provider(provider_id) {
                if !pc.native_features.service_tier.is_empty() {
                    obj.insert(
                        "service_tier".to_string(),
                        Value::String(pc.native_features.service_tier.clone()),
                    );
                }
            }
            // Cline: session correlation + reasoning effort defaults + always
            // stream upstream (aggregated back to JSON in call_cline).
            if provider_id == "cline" {
                let session = obj
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(proxy_fire_providers::cline::new_session_id);
                obj.insert("session_id".to_string(), Value::String(session));
                if obj.get("reasoning_effort").is_none() {
                    obj.insert("reasoning_effort".to_string(), Value::String("high".to_string()));
                }
                obj.insert("stream".to_string(), Value::Bool(true));
            }
        }
        body
    }
}

pub fn chat_endpoint_of(cfg: &ProviderConfig) -> String {
    format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'))
}

pub fn find_provider_cfg<'a>(cfg: &'a Config, id: &str) -> Option<&'a ProviderConfig> {
    cfg.providers
        .iter()
        .find(|p| p.id == id && p.enabled && !p.missing_key())
}

fn headers_to_map(h: &reqwest::header::HeaderMap) -> HashMap<String, String> {
    h.iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|v| (k.as_str().to_string(), v.to_string()))
        })
        .collect()
}

fn used_model(body: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct R {
        model: Option<String>,
    }
    serde_json::from_slice::<R>(body).ok().and_then(|r| r.model)
}

/// Build the body for OpenRouter's ultimate free router.
pub fn open_router_free_router_request(mut body: Value) -> Value {
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "model".to_string(),
            Value::String("openrouter/free".to_string()),
        );
        obj.remove("models");
        obj.remove("stream");
    }
    body
}

/// Strip stream/turn the body for the models[] array (OpenRouter).
pub fn build_openrouter_request(req: &Value, free_models: &[String]) -> Value {
    let mut body = req.clone();
    if let Some(obj) = body.as_object_mut() {
        if let Some(first) = free_models.first() {
            obj.insert("model".to_string(), Value::String(first.clone()));
        }
        obj.insert("models".to_string(), serde_json::json!(free_models));
        obj.remove("stream");
    }
    body
}

/// Collapse the two OpenAI max-token spellings into "max_tokens".
/// When both are set, max_tokens wins.
pub fn normalize_max_tokens(obj: &mut Map<String, Value>) {
    let v = match obj.remove("max_completion_tokens") {
        Some(v) => v,
        None => return,
    };
    if !obj.contains_key("max_tokens") {
        obj.insert("max_tokens".to_string(), v);
    }
}

/// JSON (float) or YAML (int) number → i64.
pub fn to_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .or_else(|| n.as_u64().map(|u| u as i64)),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Rough token count: 4 chars ≈ 1 token.
pub fn estimate_tokens(msgs: &[Value]) -> i64 {
    let mut total = 0i64;
    for m in msgs {
        if let Some(content) = m.get("content").and_then(|c| c.as_str()) {
            total += (content.len() / 4) as i64;
        }
    }
    total
}

/// 200 body with no usable output (empty content, no tool calls).
pub fn is_empty_completion(body: &[u8]) -> bool {
    let Ok(c) = serde_json::from_slice::<Value>(body) else {
        return true;
    };
    let choices = match c.get("choices").and_then(|v| v.as_array()) {
        Some(c) => c,
        None => return true,
    };
    for ch in choices {
        let msg = ch.get("message");
        if let Some(msg) = msg {
            if msg
                .get("tool_calls")
                .and_then(|t| t.as_array())
                .map(|t| !t.is_empty())
                .unwrap_or(false)
            {
                return false;
            }
            if msg
                .get("function_call")
                .map(|f| !f.is_null())
                .unwrap_or(false)
            {
                return false;
            }
            let content = msg.get("content");
            if has_content(content) {
                return false;
            }
            // reasoning-only output does not count as usable
        }
        if ch
            .get("text")
            .and_then(|t| t.as_str())
            .map(|t| !t.trim().is_empty())
            .unwrap_or(false)
        {
            return false;
        }
    }
    true
}

fn has_content(v: Option<&Value>) -> bool {
    match v {
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Array(parts)) => parts.iter().any(|p| {
            if let Some(text) = p.get("text").and_then(|t| t.as_str()) {
                !text.trim().is_empty()
            } else {
                p.as_object().map(|o| !o.is_empty()).unwrap_or(false)
            }
        }),
        _ => false,
    }
}

/// Empty completion solely because finish_reason == "length".
pub fn is_empty_truncated(body: &[u8]) -> bool {
    #[derive(serde::Deserialize)]
    struct R {
        choices: Vec<Choice>,
    }
    #[derive(serde::Deserialize)]
    struct Choice {
        finish_reason: Option<String>,
        message: Message,
    }
    #[derive(serde::Deserialize)]
    struct Message {
        content: Option<String>,
        tool_calls: Option<Vec<Value>>,
    }
    let Ok(r) = serde_json::from_slice::<R>(body) else {
        return false;
    };
    let Some(c) = r.choices.first() else {
        return false;
    };
    let empty = c
        .message
        .content
        .as_deref()
        .map(|s| s.trim().is_empty())
        .unwrap_or(true);
    c.finish_reason.as_deref() == Some("length")
        && empty
        && c.message
            .tool_calls
            .as_ref()
            .map(|t| t.is_empty())
            .unwrap_or(true)
}

/// 429 body saying this model has zero free-tier quota ("limit: 0,").
pub fn is_zero_quota(body: &[u8]) -> bool {
    String::from_utf8_lossy(body).contains("limit: 0,")
}

fn upstream_err_snippet(resp: Option<&Response>, err: Option<&str>) -> String {
    if let Some(e) = err {
        return format!(" ({e})");
    }
    let Some(resp) = resp else {
        return String::new();
    };
    if resp.body.is_empty() {
        return String::new();
    }
    let msg: String = String::from_utf8_lossy(&resp.body)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if msg.len() > 200 {
        format!(" ({}…)", &msg[..200])
    } else {
        format!(" ({msg})")
    }
}

/// Report whether a model ID is a chat model (re-export for the server).
pub fn is_chat(model_id: &str) -> bool {
    is_chat_model(model_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_completion_detection() {
        assert!(is_empty_completion(b"not json"));
        assert!(is_empty_completion(br#"{"choices":[]}"#));
        assert!(is_empty_completion(
            br#"{"choices":[{"message":{"content":"  "}}]}"#
        ));
        assert!(is_empty_completion(
            br#"{"choices":[{"message":{"content":null,"reasoning":"thinking"}}]}"#
        ));
        assert!(!is_empty_completion(
            br#"{"choices":[{"message":{"content":"hi"}}]}"#
        ));
        assert!(!is_empty_completion(
            br#"{"choices":[{"message":{"content":null,"tool_calls":[{}]}}]}"#
        ));
        assert!(!is_empty_completion(br#"{"choices":[{"text":"legacy"}]}"#));
        assert!(!is_empty_completion(
            br#"{"choices":[{"message":{"content":[{"type":"text","text":"x"}]}}]}"#
        ));
    }

    #[test]
    fn empty_truncated_detection() {
        assert!(is_empty_truncated(
            br#"{"choices":[{"finish_reason":"length","message":{"content":"","tool_calls":[]}}]}"#
        ));
        assert!(!is_empty_truncated(
            br#"{"choices":[{"finish_reason":"stop","message":{"content":""}}]}"#
        ));
    }

    #[test]
    fn zero_quota_detection() {
        assert!(is_zero_quota(b"resource has limit: 0, remaining 0"));
        assert!(!is_zero_quota(b"limit: 15, remaining 0"));
    }

    #[test]
    fn normalize_max_tokens_collapses() {
        let mut map = Map::new();
        map.insert("max_completion_tokens".to_string(), serde_json::json!(128));
        normalize_max_tokens(&mut map);
        assert_eq!(map.get("max_tokens").and_then(|v| v.as_i64()), Some(128));
        assert!(map.get("max_completion_tokens").is_none());

        // max_tokens wins when both are set
        let mut map = Map::new();
        map.insert("max_completion_tokens".to_string(), serde_json::json!(128));
        map.insert("max_tokens".to_string(), serde_json::json!(64));
        normalize_max_tokens(&mut map);
        assert_eq!(map.get("max_tokens").and_then(|v| v.as_i64()), Some(64));
    }

    #[test]
    fn cooldown_keys_differ_per_slot() {
        assert_eq!(cooldown_key("groq", "llama", 0, 1), "groq/llama");
        assert_eq!(cooldown_key("groq", "llama", 0, 3), "groq/llama#1");
        assert_eq!(cooldown_key("groq", "llama", 2, 3), "groq/llama#3");
        assert_eq!(slot_cooldown_key("groq", 0, 1), "groq");
        assert_eq!(slot_cooldown_key("groq", 0, 3), "groq#1");
        assert_eq!(slot_cooldown_key("groq", 1, 3), "groq#2");
    }

    #[test]
    fn openrouter_free_router_request_shape() {
        let body =
            serde_json::json!({"model": "x", "models": ["a"], "stream": true, "messages": []});
        let out = open_router_free_router_request(body);
        assert_eq!(out["model"], "openrouter/free");
        assert!(out.get("models").is_none());
        assert!(out.get("stream").is_none());
    }

    #[test]
    fn estimate_tokens_counts_content() {
        let msgs = vec![serde_json::json!({"role": "user", "content": "aaaaaaaa"})];
        assert_eq!(estimate_tokens(&msgs), 2);
    }

    #[test]
    fn to_i64_handles_float_yaml_and_strings() {
        assert_eq!(to_i64(&serde_json::json!(3.0)), Some(3));
        assert_eq!(to_i64(&serde_json::json!(7)), Some(7));
        assert_eq!(to_i64(&serde_json::json!("42")), Some(42));
        assert_eq!(to_i64(&serde_json::json!("nope")), None);
    }
}
