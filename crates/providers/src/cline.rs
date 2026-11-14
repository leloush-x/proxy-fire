//! Cline provider — WorkOS device auth, token refresh, free-model discovery,
//! and SSE→JSON aggregation for `api.cline.bot`.
//!
//! Auth model (mirrors the official Cline CLI, see sdk/packages/core/src/auth/cline.ts):
//!   1. WorkOS device authorization → user approves in browser
//!   2. WorkOS tokens are registered at Cline's /auth/register → Cline tokens
//!   3. Requests use `Authorization: Bearer workos:<accessToken>`; the access
//!      token is short-lived and refreshed on demand via /auth/refresh.
//!
//! Refresh tokens are the pool slots (CLINE_REFRESH_TOKEN[_2.._9] in
//! ~/.proxy-fire/.env). The fallback chain rotates slots and cools 429/401
//! slots natively — no parallel pooling machinery.
//!
//! Upstream `stream: true` is always requested and aggregated back into a
//! single chat.completion JSON body: some free-tier models otherwise return
//! HTTP 200 with an empty body. Proxy-Fire's chain then sees a normal
//! non-empty completion.

use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

pub const CLINE_API_BASE: &str = "https://api.cline.bot/api/v1";
const WORKOS_CLIENT_ID: &str = "client_01K3A541FN8TA3EPPHTD2325AR";
const WORKOS_DEVICE_URL: &str = "https://api.workos.com/user_management/authorize/device";
const WORKOS_AUTHENTICATE_URL: &str = "https://api.workos.com/user_management/authenticate";
const ACCESS_PREFIX: &str = "workos:";
const REFRESH_SKEW_MS: i64 = 60_000;

/// Seed free models (from Cline's rotating free-tier promotion; see
/// https://docs.cline.bot/getting-started/free-models). Used when the
/// authenticated recommended-models sync is unavailable.
/// (id, context_window, requires_upstream_stream)
pub const SEED_FREE_MODELS: &[(&str, i64, bool)] = &[
    ("deepseek/deepseek-v4-flash", 200_000, true),
    ("poolside/laguna-s-2.1:free", 200_000, false),
    ("stepfun/step-3.7-flash", 200_000, true),
];

// ---------------------------------------------------------------------------
// Access-token cache: refresh tokens are stable; access tokens are short-lived.
// Refresh on demand with a 60s skew. In-memory only (never written to disk).
// ---------------------------------------------------------------------------

struct CachedToken {
    access: String,  // includes workos: prefix
    expires_at: i64, // unix ms
}

fn token_cache() -> &'static RwLock<std::collections::HashMap<String, CachedToken>> {
    static CACHE: OnceLock<RwLock<std::collections::HashMap<String, CachedToken>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(std::collections::HashMap::new()))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn parse_expiry_ms(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|t| t.timestamp_millis())
            .unwrap_or(0),
        _ => 0,
    }
}

#[derive(Debug, Deserialize)]
struct ClineRefreshResp {
    data: ClineTokenData,
}

#[derive(Debug, Deserialize)]
struct ClineTokenData {
    #[serde(rename = "accessToken")]
    access_token: String,
    // Cline may rotate the refresh token on refresh; the pool keeps the original.
    #[serde(default, rename = "refreshToken")]
    #[allow(dead_code)]
    refresh_token: Option<String>,
    #[serde(default, rename = "expiresAt")]
    expires_at: Option<Value>,
}

/// Exchange a refresh token for a usable access token (`workos:`-prefixed).
/// Cached in memory; refreshes at most once per token per lifetime minus skew.
pub async fn ensure_access_token(client: &Client, refresh_token: &str) -> Result<String, String> {
    let now = now_ms();
    if let Ok(map) = token_cache().read() {
        if let Some(c) = map.get(refresh_token) {
            if now < c.expires_at {
                return Ok(c.access.clone());
            }
        }
    }

    let resp = client
        .post(format!("{CLINE_API_BASE}/auth/refresh"))
        .json(&json!({"refreshToken": refresh_token, "grantType": "refresh_token"}))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("cline refresh failed: {e}"))?;
    if resp.status().as_u16() != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("cline refresh rejected: {}", tail(&body)));
    }
    let r: ClineRefreshResp = resp.json().await.map_err(|e| format!("cline refresh decode: {e}"))?;
    let access = format!("{ACCESS_PREFIX}{}", r.data.access_token);
    let expires_at = parse_expiry_ms(&r.data.expires_at.unwrap_or(Value::Null))
        .saturating_sub(REFRESH_SKEW_MS);
    // Fall back to a conservative 1h lifetime if the server omits expiry.
    let expires_at = if expires_at <= now_ms() { now_ms() + 3_600_000 } else { expires_at };

    if let Ok(mut map) = token_cache().write() {
        map.insert(
            refresh_token.to_string(),
            CachedToken {
                access: access.clone(),
                expires_at,
            },
        );
    }
    Ok(access)
}

pub fn new_session_id() -> String {
    format!("sess_{}", now_ms())
}

// ---------------------------------------------------------------------------
// WorkOS device authorization flow (interactive login)
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct DeviceAuth {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub interval_secs: u64,
    pub expires_in_secs: u64,
}

#[derive(Debug, Deserialize)]
struct DeviceAuthResp {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
    #[serde(default)]
    expires_in: Option<u64>,
}

pub async fn start_device_auth(client: &Client) -> Result<DeviceAuth, String> {
    let resp = client
        .post(WORKOS_DEVICE_URL)
        .form(&[("client_id", WORKOS_CLIENT_ID)])
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("workos device auth request: {e}"))?;
    if resp.status().as_u16() != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("workos device auth failed: {}", tail(&body)));
    }
    let d: DeviceAuthResp = resp.json().await.map_err(|e| format!("device auth decode: {e}"))?;
    Ok(DeviceAuth {
        device_code: d.device_code,
        user_code: d.user_code,
        verification_uri: d.verification_uri,
        verification_uri_complete: d.verification_uri_complete,
        interval_secs: d.interval.unwrap_or(5).max(5),
        expires_in_secs: d.expires_in.unwrap_or(300).max(30),
    })
}

#[derive(Debug)]
pub struct WorkosTokens {
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct AuthenticateResp {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// Poll WorkOS until the user approves (or the code expires).
pub async fn poll_device_token(
    client: &Client,
    device_code: &str,
    interval_secs: u64,
    expires_in_secs: u64,
) -> Result<WorkosTokens, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(expires_in_secs);
    let mut interval = interval_secs.max(5);
    loop {
        if std::time::Instant::now() >= deadline {
            return Err("device authorization expired (timed out waiting for approval)".into());
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let resp = client
            .post(WORKOS_AUTHENTICATE_URL)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code),
                ("client_id", WORKOS_CLIENT_ID),
            ])
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| format!("workos poll: {e}"))?;
        let status = resp.status().as_u16();
        let body: AuthenticateResp = resp.json().await.unwrap_or(AuthenticateResp {
            access_token: None,
            refresh_token: None,
            error: None,
            error_description: None,
        });
        if status == 200 {
            if let (Some(a), Some(r)) = (body.access_token, body.refresh_token) {
                return Ok(WorkosTokens { access_token: a, refresh_token: r });
            }
            return Err("workos returned 200 with missing tokens".into());
        }
        match body.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some(other) => {
                let desc = body.error_description.as_deref().unwrap_or(other);
                return Err(format!("workos authorization failed: {desc}"));
            }
            None => return Err(format!("workos poll failed (HTTP {status})")),
        }
    }
}

#[derive(Debug)]
pub struct ClineRegistration {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_ms: i64,
    pub email: String,
}

#[derive(Debug, Deserialize)]
struct ClineRegisterResp {
    data: ClineRegisterData,
}

#[derive(Debug, Deserialize)]
struct ClineRegisterData {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(default, rename = "expiresAt")]
    expires_at: Option<Value>,
    #[serde(default, rename = "userInfo")]
    user_info: Option<ClineUserInfo>,
}

#[derive(Debug, Deserialize)]
struct ClineUserInfo {
    #[serde(default)]
    email: Option<String>,
}

/// Register WorkOS tokens with Cline → Cline access/refresh tokens.
pub async fn register_with_cline(
    client: &Client,
    workos_access: &str,
    workos_refresh: &str,
) -> Result<ClineRegistration, String> {
    let resp = client
        .post(format!("{CLINE_API_BASE}/auth/register"))
        .json(&json!({"accessToken": workos_access, "refreshToken": workos_refresh}))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("cline register request: {e}"))?;
    if resp.status().as_u16() != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("cline register failed: {}", tail(&body)));
    }
    let r: ClineRegisterResp = resp.json().await.map_err(|e| format!("cline register decode: {e}"))?;
    let refresh = r.data.refresh_token.ok_or_else(|| "cline register response missing refresh token".to_string())?;
    let expires_at_ms = parse_expiry_ms(&r.data.expires_at.unwrap_or(Value::Null)).saturating_sub(REFRESH_SKEW_MS);
    Ok(ClineRegistration {
        access_token: format!("{ACCESS_PREFIX}{}", r.data.access_token),
        refresh_token: refresh,
        expires_at_ms,
        email: r
            .data
            .user_info
            .and_then(|u| u.email)
            .unwrap_or_else(|| "unknown".to_string()),
    })
}

/// Validate a refresh token by exchanging it for an access token.
/// Returns the account email when available.
pub async fn validate_refresh_token(client: &Client, refresh_token: &str) -> Result<String, String> {
    let access = ensure_access_token(client, refresh_token).await?;
    let _ = access; // token works
    Ok("ok".into())
}

fn tail(s: &str) -> String {
    s.chars().take(300).collect()
}

// ---------------------------------------------------------------------------
// SSE → chat.completion aggregation.
// Cline free-tier models answer `stream: true` reliably (HTTP 200 non-stream
// bodies are often EMPTY), so the Cline path always requests a stream and
// reassembles it here into the non-streaming shape Proxy-Fire buffers.
// ---------------------------------------------------------------------------

/// Reassemble an upstream SSE stream into a single chat.completion JSON body.
/// Tolerates the {"data": {...}} envelope used by Cline, [DONE] sentinels,
/// and missing usage blocks.
pub fn aggregate_stream(body: &[u8]) -> Result<Vec<u8>, String> {
    let text = String::from_utf8_lossy(body);
    let mut model = String::new();
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish_reason = String::new();
    let mut usage: Option<Value> = None;
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut cur_tool: Option<Value> = None;
    let mut cur_tool_index: i64 = -1;
    let mut cur_args = String::new();

    fn flush_tool(cur_tool: &mut Option<Value>, cur_args: &mut String, tool_calls: &mut Vec<Value>) {
        if let Some(mut t) = cur_tool.take() {
            if let Some(f) = t.get_mut("function").and_then(|f| f.as_object_mut()) {
                f.insert("arguments".into(), Value::String(std::mem::take(cur_args)));
            }
            tool_calls.push(t);
        }
    }

    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');
        let Some(payload) = line.strip_prefix("data:") else { continue };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let mut obj: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Unwrap {"data": {...}} envelope when present.
        if let Some(inner) = obj.get("data").filter(|d| d.is_object()).cloned() {
            if inner.get("choices").is_some() || inner.get("model").is_some() || inner.get("usage").is_some() {
                obj = inner;
            }
        }
        if let Some(m) = obj.get("model").and_then(|m| m.as_str()) {
            if !m.is_empty() {
                model = m.to_string();
            }
        }
        if let Some(u) = obj.get("usage") {
            if u.is_object() && !u.as_object().map(|m| m.is_empty()).unwrap_or(true) {
                usage = Some(u.clone());
            }
        }
        let Some(choices) = obj.get("choices").and_then(|c| c.as_array()) else { continue };
        let Some(choice) = choices.first() else { continue };
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            if !fr.is_empty() {
                finish_reason = fr.to_string();
            }
        }
        let delta = choice.get("delta").or_else(|| choice.get("message")).unwrap_or(choice);
        if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
            content.push_str(c);
        }
        // reasoning can arrive as `reasoning`, `reasoning_content`, or nested details
        for key in ["reasoning", "reasoning_content", "thinking"] {
            if let Some(r) = delta.get(key).and_then(|r| r.as_str()) {
                reasoning.push_str(r);
            }
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tcs {
                let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                if idx != cur_tool_index {
                    flush_tool(&mut cur_tool, &mut cur_args, &mut tool_calls);
                    cur_tool_index = idx;
                    cur_tool = Some(json!({
                        "id": tc.get("id").cloned().unwrap_or_else(|| json!("call_cline")),
                        "type": "function",
                        "function": {"name": "", "arguments": ""},
                    }));
                }
                if let Some(t) = cur_tool.as_mut() {
                    if let Some(fn_) = tc.get("function") {
                        if let Some(name) = fn_.get("name").and_then(|n| n.as_str()) {
                            if !name.is_empty() {
                                if let Some(f) = t.get_mut("function").and_then(|f| f.as_object_mut()) {
                                    f.insert("name".into(), Value::String(name.to_string()));
                                }
                            }
                        }
                        if let Some(args) = fn_.get("arguments").and_then(|a| a.as_str()) {
                            cur_args.push_str(args);
                        }
                    }
                }
            }
        }
    }
    flush_tool(&mut cur_tool, &mut cur_args, &mut tool_calls);

    let mut message = json!({
        "role": "assistant",
        "content": content,
    });
    if !reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let finish_reason = if finish_reason.is_empty() {
        "stop".to_string()
    } else {
        finish_reason
    };
    let mut out = json!({
        "id": format!("chatcmpl_{}", now_ms()),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
    });
    if let Some(u) = usage {
        out["usage"] = u;
    }
    serde_json::to_vec(&out).map_err(|e| format!("aggregated completion encode: {e}"))
}

// ---------------------------------------------------------------------------
// Provider implementation: free-model discovery.
// ---------------------------------------------------------------------------

pub struct Cline;

impl crate::Provider for Cline {
    fn id(&self) -> &'static str {
        "cline"
    }

    fn scan<'a>(
        &'a self,
        client: &'a Client,
        cfg: &'a proxy_fire_core::config::ProviderConfig,
    ) -> futures::future::BoxFuture<'a, Result<Vec<proxy_fire_core::catalog::CatalogEntry>, crate::ProviderError>> {
        Box::pin(async move {
            let mut raws: Vec<crate::RawModel> = SEED_FREE_MODELS
                .iter()
                .map(|(id, ctx, _)| crate::RawModel {
                    model_id: id.to_string(),
                    context_window: *ctx,
                    metadata: Some({
                        let mut m = serde_json::Map::new();
                        m.insert("source".into(), Value::String("cline".into()));
                        m.insert("requires_stream".into(), Value::Bool(true));
                        m
                    }),
                })
                .collect();

            // Best-effort live sync of Cline's free-model promotion.
            // Auth is optional: with no refresh token (or a failed refresh)
            // we ship the seed list instead of failing the whole scan.
            let seeds_done = raws.len();
            if let Some(refresh) = cfg.all_keys().into_iter().next() {
                match ensure_access_token(client, &refresh).await {
                    Ok(access) => {
                        let url = format!("{CLINE_API_BASE}/ai/cline/recommended-models");
                        match client
                            .get(&url)
                            .header("Authorization", access)
                            .timeout(Duration::from_secs(15))
                            .send()
                            .await
                        {
                            Ok(resp) if resp.status().as_u16() == 200 => {
                                if let Ok(v) = resp.json::<Value>().await {
                                    let mut ids: Vec<String> = Vec::new();
                                    for key in ["free", "models", "data"] {
                                        if let Some(arr) = v.get(key).and_then(|a| a.as_array()) {
                                            for item in arr {
                                                let id = item
                                                    .get("id")
                                                    .or_else(|| item.get("model"))
                                                    .and_then(|s| s.as_str())
                                                    .map(str::to_string);
                                                if let Some(id) = id {
                                                    if !id.is_empty() && !ids.contains(&id) {
                                                        ids.push(id);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    for id in ids {
                                        if raws.iter().any(|r| r.model_id == id) {
                                            continue;
                                        }
                                        raws.push(crate::RawModel {
                                            model_id: id,
                                            context_window: 128_000,
                                            metadata: Some({
                                                let mut m = serde_json::Map::new();
                                                m.insert("source".into(), Value::String("cline".into()));
                                                m.insert("requires_stream".into(), Value::Bool(true));
                                                m
                                            }),
                                        });
                                    }
                                    tracing::info!(
                                        "cline: recommended-models sync added {} live models ({} seeds)",
                                        raws.len() - seeds_done,
                                        seeds_done
                                    );
                                }
                            }
                            Ok(resp) => {
                                tracing::info!(
                                    "cline: recommended-models HTTP {} — using seed list",
                                    resp.status().as_u16()
                                );
                            }
                            Err(e) => {
                                tracing::info!("cline: recommended-models sync failed: {e} — using seed list");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::info!("cline: token refresh for scan failed: {e} — using seed list");
                    }
                }
            }

            Ok(crate::chat_entries("cline", "free", raws))
        })
    }
}
