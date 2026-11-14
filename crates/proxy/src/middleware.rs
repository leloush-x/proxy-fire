//! tower middleware: optional bearer auth, request logging, panic recovery,
//! CORS (permissive, like api.openai.com) and x-request-id generation.
//! API keys from upstream providers are NEVER logged.

use axum::{body::Body, http::Request, http::StatusCode, middleware::Next, response::Response};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::errors::openai_error;
use crate::server::AppState;

static REQUEST_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn gen_request_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = REQUEST_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("req_{nanos:x}{n:x}")
}

pub async fn auth(state: Arc<AppState>, req: Request<Body>, next: Next) -> Response {
    let token = state.cfg.read().unwrap().proxy.auth_token.clone();
    if token.is_empty() {
        return next.run(req).await;
    }
    let auth_header = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let x_api_key = req
        .headers()
        .get("x-api-key")
        .or_else(|| req.headers().get("api-key"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ok = auth_header == format!("Bearer {token}") || auth_header == token || x_api_key == token;
    if ok {
        return next.run(req).await;
    }
    openai_error(
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "invalid_api_key",
        "invalid or missing proxy auth token",
    )
}

/// Permissive CORS so browser-based clients work directly against the proxy.
/// Short-circuits OPTIONS preflights before any auth is applied.
pub async fn cors(req: Request<Body>, next: Next) -> Response {
    if req.method() == axum::http::Method::OPTIONS {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header("access-control-allow-origin", "*")
            .header("access-control-allow-methods", "GET, POST, PUT, PATCH, DELETE, OPTIONS")
            .header(
                "access-control-allow-headers",
                "authorization, content-type, x-api-key, api-key, openai-organization, openai-project, openai-beta, x-request-id",
            )
            .header("access-control-max-age", "86400")
            .body(Body::empty())
            .unwrap();
    }
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    headers.insert("access-control-allow-origin", "*".parse().unwrap());
    headers.insert(
        "access-control-allow-headers",
        "authorization, content-type, x-api-key, api-key, openai-organization, openai-project, openai-beta, x-request-id"
            .parse()
            .unwrap(),
    );
    headers.insert(
        "access-control-expose-headers",
        "x-request-id, x-cache, x-used-model, x-proxy-exhausted, retry-after"
            .parse()
            .unwrap(),
    );
    res
}

/// Every response carries an x-request-id (client-provided one wins).
pub async fn request_id(req: Request<Body>, next: Next) -> Response {
    let incoming = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .map(str::to_string);
    let mut res = next.run(req).await;
    if let Some(id) = incoming {
        res.headers_mut()
            .entry("x-request-id")
            .or_insert_with(|| id.parse().unwrap());
    } else if !res.headers().contains_key("x-request-id") {
        res.headers_mut()
            .insert("x-request-id", gen_request_id().parse().unwrap());
    }
    res
}

pub async fn logging(req: Request<Body>, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let res = next.run(req).await;
    tracing::info!("{} {} {} {:?}", method, path, res.status(), start.elapsed());
    res
}

pub async fn recovery(req: Request<Body>, next: Next) -> Response {
    // axum's default panic behavior returns an empty 500; we want the
    // OpenAI-style body.
    let result = std::panic::AssertUnwindSafe(next.run(req));
    match futures::FutureExt::catch_unwind(result).await {
        Ok(res) => res,
        Err(_) => {
            tracing::error!("panic while handling request");
            openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "internal_error",
                "internal server error",
            )
        }
    }
}

pub fn method_not_allowed() -> axum::response::Response {
    crate::errors::openai_error(
        axum::http::StatusCode::METHOD_NOT_ALLOWED,
        "invalid_request_error",
        "method_not_allowed",
        "method not allowed",
    )
}
