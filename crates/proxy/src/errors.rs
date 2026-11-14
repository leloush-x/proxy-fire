//! OpenAI-style error bodies so client SDKs surface the message.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

pub fn openai_error(status: StatusCode, err_type: &str, code: &str, message: &str) -> Response {
    (
        status,
        [
            ("content-type", "application/json"),
            ("x-content-type-options", "nosniff"),
        ],
        Json(json!({
            "error": {
                "message": message,
                "type": err_type,
                "param": null,
                "code": code,
            }
        })),
    )
        .into_response()
}

/// Seconds until the earliest provider cooldown lifts (Retry-After header),
/// rounded up, at least 1. Falls back to 5s when nothing is cooling down.
pub fn retry_after_seconds(earliest_recovery: Option<chrono::DateTime<chrono::Utc>>) -> u64 {
    const DEFAULT_WAIT_MS: i64 = 5_000;
    let wait_ms = match earliest_recovery {
        Some(t) => (t - chrono::Utc::now()).num_milliseconds().max(1000),
        None => DEFAULT_WAIT_MS,
    };
    ((wait_ms + 999) / 1000).max(1) as u64
}
