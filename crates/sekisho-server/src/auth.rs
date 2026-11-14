//! Who is calling: people by their session tokens for the API, gateway
//! agents by the gateway token.
//!
//! A session token is checked against the published signing keys, its
//! session must not have ended, and the person is read from the database on
//! every request, so disabling them or changing their admin mark applies at
//! once. Admins are the people marked as one and the members of
//! `SEKISHO_ADMIN_GROUPS`.

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sekisho_core::access::is_admin;
use sekisho_core::keys::verify_secret;
use sekisho_core::session_token::{TokenError, verify};
use sekisho_store::{StoreError, User};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::api::ApiError;
use crate::app::AppState;

/// The signed-in person behind a request.
#[derive(Clone, Debug)]
pub struct Caller {
    /// As stored now, not as the token says.
    pub user: User,
    pub session: Uuid,
    /// Marked as an admin, or in one of `SEKISHO_ADMIN_GROUPS`.
    pub admin: bool,
}

/// The token of an `Authorization: Bearer <token>` header.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let (scheme, token) = headers.get(AUTHORIZATION)?.to_str().ok()?.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
}

fn unauthorized(message: &str) -> ApiError {
    ApiError::Unauthorized(message.to_owned())
}

/// The person a request's session token names.
async fn caller(state: &AppState, headers: &HeaderMap) -> Result<Caller, ApiError> {
    let Some(token) = bearer(headers) else {
        return Err(unauthorized(
            "sign in: send your session token as a bearer token",
        ));
    };
    if verify_secret(token, state.bootstrap_sha256()) {
        return Err(unauthorized(
            "sign in; the bootstrap token only works on /setup",
        ));
    }
    let cache = state.cache();
    let claims = match verify(
        token,
        |kid| cache.public_key(kid),
        cache.issuer(),
        OffsetDateTime::now_utc(),
    ) {
        Ok(claims) => claims,
        Err(TokenError::Expired) => {
            return Err(unauthorized("session token expired; refresh it"));
        }
        Err(_) => {
            return Err(unauthorized(
                "not a session token; environment keys work only on the gateway",
            ));
        }
    };
    let (Ok(user_id), Ok(session)) = (claims.sub.parse::<Uuid>(), claims.sid.parse::<Uuid>())
    else {
        return Err(unauthorized("not a session token"));
    };
    if cache.session_ended(&session) {
        return Err(unauthorized("session ended; sign in again"));
    }
    let user = match state.store().user(&user_id).await {
        Ok(user) => user,
        Err(StoreError::NotFound) => return Err(unauthorized("session ended; sign in again")),
        Err(e) => return Err(e.into()),
    };
    // Disabling ends sessions; the cache may not know yet.
    if user.disabled_at.is_some() {
        return Err(unauthorized("session ended; sign in again"));
    }
    let admin = is_admin(user.admin, &user.access(), state.admin_groups());
    Ok(Caller {
        user,
        session,
        admin,
    })
}

/// Any signed-in person.
pub async fn require_user(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    match caller(&state, req.headers()).await {
        Ok(caller) => {
            req.extensions_mut().insert(caller);
            next.run(req).await
        }
        Err(e) => e.into_response(),
    }
}

/// An admin.
pub async fn require_admin(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    match caller(&state, req.headers()).await {
        Ok(caller) if caller.admin => {
            req.extensions_mut().insert(caller);
            next.run(req).await
        }
        Ok(_) => ApiError::Forbidden.into_response(),
        Err(e) => e.into_response(),
    }
}

/// A gateway agent, by the gateway token.
pub async fn require_gateway(State(state): State<AppState>, req: Request, next: Next) -> Response {
    match bearer(req.headers()) {
        Some(token) if verify_secret(token, state.gateway_sha256()) => next.run(req).await,
        _ => unauthorized("missing or wrong bearer token").into_response(),
    }
}
