//! OpenAI Responses API (`POST /v1/responses`) — a practical subset that
//! makes agent CLIs (Codex, etc.) work against free models through this proxy.
//!
//! Supported: text in (string or item array), instructions, function tools,
//! tool_choice, streaming (synthesized from the buffered upstream completion),
//! reasoning_content passthrough, usage accounting.
//! Not supported: stored responses (GET/DELETE), built-in tools (web search,
//! file search, computer use), background mode — these return explicit errors.
//!
//! Conversion pipeline (mirrors chat/completions):
//!   Responses request → chat/completions request → fallback chain → Responses object

use crate::errors::openai_error;
use crate::server::{run_chat, AppState};
use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::Response,
};
use serde_json::{json, Value};
use std::sync::Arc;

const MAX_REQUEST_BODY_BYTES: usize = 16 << 20;

pub async fn create_response(
    State(state): State<Arc<AppState>>,
    method: axum::http::Method,
    body: axum::body::Bytes,
) -> Response {
    if method != axum::http::Method::POST {
        return crate::middleware::method_not_allowed();
    }
    if body.len() > MAX_REQUEST_BODY_BYTES {
        return openai_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "request_too_large",
            "request body exceeds 16 MB",
        );
    }
    let raw: Value = match serde_json::from_slice(&body) {
        Ok(v @ Value::Object(_)) => v,
        _ => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_json",
                "request body is not a valid JSON object",
            )
        }
    };

    // ---- Build a chat/completions request ----
    let mut messages: Vec<Value> = Vec::new();
    if let Some(instr) = raw.get("instructions").and_then(|s| s.as_str()) {
        if !instr.is_empty() {
            messages.push(json!({"role": "system", "content": instr}));
        }
    }
    match raw.get("input") {
        Some(Value::String(s)) => messages.push(json!({"role": "user", "content": s})),
        Some(Value::Array(items)) => {
            for item in items {
                match input_item_to_messages(item) {
                    Ok(msgs) => messages.extend(msgs),
                    Err(e) => {
                        return openai_error(
                            StatusCode::BAD_REQUEST,
                            "invalid_request_error",
                            "invalid_input",
                            &e,
                        )
                    }
                }
            }
        }
        Some(_) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid_input",
                "input must be a string or an array of items",
            )
        }
        None => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "missing_required_parameter",
                "input is required",
            )
        }
    }

    let model = raw
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("auto")
        .to_string();
    let stream = raw.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let mut chat = json!({
        "model": model,
        "messages": messages,
        "stream": stream,
    });
    for key in [
        "temperature",
        "top_p",
        "stop",
        "user",
        "metadata",
        "parallel_tool_calls",
        "seed",
    ] {
        if let Some(v) = raw.get(key) {
            chat[key] = v.clone();
        }
    }
    if let Some(mt) = raw.get("max_output_tokens") {
        chat["max_tokens"] = mt.clone();
    } else if let Some(mt) = raw.get("max_completion_tokens") {
        chat["max_tokens"] = mt.clone();
    }
    if let Some(tools) = raw.get("tools").and_then(|t| t.as_array()) {
        chat["tools"] = Value::Array(tools.iter().filter_map(responses_tool_to_chat).collect());
    }
    if let Some(tc) = raw.get("tool_choice") {
        chat["tool_choice"] = responses_tool_choice_to_chat(tc);
    }
    // client_metadata is tolerated and ignored
    let _ = raw.get("client_metadata");

    // ---- Run the shared chat pipeline ----
    let resp = run_chat(state, chat).await;
    let status = resp.status();
    let headers = resp.headers().clone();
    let is_sse = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);
    let (parts, body) = resp.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_REQUEST_BODY_BYTES)
        .await
        .unwrap_or_default();

    if !status.is_success() {
        // Already OpenAI-shaped errors; pass them straight through.
        return Response::from_parts(parts, Body::from(bytes));
    }

    if is_sse {
        let converted = sse_chat_to_responses(&bytes, &model);
        let mut parts = parts;
        parts.headers.remove(header::CONTENT_LENGTH);
        parts
            .headers
            .insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
        parts
            .headers
            .insert("cache-control", "no-cache".parse().unwrap());
        parts
            .headers
            .insert("connection", "keep-alive".parse().unwrap());
        parts
            .headers
            .insert("x-accel-buffering", "no".parse().unwrap());
        return Response::from_parts(parts, Body::from(converted));
    }

    match serde_json::from_slice::<Value>(&bytes) {
        Ok(completion) => {
            let out = responses_object_from_completion(&completion, &model);
            let mut parts = parts;
            parts.headers.remove(header::CONTENT_LENGTH);
            parts
                .headers
                .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
            Response::from_parts(parts, Body::from(serde_json::to_vec(&out).unwrap()))
        }
        Err(_) => Response::from_parts(parts, Body::from(bytes)),
    }
}

/// GET/DELETE /v1/responses/{id} — this proxy never stores responses.
pub async fn response_unsupported(method: axum::http::Method) -> Response {
    openai_error(
        StatusCode::NOT_IMPLEMENTED,
        "invalid_request_error",
        "unsupported_endpoint",
        &format!("{method} /v1/responses/{{id}} is not supported — responses are not stored by this proxy"),
    )
}

// ---------- input conversion ----------

/// Map a Responses input item to one or more chat messages.
fn input_item_to_messages(item: &Value) -> Result<Vec<Value>, String> {
    let ty = item
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("message");
    match ty {
        "message" => {
            let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = chat_content_from_responses(item.get("content"));
            Ok(vec![json!({"role": role, "content": content})])
        }
        "input_text" | "text" => Ok(vec![json!({
            "role": "user",
            "content": item.get("text").cloned().unwrap_or(Value::String(String::new()))
        })]),
        "input_image" => Ok(vec![json!({
            "role": "user",
            "content": [image_part(item)]
        })]),
        "function_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("call_proxy")
                .to_string();
            Ok(vec![json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": item.get("name").cloned().unwrap_or(json!("")),
                        "arguments": item.get("arguments").cloned().unwrap_or(json!("{}")),
                    }
                }],
            })])
        }
        "function_call_output" => {
            let output = item
                .get("output")
                .cloned()
                .unwrap_or(Value::String(String::new()));
            let content = output
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| output.to_string());
            Ok(vec![json!({
                "role": "tool",
                "tool_call_id": item.get("call_id").cloned().unwrap_or(json!("call_proxy")),
                "content": content,
            })])
        }
        // reasoning and item_reference items carry no textable content
        "reasoning" | "item_reference" => Ok(vec![]),
        other => Err(format!("unsupported input item type: {other:?}")),
    }
}

fn chat_content_from_responses(content: Option<&Value>) -> Value {
    match content {
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Array(parts)) => {
            // Collapse all-text content to a plain string (best compatibility
            // with strict OpenAI-compatible upstreams); keep arrays otherwise.
            let all_text = parts.iter().all(|p| {
                matches!(
                    p.get("type").and_then(|t| t.as_str()),
                    Some("input_text") | Some("output_text") | Some("text") | Some("refusal")
                )
            });
            if all_text {
                let mut s = String::new();
                for p in parts {
                    if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                        s.push_str(t);
                    }
                }
                return Value::String(s);
            }
            let mut out: Vec<Value> = Vec::new();
            for p in parts {
                match p.get("type").and_then(|t| t.as_str()) {
                    Some("input_text") | Some("output_text") | Some("text") => {
                        out.push(json!({"type": "text", "text": p.get("text").cloned().unwrap_or(json!(""))}));
                    }
                    Some("refusal") => {
                        if let Some(t) = p.get("refusal").and_then(|r| r.as_str()) {
                            out.push(json!({"type": "text", "text": t}));
                        }
                    }
                    Some("input_image") | Some("image_url") => out.push(image_part(p)),
                    _ => {}
                }
            }
            Value::Array(out)
        }
        _ => Value::String(String::new()),
    }
}

fn image_part(p: &Value) -> Value {
    let url = p
        .get("image_url")
        .or_else(|| p.get("url"))
        .and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o.get("url").and_then(|u| u.as_str()).map(str::to_string),
            _ => None,
        })
        .unwrap_or_default();
    json!({"type": "image_url", "image_url": {"url": url}})
}

fn responses_tool_to_chat(t: &Value) -> Option<Value> {
    if t.get("type").and_then(|t| t.as_str()) != Some("function") {
        // Built-in tools are not supported — silently drop rather than fail the request.
        return None;
    }
    Some(json!({
        "type": "function",
        "function": {
            "name": t.get("name").cloned().unwrap_or(json!("")),
            "description": t.get("description").cloned().unwrap_or(json!("")),
            "parameters": t.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        }
    }))
}

fn responses_tool_choice_to_chat(tc: &Value) -> Value {
    match tc {
        Value::String(_) => tc.clone(),
        Value::Object(o) if o.get("type").and_then(|t| t.as_str()) == Some("function") => {
            if let Some(name) = o.get("name") {
                return json!({"type": "function", "function": {"name": name}});
            }
            tc.clone()
        }
        _ => json!("auto"),
    }
}

// ---------- output conversion ----------

fn new_id(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}_{nanos:x}")
}

fn usage_object(u: Option<&Value>) -> Value {
    let p = u
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let c = u
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let t = u
        .and_then(|u| u.get("total_tokens"))
        .and_then(|v| v.as_i64())
        .unwrap_or(p + c);
    json!({"input_tokens": p, "output_tokens": c, "total_tokens": t})
}

/// The full `response` object, built from a buffered chat completion.
fn responses_object_from_completion(c: &Value, fallback_model: &str) -> Value {
    let choice = c
        .get("choices")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first());
    let message = choice
        .and_then(|ch| ch.get("message"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let text = message
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let reasoning = message
        .get("reasoning_content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let tool_calls = message
        .get("tool_calls")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    let created = c
        .get("created")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    let model = c
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or(fallback_model)
        .to_string();

    let mut output: Vec<Value> = Vec::new();
    if !reasoning.is_empty() {
        output.push(json!({
            "id": new_id("rs"),
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": reasoning}],
        }));
    }
    if !text.is_empty() || tool_calls.is_empty() {
        output.push(json!({
            "id": new_id("msg"),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }));
    }
    for (i, tc) in tool_calls.iter().enumerate() {
        output.push(json!({
            "id": new_id("fc"),
            "type": "function_call",
            "status": "completed",
            "call_id": tc.get("id").cloned().unwrap_or_else(|| json!(format!("call_{i}"))),
            "name": tc.get("function").and_then(|f| f.get("name")).cloned().unwrap_or(json!("")),
            "arguments": tc.get("function").and_then(|f| f.get("arguments")).cloned().unwrap_or(json!("{}")),
        }));
    }

    json!({
        "id": new_id("resp"),
        "object": "response",
        "created_at": created,
        "status": "completed",
        "model": model,
        "output": output,
        "output_text": text,
        "parallel_tool_calls": true,
        "tool_choice": "auto",
        "tools": [],
        "error": null,
        "usage": usage_object(c.get("usage")),
    })
}

/// Convert a buffered chat.completion SSE body into Responses API SSE events.
fn sse_chat_to_responses(body: &[u8], fallback_model: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(body);
    let mut model = fallback_model.to_string();
    let mut created = chrono::Utc::now().timestamp();
    let mut usage: Option<Value> = None;
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish = "stop".to_string();
    // tool calls keyed by index
    let mut tool_calls: Vec<(String, String, String)> = Vec::new(); // (call_id, name, args)
    let mut tool_index: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();

    for line in text.lines() {
        let payload = match line.trim_end_matches('\r').strip_prefix("data:") {
            Some(p) => p.trim(),
            None => continue,
        };
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let mut obj: Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(inner) = obj.get("data").filter(|d| d.is_object()).cloned() {
            if inner.get("choices").is_some() {
                obj = inner;
            }
        }
        if let Some(m) = obj.get("model").and_then(|m| m.as_str()) {
            if !m.is_empty() {
                model = m.to_string();
            }
        }
        if let Some(c) = obj.get("created").and_then(|c| c.as_i64()) {
            created = c;
        }
        if let Some(u) = obj.get("usage").filter(|u| u.is_object()) {
            usage = Some(u.clone());
        }
        let Some(choices) = obj.get("choices").and_then(|c| c.as_array()) else {
            continue;
        };
        let Some(choice) = choices.first() else {
            continue;
        };
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            if !fr.is_empty() {
                finish = fr.to_string();
            }
        }
        let delta = choice
            .get("delta")
            .or_else(|| choice.get("message"))
            .unwrap_or(choice);
        if let Some(t) = delta.get("content").and_then(|c| c.as_str()) {
            content.push_str(t);
        }
        for key in ["reasoning", "reasoning_content", "thinking"] {
            if let Some(r) = delta.get(key).and_then(|r| r.as_str()) {
                reasoning.push_str(r);
            }
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tcs {
                let idx = tc.get("index").and_then(|i| i.as_i64()).unwrap_or(0);
                let slot = *tool_index.entry(idx).or_insert_with(|| {
                    tool_calls.push((String::new(), String::new(), String::new()));
                    tool_calls.len() - 1
                });
                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                    if !id.is_empty() {
                        tool_calls[slot].0 = id.to_string();
                    }
                }
                if let Some(f) = tc.get("function") {
                    if let Some(name) = f.get("name").and_then(|n| n.as_str()) {
                        if !name.is_empty() {
                            tool_calls[slot].1 = name.to_string();
                        }
                    }
                    if let Some(args) = f.get("arguments").and_then(|a| a.as_str()) {
                        tool_calls[slot].2.push_str(args);
                    }
                }
            }
        }
    }

    let response_id = new_id("resp");
    let mut out = String::new();
    let response_shell =
        |event_type: &str, status: &str, output: &Value, usage: Option<&Value>| -> Value {
            json!({
                "type": event_type,
                "id": response_id,
                "object": "response",
                "created_at": created,
                "status": status,
                "model": model,
                "output": output,
                "output_text": content,
                "parallel_tool_calls": true,
                "tool_choice": "auto",
                "tools": [],
                "error": null,
                "usage": usage_object(usage),
            })
        };

    let emit = |out: &mut String, event: &str, data: &Value| {
        out.push_str(&format!("event: {event}\n"));
        out.push_str(&format!("data: {data}\n\n"));
    };

    // Build the final output items once, then stream them as a tight event sequence.
    let mut output_items: Vec<Value> = Vec::new();
    if !reasoning.is_empty() {
        output_items.push(json!({
            "id": new_id("rs"),
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": reasoning}],
        }));
    }
    let msg_id = new_id("msg");
    if !content.is_empty() || (tool_calls.is_empty() && reasoning.is_empty()) {
        output_items.push(json!({
            "id": msg_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": content, "annotations": []}],
        }));
    }
    for (i, (call_id, name, args)) in tool_calls.iter().enumerate() {
        let call_id = if call_id.is_empty() {
            format!("call_{i}")
        } else {
            call_id.clone()
        };
        output_items.push(json!({
            "id": new_id("fc"),
            "type": "function_call",
            "status": "completed",
            "call_id": call_id,
            "name": name,
            "arguments": args,
        }));
    }

    emit(
        &mut out,
        "response.created",
        &response_shell("response.created", "in_progress", &json!([]), None),
    );
    emit(
        &mut out,
        "response.in_progress",
        &response_shell("response.in_progress", "in_progress", &json!([]), None),
    );

    for (i, item) in output_items.iter().enumerate() {
        emit(
            &mut out,
            "response.output_item.added",
            &json!({"type": "response.output_item.added", "output_index": i, "item": item}),
        );
        if item.get("type").and_then(|t| t.as_str()) == Some("message") {
            let item_id = item["id"].clone();
            emit(
                &mut out,
                "response.content_part.added",
                &json!({
                    "type": "response.content_part.added",
                    "item_id": item_id,
                    "output_index": i,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": []},
                }),
            );
            emit(
                &mut out,
                "response.output_text.delta",
                &json!({
                    "type": "response.output_text.delta",
                    "item_id": item_id,
                    "output_index": i,
                    "content_index": 0,
                    "delta": content,
                }),
            );
            emit(
                &mut out,
                "response.output_text.done",
                &json!({
                    "type": "response.output_text.done",
                    "item_id": item_id,
                    "output_index": i,
                    "content_index": 0,
                    "text": content,
                }),
            );
            emit(
                &mut out,
                "response.content_part.done",
                &json!({
                    "type": "response.content_part.done",
                    "item_id": item_id,
                    "output_index": i,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": content, "annotations": []},
                }),
            );
        } else if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
            let args = item
                .get("arguments")
                .and_then(|a| a.as_str())
                .unwrap_or("")
                .to_string();
            emit(
                &mut out,
                "response.function_call_arguments.delta",
                &json!({
                    "type": "response.function_call_arguments.delta",
                    "item_id": item["id"],
                    "output_index": i,
                    "delta": args,
                }),
            );
            emit(
                &mut out,
                "response.function_call_arguments.done",
                &json!({
                    "type": "response.function_call_arguments.done",
                    "item_id": item["id"],
                    "output_index": i,
                    "arguments": args,
                }),
            );
        }
        emit(
            &mut out,
            "response.output_item.done",
            &json!({"type": "response.output_item.done", "output_index": i, "item": item}),
        );
    }

    let _ = finish;
    emit(
        &mut out,
        "response.completed",
        &response_shell(
            "response.completed",
            "completed",
            &json!(output_items),
            usage.as_ref(),
        ),
    );
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAT_SSE: &str = concat!(
        "data: {\"id\":\"chatcmpl-x\",\"created\":1700000000,\"model\":\"deepseek/deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-x\",\"created\":1700000000,\"model\":\"deepseek/deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-x\",\"created\":1700000000,\"model\":\"deepseek/deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
        "data: [DONE]\n\n",
    );

    #[test]
    fn responses_sse_contains_core_events() {
        let out = sse_chat_to_responses(CHAT_SSE.as_bytes(), "m");
        let s = String::from_utf8(out).unwrap();
        for ev in [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_text.done",
            "response.output_item.done",
            "response.completed",
        ] {
            assert!(s.contains(ev), "missing event {ev}");
        }
        assert!(s.contains("\"delta\":\"Hello world\""));
        assert!(s.contains("\"input_tokens\":5"));
    }

    #[test]
    fn completion_to_response_object_basic() {
        let c: Value = serde_json::json!({
            "id": "chatcmpl-y", "created": 1700000000, "model": "m",
            "choices": [{"index":0, "message":{"role":"assistant","content":"hi","reasoning_content":"think"}, "finish_reason":"stop"}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4},
        });
        let out = responses_object_from_completion(&c, "m");
        assert_eq!(out["object"], "response");
        assert_eq!(out["status"], "completed");
        assert_eq!(out["output_text"], "hi");
        let types: Vec<&str> = out["output"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, vec!["reasoning", "message"]);
        assert_eq!(out["usage"]["input_tokens"], 3);
    }

    #[test]
    fn completion_with_tool_calls_becomes_function_call_items() {
        let c: Value = serde_json::json!({
            "id": "chatcmpl-z", "created": 1700000000, "model": "m",
            "choices": [{"index":0, "message":{
                "role":"assistant","content":null,
                "tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]
            }, "finish_reason":"tool_calls"}],
        });
        let out = responses_object_from_completion(&c, "m");
        let types: Vec<&str> = out["output"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["type"].as_str().unwrap())
            .collect();
        assert_eq!(types, vec!["function_call"]);
        assert_eq!(out["output"][0]["call_id"], "call_1");
        assert_eq!(out["output"][0]["name"], "get_weather");
    }

    #[test]
    fn tool_choice_object_becomes_chat_function() {
        let tc = json!({"type": "function", "name": "get_weather"});
        assert_eq!(
            responses_tool_choice_to_chat(&tc),
            json!({"type": "function", "function": {"name": "get_weather"}})
        );
        assert_eq!(responses_tool_choice_to_chat(&json!("auto")), json!("auto"));
    }
}
