//! Provider-specific rate-limit header extraction → unified RateLimitInfo.
//!
//! Provider: OpenRouter — free tier 50 req/day; Retry-After on 429.
//! Provider: Groq — x-ratelimit-reset-requests/tokens (Go duration strings).
//! Provider: Gemini — X-RateLimit-Reset (unix ts); body may have RESOURCE_EXHAUSTED.
//! Provider: Cerebras — x-ratelimit-reset-tokens (fractional float seconds).
//! Provider: Mistral / Cohere / Together / DeepSeek / NVIDIA NIM — Retry-After.

use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct RateLimitInfo {
    /// Milliseconds to wait before retrying.
    pub retry_after_ms: i64,
    /// Unix seconds when the limit resets (Gemini, Cerebras).
    pub reset_timestamp: i64,
    /// Remaining requests in the current window (-1 = unknown).
    pub remaining_requests: i64,
    /// Remaining tokens in the current window (-1 = unknown).
    pub remaining_tokens: i64,
    /// Cerebras fractional-seconds token-bucket reset.
    pub cerebras_tok_reset_ms: i64,
    /// "RESOURCE_EXHAUSTED" when Gemini signals a quota error.
    pub gemini_error_type: String,
    /// True when a daily quota is exhausted → wait until midnight PT.
    pub is_rpd_exhausted: bool,
    /// Groq reset durations, parsed.
    pub groq_reset_requests_ms: i64,
    pub groq_reset_tokens_ms: i64,
}

impl RateLimitInfo {
    /// How long to wait before retrying.
    pub fn wait_duration(&self, now: DateTime<Utc>) -> Duration {
        if self.is_rpd_exhausted {
            return time_until_midnight_pt(now);
        }
        if self.retry_after_ms > 0 {
            // Add up to 10% deterministic jitter
            let jitter_ms = self.retry_after_ms / 10;
            if jitter_ms > 0 {
                let jitter = (now.timestamp_nanos_opt().unwrap_or(0).rem_euclid(jitter_ms)).max(0);
                return Duration::milliseconds(self.retry_after_ms + jitter);
            }
            return Duration::milliseconds(self.retry_after_ms);
        }
        Duration::seconds(5)
    }
}

/// Extract all known rate-limit signals from a response.
pub fn extract_rate_limit_info(
    provider_id: &str,
    headers: &HashMap<String, String>,
    body: &[u8],
) -> RateLimitInfo {
    // HTTP headers are case-insensitive; normalize once for lookup.
    let lowered: HashMap<String, String> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect();
    let get = |name: &str| lowered.get(name).cloned();

    let mut info = RateLimitInfo {
        remaining_requests: -1,
        remaining_tokens: -1,
        ..Default::default()
    };

    // Universal: Retry-After (seconds)
    if let Some(ra) = get("retry-after") {
        if let Ok(secs) = ra.trim().parse::<i64>() {
            info.retry_after_ms = secs * 1000;
        }
    }
    // Universal: remaining count
    if let Some(rem) = get("x-ratelimit-remaining") {
        if let Ok(n) = rem.trim().parse::<i64>() {
            info.remaining_requests = n;
        }
    }

    match provider_id.to_ascii_lowercase().as_str() {
        "gemini" => extract_gemini(&mut info, &get, body),
        "cerebras" => extract_cerebras(&mut info, &get),
        "groq" => extract_groq(&mut info, &get),
        "cline" => extract_cline(&mut info, body),
        _ => {}
    }
    info
}

/// Cline daily-cap 429 (INFERENCE_CAP_ERROR) body → cooldown hint.
/// Bodies look like: "…INFERENCE_CAP_ERROR… Try again in 17h 59m…".
/// When the cap is present but unparseable, bench the slot for 18h
/// (Cline's free quota resets daily).
fn extract_cline(info: &mut RateLimitInfo, body: &[u8]) {
    let text = String::from_utf8_lossy(body);
    let lower = text.to_ascii_lowercase();
    if !lower.contains("inference_cap") && !lower.contains("try again in") {
        return;
    }
    let ms = proxy_fire_providers_cline_parse_ms(body);
    if ms > 0 {
        info.retry_after_ms = ms;
    } else {
        // Conservative default: 18h
        info.retry_after_ms = 18 * 3_600_000;
    }
}

// Local copy of the duration parser (core can't depend on providers).
fn proxy_fire_providers_cline_parse_ms(body: &[u8]) -> i64 {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    let Some(idx) = text.find("try again in") else {
        return 0;
    };
    let re = regex::Regex::new(r"(\d+)\s*(h|m|s)").unwrap();
    let tail = &text[idx..];
    let mut total: i64 = 0;
    let mut any = false;
    for cap in re.captures_iter(tail) {
        let n: i64 = cap[1].parse().unwrap_or(0);
        any = true;
        total += match &cap[2] {
            "h" => n * 3_600_000,
            "m" => n * 60_000,
            _ => n * 1_000,
        };
    }
    if any {
        total
    } else {
        0
    }
}

fn extract_gemini(info: &mut RateLimitInfo, get: &dyn Fn(&str) -> Option<String>, body: &[u8]) {
    if String::from_utf8_lossy(body).contains("RESOURCE_EXHAUSTED") {
        info.gemini_error_type = "RESOURCE_EXHAUSTED".to_string();
    }
    // Parse "details": [{"@type": "...RetryInfo", "retryDelay": "30s"}]
    #[derive(Deserialize)]
    struct ErrWrap {
        error: ErrBody,
    }
    #[derive(Deserialize)]
    struct ErrBody {
        #[serde(default)]
        details: Vec<ErrDetail>,
    }
    #[derive(Deserialize)]
    struct ErrDetail {
        #[serde(rename = "@type")]
        type_: Option<String>,
        #[serde(rename = "retryDelay")]
        retry_delay: Option<String>,
    }
    if let Ok(wrap) = serde_json::from_slice::<ErrWrap>(body) {
        for d in wrap.error.details {
            let is_retry_info = d
                .type_
                .as_deref()
                .map(|t| t.contains("RetryInfo"))
                .unwrap_or(false);
            if !is_retry_info {
                continue;
            }
            if let Some(delay) = d.retry_delay {
                if let Ok(dur) = humantime::parse_duration(delay.trim()) {
                    let ms = dur.as_millis() as i64;
                    if ms > info.retry_after_ms {
                        info.retry_after_ms = ms;
                    }
                }
            }
        }
    }
    // X-RateLimit-Reset: unix timestamp (seconds)
    if let Some(reset) = get("x-ratelimit-reset") {
        if let Ok(ts) = reset.trim().parse::<i64>() {
            info.reset_timestamp = ts;
            let diff_ms = ts * 1000 - Utc::now().timestamp_millis();
            if diff_ms > info.retry_after_ms {
                info.retry_after_ms = diff_ms;
            }
        }
    }
    // More than 60 minutes away → RPD exhaustion; wait until midnight PT.
    if info.retry_after_ms > 60 * 60 * 1000 {
        info.is_rpd_exhausted = true;
    }
    if let Some(rem) = get("x-ratelimit-remaining") {
        if let Ok(n) = rem.trim().parse::<i64>() {
            info.remaining_requests = n;
        }
    }
}

fn extract_cerebras(info: &mut RateLimitInfo, get: &dyn Fn(&str) -> Option<String>) {
    // x-ratelimit-reset-tokens: fractional float seconds
    if let Some(tr) = get("x-ratelimit-reset-tokens") {
        if let Ok(secs) = tr.trim().parse::<f64>() {
            info.cerebras_tok_reset_ms = (secs * 1000.0) as i64;
            if info.cerebras_tok_reset_ms > info.retry_after_ms {
                info.retry_after_ms = info.cerebras_tok_reset_ms;
            }
        }
    }
    if let Some(rem) = get("x-ratelimit-remaining-requests") {
        if let Ok(n) = rem.trim().parse::<i64>() {
            info.remaining_requests = n;
        }
    }
    if let Some(rem) = get("x-ratelimit-remaining-tokens") {
        if let Ok(n) = rem.trim().parse::<i64>() {
            info.remaining_tokens = n;
        }
    }
    if let Some(ts) = get("x-ratelimit-reset-requests") {
        if let Ok(secs) = ts.trim().parse::<f64>() {
            info.reset_timestamp = Utc::now().timestamp() + secs as i64;
        }
    }
}

fn extract_groq(info: &mut RateLimitInfo, get: &dyn Fn(&str) -> Option<String>) {
    // Groq uses Go duration strings like "1m0s", "6.566s"
    for (name, slot) in [
        (
            "x-ratelimit-reset-requests",
            &mut info.groq_reset_requests_ms,
        ),
        ("x-ratelimit-reset-tokens", &mut info.groq_reset_tokens_ms),
    ] {
        if let Some(r) = get(name) {
            if let Some(dur) = parse_go_duration(r.trim()) {
                let ms = dur.as_millis() as i64;
                *slot = ms;
                if ms > info.retry_after_ms {
                    info.retry_after_ms = ms;
                }
            }
        }
    }
    if let Some(rem) = get("x-ratelimit-remaining-requests") {
        if let Ok(n) = rem.trim().parse::<i64>() {
            info.remaining_requests = n;
        }
    }
    if let Some(rem) = get("x-ratelimit-remaining-tokens") {
        if let Ok(n) = rem.trim().parse::<i64>() {
            info.remaining_tokens = n;
        }
    }
}

/// Parse a Go-style duration string ("1m30s", "6.566s", "500ms", "1h").
pub fn parse_go_duration(s: &str) -> Option<std::time::Duration> {
    // Fast path: humantime handles "6.5s", "2m", "1h30m" — but not bare "100ms"? It does: "100ms".
    humantime::parse_duration(s).ok()
}

/// Duration from now until the next midnight in America/Los_Angeles.
pub fn time_until_midnight_pt(now: DateTime<Utc>) -> Duration {
    use chrono::TimeZone;
    let tz = chrono_tz::America::Los_Angeles;
    let now_pt = now.with_timezone(&tz);
    let tomorrow = (now_pt.date_naive() + chrono::Days::new(1))
        .and_hms_opt(0, 0, 0)
        .expect("valid midnight");
    let midnight_pt = tz
        .from_local_datetime(&tomorrow)
        .single()
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|| now + Duration::hours(24));
    midnight_pt - now
}

/// The most recent midnight PT.
pub fn midnight_pt() -> DateTime<Utc> {
    use chrono::TimeZone;
    let tz = chrono_tz::America::Los_Angeles;
    let now_pt = Utc::now().with_timezone(&tz);
    let today = now_pt
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("valid midnight");
    tz.from_local_datetime(&today)
        .single()
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(Utc::now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    #[test]
    fn extracts_groq_duration_strings() {
        let mut h = HashMap::new();
        h.insert("x-ratelimit-reset-requests".to_string(), "1m0s".to_string());
        h.insert("x-ratelimit-reset-tokens".to_string(), "6.566s".to_string());
        let info = extract_rate_limit_info("groq", &h, b"");
        assert_eq!(info.groq_reset_requests_ms, 60_000);
        assert_eq!(info.groq_reset_tokens_ms, 6_566);
        assert_eq!(info.retry_after_ms, 60_000);
    }

    #[test]
    fn extracts_gemini_retry_delay() {
        let body = br#"{"error":{"status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"30s"}]}}"#;
        let info = extract_rate_limit_info("gemini", &HashMap::new(), body);
        assert_eq!(info.gemini_error_type, "RESOURCE_EXHAUSTED");
        assert_eq!(info.retry_after_ms, 30_000);
    }

    #[test]
    fn gemini_rpd_wait_covers_midnight() {
        let now = Utc::now();
        let wait = time_until_midnight_pt(now);
        assert!(wait > Duration::zero());
        assert!(wait <= Duration::hours(25));
    }

    #[test]
    fn wait_duration_uses_retry_after_with_jitter() {
        let info = RateLimitInfo {
            retry_after_ms: 10_000,
            ..Default::default()
        };
        let d = info.wait_duration(Utc::now());
        assert!(d >= Duration::milliseconds(10_000));
        assert!(d < Duration::milliseconds(11_100));
    }
}
