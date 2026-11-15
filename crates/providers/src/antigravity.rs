//! Antigravity provider — the Google Antigravity IDE's Cloud Code API.
//!
//! Reverse-engineered from the Antigravity IDE and the community reference
//! (https://github.com/badrisnarayanan/antigravity-claude-proxy, MIT).
//! The upstream protocol is Google's Cloud Code ("generateContent") format,
//! NOT OpenAI's — so this provider translates:
//!
//!   OpenAI chat/completions body  →  Google generateContent envelope
//!   Google SSE stream (alt=sse) ←→  aggregated back to an OpenAI completion
//!
//! Auth model: same Google OAuth client the IDE itself uses (PKCE public
//! client id + secret baked into the official app). One Google account per
//! pool slot (ANTIGRAVITY_REFRESH_TOKEN[_2.._9] in ~/.proxy-fire/.env).
//! Access tokens are short-lived; refreshed on demand, cached in memory only.
//!
//! Upstream is always streamed and re-aggregated to a single JSON body so
//! Proxy-Fire's empty-response detection and usage accounting work unchanged.
//!
//! Free tier: models like gemini-3.8-flash require no credits on the free
//! plan (weekly quota). Paid tiers unlock Claude 5.5 / Gemini 4 Argon etc.
//! Models rotate; `proxy-fire scan update` re-syncs from fetchAvailableModels.

use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

pub const ANTIGRAVITY_API_BASE: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const ANTIGRAVITY_PROD_BASE: &str = "https://cloudcode-pa.googleapis.com";
pub const ANTIGRAVITY_DAILY_BASE: &str = "https://daily-cloudcode-pa.googleapis.com";

/// Model catalog fallback (Antigravity upstream, November 2026).
/// Tier-differentiated names (`*-low/-medium/-high`) are grouped under one
/// canonical id exposing an OpenAI-style `reasoning_effort` knob; the wire
/// id is derived at request time. Effort levels advertised per model:
///   Gemini 4 Hydrogen  — low / medium / high (all plans)
///   Gemini 3.8 Flash   — low / medium / high (all plans, baseline)
///   Gemini 4 Argon     — low / high (Google AI Pro & Ultra)
///   Gemini 4 Carbon    — high (Google AI Pro & Ultra)
///   Claude Sonnet 5.5  — high (Google AI Pro & Ultra, paid non-trial)
///   Claude Opus 5.5    — high (Google AI Pro & Ultra, paid non-trial)
#[derive(Clone, Copy)]
pub struct AgcModel {
    pub id: &'static str,
    pub context_window: i64,
    pub efforts: &'static [&'static str],
    pub thinking: bool,
    pub tier: &'static str,
}

const ALL_EFFORTS: &[&str] = &["low", "medium", "high"];
const HIGH_ONLY: &[&str] = &["high"];
const LOW_HIGH: &[&str] = &["low", "high"];

pub const SEED_MODELS: &[AgcModel] = &[
    // Gemini 4 family (1M unified context)
    AgcModel {
        id: "gemini-4-hydrogen",
        context_window: 1_048_576,
        efforts: ALL_EFFORTS,
        thinking: true,
        tier: "free",
    },
    AgcModel {
        id: "gemini-4-argon",
        context_window: 1_048_576,
        efforts: LOW_HIGH,
        thinking: true,
        tier: "credit",
    },
    AgcModel {
        id: "gemini-4-carbon",
        context_window: 1_048_576,
        efforts: HIGH_ONLY,
        thinking: true,
        tier: "credit",
    },
    // Gemini 3.8 Flash — baseline across all plans, including Free
    AgcModel {
        id: "gemini-3.8-flash",
        context_window: 1_048_576,
        efforts: ALL_EFFORTS,
        thinking: false,
        tier: "free",
    },
    // Claude 5.5 family (paid non-trial Google AI Pro & Ultra only)
    AgcModel {
        id: "claude-sonnet-5-5",
        context_window: 200_000,
        efforts: HIGH_ONLY,
        thinking: true,
        tier: "credit",
    },
    AgcModel {
        id: "claude-opus-5-5",
        context_window: 200_000,
        efforts: HIGH_ONLY,
        thinking: true,
        tier: "credit",
    },
];

/// Normalise an OpenAI-ish effort value ("minimal"/"low"/"medium"/"high").
fn normalize_effort(raw: &str) -> &'static str {
    match raw.to_ascii_lowercase().as_str() {
        "minimal" | "low" => "low",
        "medium" | "standard" => "medium",
        "high" | "max" | "xhigh" => "high",
        _ => "medium",
    }
}

/// The model's default effort when the caller doesn't pass reasoning_effort.
/// Prefers medium (OpenAI convention); falls back to the highest available.
fn default_effort(efforts: &[&'static str]) -> &'static str {
    if efforts.contains(&"medium") {
        "medium"
    } else {
        efforts.last().copied().unwrap_or("high")
    }
}

/// Resolve the wire (upstream) model id for a requested model + optional effort.
/// - Explicit upstream ids ("gemini-3.8-flash-high") pass through unchanged.
/// - Canonical ids ("gemini-3.8-flash") resolve to <canonical>-<effort>,
///   clamping reasoning_effort to the efforts the model actually supports.
pub fn wire_model_id(requested: &str, reasoning_effort: Option<&str>) -> String {
    // Explicit upstream id passes through untouched.
    for m in SEED_MODELS {
        for e in m.efforts {
            if requested.eq_ignore_ascii_case(&format!("{}-{}", m.id, e)) {
                return requested.to_string();
            }
        }
    }
    let Some(m) = SEED_MODELS
        .iter()
        .find(|m| m.id.eq_ignore_ascii_case(requested))
    else {
        return requested.to_string(); // unknown model — let upstream decide
    };
    let effort = reasoning_effort
        .map(normalize_effort)
        .unwrap_or_else(|| default_effort(m.efforts));
    let effort = if m.efforts.contains(&effort) {
        effort
    } else {
        // clamp: pick the closest supported level (low→low, medium→low|high, high→high)
        match effort {
            // single-effort models (e.g. claude 5.5) — any knob lands on the one level
            "low" => "high",
            "medium" => {
                if m.efforts.contains(&"low") {
                    "low"
                } else {
                    m.efforts[0]
                }
            }
            _ => m.efforts[0],
        }
    };
    format!("{}-{}", m.id, effort)
}

/// The effort levels to advertise for display purposes.
pub fn effort_levels(id: &str) -> Option<&'static [&'static str]> {
    SEED_MODELS.iter().find(|m| m.id == id).map(|m| m.efforts)
}

const REFRESH_SKEW_MS: i64 = 60_000;

/// Client version baked into the IDE (product.json "version").
/// Codename gates accept ≥ 1.110.0.
const ANTIGRAVITY_CLIENT_VERSION: &str = "1.110.0";
/// IDE "ideVersion" — the User-Agent must use the `antigravity/hub/<ver> os/arch`
/// format or the backend hides newer models from the client.
const ANTIGRAVITY_IDE_VERSION: &str = "2.25.0";
const X_GOOG_API_CLIENT: &str = "gl-node/18.18.2 fire/0.8.6 grpc/1.10.x";

pub fn ide_user_agent() -> String {
    let (os, arch) = platform_triple();
    format!("antigravity/hub/{ANTIGRAVITY_IDE_VERSION} {os}/{arch}")
}

fn platform_triple() -> (&'static str, &'static str) {
    let (os, arch) = if cfg!(target_os = "macos") {
        (
            "darwin",
            if cfg!(target_arch = "aarch64") {
                "arm64"
            } else {
                "x64"
            },
        )
    } else if cfg!(target_os = "windows") {
        (
            "win32",
            if cfg!(target_arch = "x86_64") {
                "x64"
            } else {
                "arm64"
            },
        )
    } else {
        (
            "linux",
            if cfg!(target_arch = "aarch64") {
                "arm64"
            } else {
                "x64"
            },
        )
    };
    (os, arch)
}

/// IDE_TYPE.ANTIGRAVITY = 9, PLUGIN_TYPE.GEMINI = 2 (numeric enums expected by the API).
fn client_metadata() -> Value {
    let platform = if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            2
        } else {
            1
        } // DARWIN_ARM64=2 / DARWIN_AMD64=1
    } else if cfg!(target_os = "windows") {
        5 // WINDOWS_AMD64
    } else {
        if cfg!(target_arch = "aarch64") {
            4
        } else {
            3
        } // LINUX_ARM64 / LINUX_AMD64
    };
    json!({
        "ideType": 9,
        "platform": platform,
        "pluginType": 2,
    })
}

/// Headers the IDE itself sends (required: X-Client-Name/Version, UA).
pub fn antigravity_client_headers() -> Vec<(String, String)> {
    vec![
        ("User-Agent".to_string(), ide_user_agent()),
        ("X-Client-Name".to_string(), "antigravity".to_string()),
        (
            "X-Client-Version".to_string(),
            ANTIGRAVITY_CLIENT_VERSION.to_string(),
        ),
        (
            "x-goog-api-client".to_string(),
            X_GOOG_API_CLIENT.to_string(),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Access-token cache (refresh tokens are stable, access tokens short-lived)
// ---------------------------------------------------------------------------

struct CachedAccess {
    access: String,
    expires_at: i64,
}

fn token_cache() -> &'static RwLock<std::collections::HashMap<String, CachedAccess>> {
    static CACHE: OnceLock<RwLock<std::collections::HashMap<String, CachedAccess>>> =
        OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(std::collections::HashMap::new()))
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

const OAUTH_CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const OAUTH_CLIENT_SECRET: &str = "GOCSPX-K58FWR486LJ1mLB8sXC4z6qDAf";
const OAUTH_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const OAUTH_CLIENT_SCOPES: &str = "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/cclog https://www.googleapis.com/auth/experimentsandconfigs";

#[derive(Deserialize)]
struct TokenResp {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

/// Exchange a refresh token for a usable access token (in-memory cache, 5-min skew).
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
        .post(OAUTH_TOKEN_URL)
        .form(&[
            ("client_id", OAUTH_CLIENT_ID),
            ("client_secret", OAUTH_CLIENT_SECRET),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("antigravity refresh failed: {e}"))?;
    if resp.status().as_u16() != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("antigravity refresh rejected: {body}"));
    }
    let t: TokenResp = resp
        .json()
        .await
        .map_err(|e| format!("refresh decode: {e}"))?;
    if let Some(err) = t.error {
        return Err(format!(
            "antigravity: {} {}",
            err,
            t.error_description.unwrap_or_default()
        ));
    }
    let expires_at = now_ms() + (t.expires_in.unwrap_or(3600) as i64) * 1000 - REFRESH_SKEW_MS;
    if let Ok(mut map) = token_cache().write() {
        map.insert(
            refresh_token.to_string(),
            CachedAccess {
                access: t.access_token.clone(),
                expires_at,
            },
        );
    }
    Ok(t.access_token)
}

/// Build the browser authorize URL for interactive login.
pub fn authorize_url(state: &str, code_challenge: &str) -> String {
    format!(
        "{OAUTH_AUTH_URL}?client_id={OAUTH_CLIENT_ID}&redirect_uri={redirect}&response_type=code&scope={scope}&access_type=offline&prompt=consent&state={state}&code_challenge={code_challenge}&code_challenge_method=S256",
        redirect = urlencoding_encode("http://localhost:51121/oauth-callback"),
        scope = urlencoding_encode(OAUTH_CLIENT_SCOPES),
    )
}

/// Minimal percent-encoder (avoids pulling in a new dependency for one call site).
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Exchange an interactive auth code for (access_token, refresh_token).
pub async fn exchange_code(
    client: &Client,
    code: &str,
    code_verifier: &str,
) -> Result<(String, String), String> {
    let resp = client
        .post(OAUTH_TOKEN_URL)
        .form(&[
            ("client_id", OAUTH_CLIENT_ID),
            ("client_secret", OAUTH_CLIENT_SECRET),
            ("code", code),
            ("code_verifier", code_verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", "http://localhost:51121/oauth-callback"),
        ])
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("token exchange failed: {e}"))?;
    if resp.status().as_u16() != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("token exchange rejected: {body}"));
    }
    #[derive(Deserialize)]
    struct CodeResp {
        access_token: String,
        refresh_token: Option<String>,
    }
    let c: CodeResp = resp
        .json()
        .await
        .map_err(|e| format!("token exchange decode: {e}"))?;
    let refresh = c.refresh_token.ok_or_else(|| {
        "Google returned no refresh_token (account already authorized; revoke access at https://myaccount.google.com/permissions and retry)".to_string()
    })?;
    Ok((c.access_token, refresh))
}

/// Call loadCodeAssist to discover project + tier (property names mirror the IDE).
pub async fn load_code_assist(client: &Client, access_token: &str) -> Result<Value, String> {
    let url = format!("{ANTIGRAVITY_PROD_BASE}/v1internal:loadCodeAssist");
    let resp = client
        .post(&url)
        .headers(authz_with_meta(access_token, None))
        .json(&json!({
            "metadata": client_metadata(),
            "mode": 1,
        }))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| format!("loadCodeAssist request failed: {e}"))?;
    let status = resp.status().as_u16();
    if status != 200 {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "loadCodeAssist rejected ({status}): {}",
            tail(&body)
        ));
    }
    resp.json()
        .await
        .map_err(|e| format!("loadCodeAssist decode: {e}"))
}

/// Onboard a freshly-consented account to a free-tier project.
pub async fn onboard_user(
    client: &Client,
    access_token: &str,
    tier_id: &str,
) -> Result<String, String> {
    for endpoint in [ANTIGRAVITY_DAILY_BASE, ANTIGRAVITY_PROD_BASE] {
        let url = format!("{endpoint}/v1internal:onboardUser");
        if let Ok(resp) = client
            .post(&url)
            .headers(authz_with_meta(access_token, None))
            .json(&json!({
                "tierId": tier_id,
                "metadata": client_metadata(),
            }))
            .timeout(Duration::from_secs(15))
            .send()
            .await
        {
            if resp.status().as_u16() == 200 {
                #[derive(Deserialize)]
                struct OnboardResp {
                    #[serde(default)]
                    done: Option<bool>,
                    #[serde(default, rename = "response")]
                    response: Option<Value>,
                }
                if let Ok(body) = resp.json::<OnboardResp>().await {
                    if body.done.unwrap_or(false) {
                        if let Some(pid) = body
                            .response
                            .as_ref()
                            .and_then(|v| v.get("cloudaicompanionProject"))
                            .and_then(|p| p.get("id"))
                            .and_then(|id| id.as_str())
                        {
                            return Ok(pid.to_string());
                        }
                    }
                }
            }
        }
    }
    Err("onboardUser did not complete (try again or create the project manually at https://console.cloud.google.com)".into())
}

fn authz_with_meta(token: &str, session_id: Option<&str>) -> reqwest::header::HeaderMap {
    let mut m = reqwest::header::HeaderMap::new();
    if let Ok(v) = format!("Bearer {token}").parse() {
        m.insert("Authorization", v);
    }
    for (k, v) in antigravity_client_headers() {
        if let Ok(name) = reqwest::header::HeaderName::from_bytes(k.as_bytes()) {
            if let Ok(value) = reqwest::header::HeaderValue::from_str(&v) {
                m.insert(name, value);
            }
        }
    }
    if let Some(s) = session_id {
        if let Ok(v) = reqwest::header::HeaderValue::from_str(s) {
            m.insert("X-Machine-Session-Id", v);
        }
    }
    m
}

fn tail(s: &str) -> String {
    s.chars().take(300).collect()
}

/// Live model list from fetchAvailableModels (ids only). Best-effort.
pub async fn list_models(
    client: &Client,
    access_token: &str,
    project_id: Option<&str>,
) -> Vec<String> {
    for endpoint in [ANTIGRAVITY_PROD_BASE, ANTIGRAVITY_DAILY_BASE] {
        let url = format!("{endpoint}/v1internal:fetchAvailableModels");
        let body = if let Some(p) = project_id {
            json!({"project": p})
        } else {
            json!({})
        };
        if let Ok(resp) = client
            .post(&url)
            .headers(authz_with_meta(access_token, None))
            .json(&body)
            .timeout(Duration::from_secs(15))
            .send()
            .await
        {
            if resp.status().as_u16() == 200 {
                if let Ok(v) = resp.json::<Value>().await {
                    let mut ids: Vec<String> = v
                        .get("models")
                        .and_then(|m| m.as_object())
                        .map(|m| {
                            m.keys()
                                .filter(|id| {
                                    let id = id.as_str();
                                    is_supported_model(id)
                                        && !is_old_gemini(id)
                                        && !is_disabled_placeholder(id)
                                })
                                .cloned()
                                .collect()
                        })
                        .unwrap_or_default();
                    ids.sort();
                    if !ids.is_empty() {
                        return ids;
                    }
                }
            }
        }
    }
    SEED_MODELS.iter().map(|m| m.id.to_string()).collect()
}

fn is_supported_model(id: &str) -> bool {
    let l = id.to_ascii_lowercase();
    l.contains("claude") || l.contains("gemini")
}
fn is_old_gemini(id: &str) -> bool {
    let l = id.to_ascii_lowercase();
    if l == "gemini-pro-agent" {
        return true;
    }
    l.split('-')
        .nth(1)
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v < 3.5)
        .unwrap_or(false)
}
fn is_disabled_placeholder(id: &str) -> bool {
    id.ends_with("-disabled")
}

/// Aggregate the SSE stream from upstream into a single chat.completion body.
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

    for raw in text.lines() {
        let Some(payload) = raw.trim_end_matches('\r').strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let mut obj: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(inner) = obj.get("data").filter(|d| d.is_object()).cloned() {
            if inner.get("choices").is_some()
                || inner.get("model").is_some()
                || inner.get("usage").is_some()
            {
                obj = inner;
            }
        }
        if let Some(m) = obj.get("model").and_then(|m| m.as_str()) {
            if !m.is_empty() {
                model = m.to_string();
            }
        }
        if let Some(u) = obj.get("usage") {
            if u.is_object() && !u.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                usage = Some(u.clone());
            }
        }
        let Some(choices) = obj.get("choices").and_then(|c| c.as_array()) else {
            continue;
        };
        let Some(choice) = choices.first() else {
            continue;
        };
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            if !fr.is_empty() {
                finish_reason = fr.to_string();
            }
        }
        let delta = choice
            .get("delta")
            .or_else(|| choice.get("message"))
            .unwrap_or(choice);
        if let Some(t) = delta.get("content").and_then(|c| c.as_str()) {
            content.push_str(t);
        }
        for k in ["reasoning", "reasoning_content", "thinking"] {
            if let Some(r) = delta.get(k).and_then(|r| r.as_str()) {
                reasoning.push_str(r);
            }
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tcs {
                let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                if idx != cur_tool_index {
                    if let Some(mut t) = cur_tool.take() {
                        if let Some(f) = t.get_mut("function").and_then(|f| f.as_object_mut()) {
                            f.insert("arguments".into(), json!(cur_args));
                        }
                        tool_calls.push(t);
                    }
                    cur_tool_index = idx;
                    cur_tool = Some(json!({
                        "id": tc.get("id").cloned().unwrap_or_else(|| json!("call_antigravity")),
                        "type": "function",
                        "function": {"name": "", "arguments": ""},
                    }));
                    cur_args.clear();
                }
                if let Some(t) = cur_tool.as_mut() {
                    if let Some(fn_) = tc.get("function") {
                        if let Some(name) = fn_.get("name").and_then(|n| n.as_str()) {
                            if !name.is_empty() {
                                if let Some(f) =
                                    t.get_mut("function").and_then(|f| f.as_object_mut())
                                {
                                    f.insert("name".into(), json!(name));
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
    if let Some(mut t) = cur_tool.take() {
        if let Some(f) = t.get_mut("function").and_then(|f| f.as_object_mut()) {
            f.insert("arguments".into(), json!(cur_args));
        }
        tool_calls.push(t);
    }

    let mut message = json!({"role": "assistant", "content": content});
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
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
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
    });
    if let Some(u) = usage {
        out["usage"] = u;
    }
    serde_json::to_vec(&out).map_err(|e| format!("aggregate encode: {e}"))
}

// ---------------------------------------------------------------------------
// OpenAI chat/completions body → Cloud Code (Gemini-format) request
// ---------------------------------------------------------------------------

/// Short, deterministic session id (mirrors the IDE's uuid+ms format).
pub fn new_session_id() -> String {
    let nanos = now_ms();
    format!("sess_{nanos:x}")
}

/// Pull plain text out of an OpenAI message `content` value.
fn text_of(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| {
                if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                    p.get("text").and_then(|t| t.as_str()).map(str::to_string)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn is_thinking_model(model_id: &str) -> bool {
    SEED_MODELS
        .iter()
        .find(|m| {
            m.id.eq_ignore_ascii_case(model_id)
                || format!("{}-high", m.id).eq_ignore_ascii_case(model_id)
                || format!("{}-medium", m.id).eq_ignore_ascii_case(model_id)
        })
        .map(|m| m.thinking)
        .unwrap_or_else(|| model_id.to_ascii_lowercase().contains("thinking"))
}

/// Build the wrapped Cloud Code request.
///   { project, model, request: { contents, generationConfig, tools?, systemInstruction? },
///     userAgent, requestType, requestId, sessionId }
pub fn build_request(
    client_body: &Value,
    requested_model: &str,
    project_id: Option<&str>,
) -> Value {
    let reasoning_effort = client_body.get("reasoning_effort").and_then(|v| v.as_str());
    let model_id = wire_model_id(requested_model, reasoning_effort);
    let thinking = is_thinking_model(&model_id);
    let session = new_session_id();

    let max_tokens = client_body
        .get("max_tokens")
        .or_else(|| client_body.get("max_completion_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(8_192);

    let mut contents: Vec<Value> = Vec::new();
    let mut system_parts: Vec<Value> = vec![json!({"text": "You are a helpful assistant."})];
    if let Some(msgs) = client_body.get("messages").and_then(|m| m.as_array()) {
        for m in msgs {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let text = text_of(m);
            match role {
                "system" => {
                    if !text.is_empty() {
                        system_parts.push(json!({"text": text}));
                    }
                }
                "user" => contents.push(json!({"role": "user", "parts": [{"text": text}]})),
                "assistant" => {
                    let mut parts: Vec<Value> = vec![json!({"text": text})];
                    if let Some(tcs) = m.get("tool_calls").and_then(|t| t.as_array()) {
                        for tc in tcs {
                            let f = tc.get("function").cloned().unwrap_or_else(|| json!({}));
                            let name = f
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("tool")
                                .to_string();
                            let args = f
                                .get("arguments")
                                .and_then(|a| a.as_str())
                                .unwrap_or("{}")
                                .to_string();
                            let parsed: Value =
                                serde_json::from_str(&args).unwrap_or_else(|_| json!({}));
                            parts.push(json!({
                                "functionCall": {
                                    "name": name,
                                    "args": parsed,
                                }
                            }));
                        }
                    }
                    contents.push(json!({"role": "model", "parts": parts}));
                }
                "tool" => {
                    let name = m
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("tool")
                        .to_string();
                    let result = m.get("content").cloned().unwrap_or(json!(text));
                    contents.push(json!({
                        "role": "function",
                        "parts": [{
                            "functionResponse": {
                                "name": name,
                                "response": {"result": result},
                            }
                        }],
                    }));
                }
                _ => {}
            }
        }
    }

    // Antigravity systemInstruction wants role=user + parts[]
    let system_instruction = json!({
        "role": "user",
        "parts": system_parts,
    });

    let mut generation_config = json!({
        "maxOutputTokens": max_tokens,
    });
    if let Some(t) = client_body.get("temperature") {
        generation_config["temperature"] = t.clone();
    }
    if let Some(t) = client_body.get("top_p") {
        generation_config["topP"] = t.clone();
    }
    if let Some(t) = client_body.get("top_k") {
        generation_config["topK"] = t.clone();
    }
    if let Some(t) = client_body.get("stop") {
        generation_config["stopSequences"] = t.clone();
    }
    if thinking {
        let effort = reasoning_effort.map(normalize_effort).unwrap_or("medium");
        let budget = match effort {
            "low" => 4096,
            "medium" => 8192,
            "high" => 32_000,
            _ => 8192,
        };
        generation_config["thinkingConfig"] = json!({
            "includeThoughts": true,
            "thinkingBudget": budget,
        });
    }
    if client_body.get("response_format").is_some() {
        generation_config["responseMimeType"] = json!("application/json");
    }

    let mut request = json!({
        "contents": contents,
        "systemInstruction": system_instruction,
        "generationConfig": generation_config,
    });

    // OpenAI tools → Gemini functionDeclarations
    if let Some(tools) = client_body.get("tools").and_then(|t| t.as_array()) {
        let fns: Vec<Value> = tools
            .iter()
            .filter_map(|t| t.get("function").cloned())
            .map(|f| {
                json!({
                    "name": f.get("name").cloned().unwrap_or_else(|| json!("unnamed")),
                    "description": f.get("description").cloned().unwrap_or_else(|| json!("")),
                    "parameters": f.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                })
            })
            .collect();
        if !fns.is_empty() {
            request["tools"] = json!([{"functionDeclarations": fns}]);
            if is_thinking_model(&model_id) {
                request["toolConfig"] = json!({
                    "functionCallingConfig": {"mode": "VALIDATED"}
                });
            }
        }
    }

    json!({
        "project": project_id,
        "model": model_id,
        "request": request,
        "userAgent": "antigravity",
        "requestType": "agent",
        "requestId": format!("agent-{}", uuid_like()),
        "sessionId": session,
    })
}

fn uuid_like() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let bytes: [u8; 16] = rng.random();
    let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Well-known fallback project for accounts without an assigned project.
pub const DEFAULT_PROJECT_ID: &str = "rising-fact-p41fc";

/// Discover (and cache) the Cloud Code project for one account.
/// 1) loadCodeAssist → cloudaicompanionProject (string or {id})
/// 2) if absent: onboardUser against the default tier → managed project id
/// 3) worst case: the public fallback project id
pub async fn project_id(client: &Client, access: &str) -> Result<String, String> {
    static CACHE: OnceLock<RwLock<std::collections::HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| RwLock::new(std::collections::HashMap::new()));
    if let Ok(map) = cache.read() {
        if let Some(p) = map.get(access) {
            return Ok(p.clone());
        }
    }
    let discovered = match load_code_assist(client, access).await {
        Ok(v) => extract_project(&v),
        Err(_) => None,
    };
    let pid = if let Some(p) = discovered {
        p
    } else {
        // Fresh account: onboard to the default (free) tier.
        let tier = match load_code_assist(client, access).await {
            Ok(v) => v
                .get("allowedTiers")
                .and_then(|t| t.as_array())
                .and_then(|arr| {
                    arr.iter()
                        .find(|t| t.get("isDefault").and_then(|v| v.as_bool()) == Some(true))
                        .or_else(|| arr.first())
                })
                .and_then(|t| t.get("id").and_then(|id| id.as_str()).map(str::to_string))
                .unwrap_or_else(|| "free-tier".to_string()),
            Err(_) => "free-tier".to_string(),
        };
        match onboard_user(client, access, &tier).await {
            Ok(p) => p,
            Err(_) => DEFAULT_PROJECT_ID.to_string(),
        }
    };
    if let Ok(mut map) = cache.write() {
        map.insert(access.to_string(), pid.clone());
    }
    Ok(pid)
}

fn extract_project(v: &Value) -> Option<String> {
    match v.get("cloudaicompanionProject") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Object(o)) => o.get("id").and_then(|id| id.as_str()).map(str::to_string),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Provider implementation: live model discovery
// ---------------------------------------------------------------------------

pub struct Antigravity;

impl crate::Provider for Antigravity {
    fn id(&self) -> &'static str {
        "antigravity"
    }

    fn scan<'a>(
        &'a self,
        client: &'a Client,
        cfg: &'a proxy_fire_core::config::ProviderConfig,
    ) -> futures::future::BoxFuture<
        'a,
        Result<Vec<proxy_fire_core::catalog::CatalogEntry>, crate::ProviderError>,
    > {
        Box::pin(async move {
            let mut raws: Vec<crate::RawModel> = SEED_MODELS
                .iter()
                .map(|m| crate::RawModel {
                    model_id: m.id.to_string(),
                    context_window: m.context_window,
                    metadata: Some({
                        let mut md = serde_json::Map::new();
                        md.insert("source".into(), Value::String("antigravity".into()));
                        md.insert("thinking".into(), Value::Bool(m.thinking));
                        md.insert("tier".into(), Value::String(m.tier.into()));
                        md.insert(
                            "efforts".into(),
                            Value::Array(m.efforts.iter().map(|e| json!(e)).collect()),
                        );
                        md
                    }),
                })
                .collect();

            // Best-effort live sync from fetchAvailableModels.
            if let Some(refresh) = cfg.all_keys().into_iter().next() {
                match ensure_access_token(client, &refresh).await {
                    Ok(access) => {
                        let project = project_id(client, &access).await.ok();
                        let ids = list_models(client, &access, project.as_deref()).await;
                        let seeds: std::collections::HashSet<String> =
                            SEED_MODELS.iter().map(|m| m.id.to_string()).collect();
                        let mut live: Vec<crate::RawModel> = ids
                            .into_iter()
                            .filter(|id| !seeds.contains(id))
                            .map(|id| {
                                let (cw, thinking, tier) = seed_meta_for(&id);
                                crate::RawModel {
                                    model_id: id,
                                    context_window: cw,
                                    metadata: Some({
                                        let mut md = serde_json::Map::new();
                                        md.insert(
                                            "source".into(),
                                            Value::String("antigravity".into()),
                                        );
                                        md.insert("thinking".into(), Value::Bool(thinking));
                                        md.insert("tier".into(), Value::String(tier.into()));
                                        md
                                    }),
                                }
                            })
                            .collect();
                        raws.append(&mut live);
                        tracing::info!(
                            "antigravity: {} seed + {} live models discovered",
                            SEED_MODELS.len(),
                            raws.len() - SEED_MODELS.len()
                        );
                    }
                    Err(e) => {
                        tracing::info!("antigravity: token refresh failed: {e} — using seed list");
                    }
                }
            }

            Ok(crate::chat_entries("antigravity", "free", raws))
        })
    }
}

fn seed_meta_for(id: &str) -> (i64, bool, &'static str) {
    SEED_MODELS
        .iter()
        .find(|m| m.id == id)
        .map(|m| (m.context_window, m.thinking, m.tier))
        .unwrap_or((131_072, true, "free"))
}

/// Generate a PKCE (S256) verifier + challenge for the interactive login flow.
pub fn pkce() -> (String, String) {
    use base64::Engine;
    use rand::Rng;
    use sha2::{Digest, Sha256};
    let mut rng = rand::rng();
    let bytes: [u8; 32] = rng.random();
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_defaults_to_medium_for_grouped_models() {
        assert_eq!(
            wire_model_id("gemini-3.8-flash", None),
            "gemini-3.8-flash-medium"
        );
        assert_eq!(
            wire_model_id("gemini-4-hydrogen", None),
            "gemini-4-hydrogen-medium"
        );
        assert_eq!(
            wire_model_id("claude-sonnet-5-5", None),
            "claude-sonnet-5-5-high"
        );
        assert_eq!(
            wire_model_id("gemini-4-carbon", None),
            "gemini-4-carbon-high"
        );
    }

    #[test]
    fn effort_knob_resolves_and_clamps() {
        assert_eq!(
            wire_model_id("gemini-3.8-flash", Some("low")),
            "gemini-3.8-flash-low"
        );
        assert_eq!(
            wire_model_id("gemini-3.8-flash", Some("high")),
            "gemini-3.8-flash-high"
        );
        assert_eq!(
            wire_model_id("gemini-3.8-flash", Some("medium")),
            "gemini-3.8-flash-medium"
        );
        // "minimal" → low (OpenAI convention)
        assert_eq!(
            wire_model_id("gemini-3.8-flash", Some("minimal")),
            "gemini-3.8-flash-low"
        );
        // argon has no medium — clamps down to low for medium requests
        assert_eq!(
            wire_model_id("gemini-4-argon", Some("medium")),
            "gemini-4-argon-low"
        );
        assert_eq!(
            wire_model_id("gemini-4-argon", Some("high")),
            "gemini-4-argon-high"
        );
        // claude sonnet is high-only — effort ignored
        assert_eq!(
            wire_model_id("claude-sonnet-5-5", Some("low")),
            "claude-sonnet-5-5-high"
        );
    }

    #[test]
    fn explicit_wire_ids_pass_through() {
        assert_eq!(
            wire_model_id("gemini-3.8-flash-high", None),
            "gemini-3.8-flash-high"
        );
        assert_eq!(
            wire_model_id("gemini-3.8-flash-medium", Some("low")),
            "gemini-3.8-flash-medium"
        );
        assert_eq!(
            wire_model_id("gemini-4-carbon-high", Some("low")),
            "gemini-4-carbon-high"
        );
    }

    #[test]
    fn unknown_models_pass_through_unchanged() {
        assert_eq!(
            wire_model_id("gemini-pro-agent", Some("high")),
            "gemini-pro-agent"
        );
        assert_eq!(
            wire_model_id("some-other-model", Some("low")),
            "some-other-model"
        );
    }

    #[test]
    fn effort_levels_reports_grouped_catalog() {
        assert_eq!(
            effort_levels("gemini-3.8-flash"),
            Some(&["low", "medium", "high"][..])
        );
        assert_eq!(effort_levels("gemini-4-argon"), Some(&["low", "high"][..]));
        assert_eq!(effort_levels("claude-sonnet-5-5"), Some(&["high"][..]));
        assert!(effort_levels("gemini-pro-agent").is_none());
    }
}
