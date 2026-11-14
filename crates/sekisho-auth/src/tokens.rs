//! The token endpoint (authorisation codes, device codes and refresh tokens)
//! and RFC 7009 revocation.
//!
//! Every refresh re-checks the person at Dex while the session's row is
//! locked, then rotates the refresh token. Errors are RFC 6749 JSON, never
//! cached.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use sekisho_core::session_token::{
    ACCESS_TOKEN_LIFETIME, AUDIENCE, SessionClaims, SigningKey, issue, looks_like_jwt, verify,
};
use sekisho_store::{CodeGrant, RefreshCheck, StoreError};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tracing::{error, info};
use uuid::Uuid;

use crate::AuthState;
use crate::clients::{self, AUTHORIZATION_CODE, Client, DEVICE_CODE, REFRESH_TOKEN};
use crate::device;
use crate::oauth::{Params, s256};
use crate::revalidate::{Revalidation, revalidate_session};

/// An RFC 6749 error answer.
pub(crate) fn oauth_error(status: StatusCode, error: &str, description: Option<&str>) -> Response {
    let mut body = json!({ "error": error });
    if let Some(d) = description {
        body["error_description"] = Value::from(d);
    }
    no_store(status, body)
}

pub(crate) fn no_store(status: StatusCode, body: Value) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        Json(body),
    )
        .into_response()
}

pub(crate) fn invalid_request(description: &str) -> Response {
    oauth_error(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        Some(description),
    )
}

pub(crate) fn invalid_grant() -> Response {
    oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", None)
}

pub(crate) fn server_error(e: &StoreError) -> Response {
    error!(error = %e, "the store failed at the token endpoint");
    oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", None)
}

fn unavailable(description: &str) -> Response {
    oauth_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "temporarily_unavailable",
        Some(description),
    )
}

/// The request's form parameters, or the `invalid_request` description.
pub(crate) fn form(headers: &HeaderMap, body: &[u8]) -> Result<Params, String> {
    let is_form = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| {
            v.trim()
                .eq_ignore_ascii_case("application/x-www-form-urlencoded")
        });
    if !is_form {
        return Err("send the parameters as application/x-www-form-urlencoded".to_owned());
    }
    Params::parse(body).map_err(|name| format!("{name} is given twice"))
}

/// A verifier as RFC 7636 section 4.1 allows: 43 to 128 unreserved
/// characters.
fn is_verifier(s: &str) -> bool {
    (43..=128).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~'))
}

/// Whether a code's grant holds for this exchange: the same client and
/// redirect URI, and the verifier of its challenge.
fn grant_holds(grant: &CodeGrant, client_id: &str, redirect_uri: &str, verifier: &str) -> bool {
    grant.client_id == client_id
        && grant.redirect_uri == redirect_uri
        && is_verifier(verifier)
        && s256(verifier) == grant.code_challenge
}

/// Why no key can sign a token now.
pub(crate) enum NoKey {
    /// None has been made yet: the server makes one before it listens.
    NotYet,
    Failed(StoreError),
}

impl NoKey {
    pub(crate) fn answer(&self) -> Response {
        match self {
            Self::NotYet => {
                error!("no signing key signs yet");
                unavailable("Sekisho has no signing key yet")
            }
            Self::Failed(e) => server_error(e),
        }
    }
}

/// The key that signs at `now`.
pub(crate) async fn signing_key(
    state: &AuthState,
    now: OffsetDateTime,
) -> Result<SigningKey, NoKey> {
    match state.store.active_signing_key(&state.secret_box, now).await {
        Ok(Some(key)) => Ok(key),
        Ok(None) => Err(NoKey::NotYet),
        Err(e) => Err(NoKey::Failed(e)),
    }
}

/// The token answer: a new access token for the session, and its refresh
/// token.
pub(crate) fn tokens(
    state: &AuthState,
    key: &SigningKey,
    user: Uuid,
    session: Uuid,
    client: &Client,
    refresh_token: &str,
    now: OffsetDateTime,
) -> Response {
    let claims = SessionClaims {
        iss: state.issuer(),
        sub: user.to_string(),
        aud: AUDIENCE.to_owned(),
        sid: session.to_string(),
        client_id: client.id.to_owned(),
        iat: now.unix_timestamp(),
        exp: (now + ACCESS_TOKEN_LIFETIME).unix_timestamp(),
    };
    no_store(
        StatusCode::OK,
        json!({
            "access_token": issue(key, &claims),
            "token_type": "Bearer",
            "expires_in": ACCESS_TOKEN_LIFETIME.whole_seconds(),
            "refresh_token": refresh_token,
        }),
    )
}

/// `POST /oauth/token`.
pub(crate) async fn token(
    State(state): State<AuthState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let params = match form(&headers, &body) {
        Ok(params) => params,
        Err(why) => return invalid_request(&why),
    };
    let Some(client) = params.get("client_id").and_then(clients::find) else {
        return oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            Some("unknown client"),
        );
    };
    let Some(grant_type) = params.get("grant_type") else {
        return invalid_request("grant_type is missing");
    };
    if !matches!(grant_type, AUTHORIZATION_CODE | REFRESH_TOKEN | DEVICE_CODE) {
        return oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", None);
    }
    if !client.allows(grant_type) {
        return oauth_error(StatusCode::BAD_REQUEST, "unauthorized_client", None);
    }
    match grant_type {
        AUTHORIZATION_CODE => redeem_code(&state, client, &params).await,
        DEVICE_CODE => device::redeem(&state, client, &params).await,
        _ => refresh(&state, client, &params).await,
    }
}

/// An authorisation code for a new session.
async fn redeem_code(state: &AuthState, client: &Client, params: &Params) -> Response {
    let (Some(code), Some(redirect_uri), Some(verifier)) = (
        params.get("code"),
        params.get("redirect_uri"),
        params.get("code_verifier"),
    ) else {
        return invalid_request("code, redirect_uri and code_verifier are required");
    };
    let now = state.now();
    // Taking the code uses it up, whatever follows.
    let grant = match state.store.take_code(code, now).await {
        Ok(Some(grant)) => grant,
        Ok(None) => return invalid_grant(),
        Err(e) => return server_error(&e),
    };
    if !grant_holds(&grant, client.id, redirect_uri, verifier) {
        return invalid_grant();
    }
    match state.store.user(&grant.user).await {
        Ok(user) if user.disabled_at.is_none() => {}
        Ok(_) | Err(StoreError::NotFound) => return invalid_grant(),
        Err(e) => return server_error(&e),
    }
    let key = match signing_key(state, now).await {
        Ok(key) => key,
        Err(no_key) => return no_key.answer(),
    };
    let (session, refresh_token) =
        match state.store.create_session(grant.user, client.id, now).await {
            Ok(created) => created,
            Err(e) => return server_error(&e),
        };
    info!(user = %grant.user, session = %session.id, "opened a session");
    tokens(
        state,
        &key,
        grant.user,
        session.id,
        client,
        &refresh_token,
        now,
    )
}

/// A refresh token for a new access token: the person is re-checked at Dex
/// and the refresh token rotates.
async fn refresh(state: &AuthState, client: &Client, params: &Params) -> Response {
    let Some(presented) = params.get("refresh_token") else {
        return invalid_request("refresh_token is missing");
    };
    let lock = match state
        .store
        .begin_refresh(presented, client.id, state.now())
        .await
    {
        Ok(RefreshCheck::Live(lock)) => lock,
        Ok(RefreshCheck::ReusedInGrace) => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                Some("refresh token rotation in progress"),
            );
        }
        Ok(RefreshCheck::Reused) => {
            info!("a rotated refresh token came back: its session has ended");
            return invalid_grant();
        }
        Ok(
            RefreshCheck::Unknown
            | RefreshCheck::OtherClient
            | RefreshCheck::Ended
            | RefreshCheck::Idle,
        ) => return invalid_grant(),
        Err(e) => return server_error(&e),
    };
    if let Err(no_key) = signing_key(state, state.now()).await {
        return no_key.answer();
    }
    let mut lock = match revalidate_session(state, lock).await {
        Ok(lock) => lock,
        Err(Revalidation::Refused | Revalidation::Disabled) => return invalid_grant(),
        Err(Revalidation::Unreachable | Revalidation::Unavailable) => {
            return unavailable("the identity provider can't be asked right now; try again");
        }
        Err(Revalidation::Internal) => {
            return oauth_error(StatusCode::INTERNAL_SERVER_ERROR, "server_error", None);
        }
    };
    // Dex may have taken seconds: issue at the time of answering.
    let now = state.now();
    let key = match signing_key(state, now).await {
        Ok(key) => key,
        Err(no_key) => return no_key.answer(),
    };
    let session = lock.session().clone();
    let refresh_token = match lock.rotate(now).await {
        Ok(token) => token,
        Err(e) => return server_error(&e),
    };
    if let Err(e) = lock.commit().await {
        return server_error(&e);
    }
    tokens(
        state,
        &key,
        session.user_id,
        session.id,
        client,
        &refresh_token,
        now,
    )
}

/// `POST /oauth/revoke` (RFC 7009): a refresh token or an access token ends
/// its session. Unknown tokens, and tokens of another client, are answered
/// 200 too.
pub(crate) async fn revoke(
    State(state): State<AuthState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let params = match form(&headers, &body) {
        Ok(params) => params,
        Err(why) => return invalid_request(&why),
    };
    let Some(token) = params.get("token") else {
        return invalid_request("token is missing");
    };
    let revoked = match params.get("client_id").and_then(clients::find) {
        None => Ok(()),
        Some(client) if looks_like_jwt(token) => revoke_access(&state, client, token).await,
        Some(client) => state
            .store
            .end_session_by_refresh(token, client.id, state.now())
            .await
            .map(|ended| {
                if ended {
                    info!("a session was revoked by its refresh token");
                }
            }),
    };
    match revoked {
        Ok(()) => (StatusCode::OK, [(header::CACHE_CONTROL, "no-store")]).into_response(),
        Err(e) => server_error(&e),
    }
}

/// Ends the session of an access token Sekisho issued to `client`. A token
/// that fails verification, or has expired, ends nothing.
async fn revoke_access(state: &AuthState, client: &Client, token: &str) -> Result<(), StoreError> {
    let now = state.now();
    let Ok(claims) = verify(
        token,
        |kid| state.cache.public_key(kid),
        &state.issuer(),
        now,
    ) else {
        return Ok(());
    };
    if claims.client_id != client.id {
        return Ok(());
    }
    let Ok(session) = claims.sid.parse::<Uuid>() else {
        return Ok(());
    };
    if state.store.end_session(&session, "revoked", now).await? {
        info!(%session, "a session was revoked by its access token");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(verifier: &str) -> CodeGrant {
        CodeGrant {
            client_id: clients::CLI.to_owned(),
            redirect_uri: "http://127.0.0.1:9/cb".to_owned(),
            code_challenge: s256(verifier),
            user: Uuid::nil(),
        }
    }

    #[test]
    fn a_grant_holds_only_for_its_client_redirect_and_verifier() {
        let verifier = "a".repeat(43);
        let g = grant(&verifier);
        assert!(grant_holds(
            &g,
            clients::CLI,
            "http://127.0.0.1:9/cb",
            &verifier
        ));
        assert!(!grant_holds(
            &g,
            "sekisho-ui",
            "http://127.0.0.1:9/cb",
            &verifier
        ));
        assert!(!grant_holds(
            &g,
            clients::CLI,
            "http://127.0.0.1:10/cb",
            &verifier
        ));
        assert!(!grant_holds(
            &g,
            clients::CLI,
            "http://127.0.0.1:9/cb",
            &"b".repeat(43)
        ));
        // The right challenge for a verifier RFC 7636 doesn't allow.
        for bad in [
            "a".repeat(42),
            "a".repeat(129),
            format!("{}+", "a".repeat(42)),
        ] {
            let g = grant(&bad);
            assert!(
                !grant_holds(&g, clients::CLI, "http://127.0.0.1:9/cb", &bad),
                "{bad}"
            );
        }
        let edge = format!("{}-._~", "a".repeat(124));
        assert!(grant_holds(
            &grant(&edge),
            clients::CLI,
            "http://127.0.0.1:9/cb",
            &edge
        ));
    }
}
