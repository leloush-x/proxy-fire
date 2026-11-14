//! Re-emit a buffered chat completion as an OpenAI-style SSE stream.
//! The fallback chain always buffers; stream:true clients still expect SSE.

use serde_json::{json, Value};

/// Build the SSE byte stream for a finished (non-streaming) completion.
/// Falls back to a plain JSON payload when the body isn't a chat completion.
pub fn write_completion_as_sse(body: &[u8]) -> Vec<u8> {
    let c: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return body.to_vec(),
    };
    let choices = match c.get("choices").and_then(|v| v.as_array()) {
        Some(c) if !c.is_empty() => c.clone(),
        _ => return body.to_vec(),
    };
    let id = c
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "chatcmpl-{}",
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
            )
        });
    let created = c
        .get("created")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    let model = c
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let fingers = c
        .get("system_fingerprint")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let usage = c.get("usage").cloned();

    let mut out = String::new();
    let mut emit = |choices: Vec<Value>, usage: Option<Value>| {
        let mut chunk = json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": choices,
        });
        if let Some(f) = &fingers {
            chunk["system_fingerprint"] = Value::String(f.clone());
        }
        if let Some(u) = usage {
            chunk["usage"] = u;
        }
        out.push_str(&format!(
            "data: {}\n\n",
            serde_json::to_string(&chunk).unwrap()
        ));
    };

    for (i, ch) in choices.iter().enumerate() {
        let idx = choice_index(ch, i);
        let msg = ch.get("message").cloned().unwrap_or_else(|| json!({}));
        let mut delta = json!({"role": "assistant"});
        match msg.get("content") {
            Some(v) if !v.is_null() => delta["content"] = v.clone(),
            _ => delta["content"] = Value::String(String::new()),
        }
        if let Some(rc) = msg.get("reasoning_content") {
            if !rc.is_null() {
                delta["reasoning_content"] = rc.clone();
            }
        }
        if let Some(calls) = msg.get("tool_calls").and_then(|v| v.as_array()) {
            if !calls.is_empty() {
                let indexed: Vec<Value> = calls
                    .iter()
                    .enumerate()
                    .map(|(j, tc)| {
                        let mut m = tc.clone();
                        m["index"] = json!(j);
                        m
                    })
                    .collect();
                delta["tool_calls"] = json!(indexed);
            }
        }
        emit(
            vec![json!({"index": idx, "delta": delta, "finish_reason": Value::Null})],
            None,
        );
    }
    let n = choices.len();
    for (i, ch) in choices.iter().enumerate() {
        let idx = choice_index(ch, i);
        let finish = ch.get("finish_reason").cloned().unwrap_or(json!("stop"));
        let u = if i == n - 1 { usage.clone() } else { None };
        emit(
            vec![json!({"index": idx, "delta": json!({}), "finish_reason": finish})],
            u,
        );
    }

    out.push_str("data: [DONE]\n\n");
    out.into_bytes()
}

fn choice_index(ch: &Value, fallback: usize) -> i64 {
    ch.get("index")
        .and_then(|v| v.as_i64())
        .unwrap_or(fallback as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesizes_content_and_finish_chunks() {
        let body = br#"{"id":"x","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1}}"#;
        let sse = write_completion_as_sse(body);
        let text = String::from_utf8(sse).unwrap();
        assert!(text.contains("data: "));
        assert!(text.contains("chat.completion.chunk"));
        assert!(text.contains("\"content\":\"hi\""));
        assert!(text.contains("\"finish_reason\":\"stop\""));
        assert!(text.contains("data: [DONE]\n\n"));
    }

    #[test]
    fn passes_through_non_completion_bodies() {
        let body = b"garbage";
        assert_eq!(write_completion_as_sse(body), body);
    }
}
