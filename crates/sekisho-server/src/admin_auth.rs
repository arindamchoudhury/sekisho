//! Bearer-token checks: the bootstrap token for the admin API, the gateway
//! token for gateway agents.

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sekisho_core::keys::verify_secret;

use crate::api::ApiError;
use crate::app::AppState;

/// The token of an `Authorization: Bearer <token>` header.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let (scheme, token) = headers.get(AUTHORIZATION)?.to_str().ok()?.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
}

fn check(headers: &HeaderMap, expected_sha256: &[u8; 32]) -> Result<(), ApiError> {
    match bearer(headers) {
        Some(token) if verify_secret(token, expected_sha256) => Ok(()),
        _ => Err(ApiError::Unauthorized),
    }
}

pub async fn require_bootstrap(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    match check(req.headers(), state.bootstrap_sha256()) {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}

pub async fn require_gateway(State(state): State<AppState>, req: Request, next: Next) -> Response {
    match check(req.headers(), state.gateway_sha256()) {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}
