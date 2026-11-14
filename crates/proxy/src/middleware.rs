//! tower middleware: optional bearer auth, request logging, panic recovery.
//! API keys from upstream providers are NEVER logged.

use axum::{body::Body, http::Request, http::StatusCode, middleware::Next, response::Response};
use std::sync::Arc;
use std::time::Instant;

use crate::errors::openai_error;
use crate::server::AppState;

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
    if auth_header == format!("Bearer {token}") || auth_header == token {
        return next.run(req).await;
    }
    openai_error(
        StatusCode::UNAUTHORIZED,
        "authentication_error",
        "invalid_api_key",
        "invalid or missing proxy auth token",
    )
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
