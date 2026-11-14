//! The RFC 8628 device flow, for machines without a browser
//! (`sekisho login --no-browser`).
//!
//! The device shows a code; the person types it on Sekisho's device page in
//! any browser, checks the confirmation page (which names the client), and
//! signs in at Dex. RFC 8628 section 5.4's two attacks are closed: the
//! confirmation form refuses a post from another site (its token is in a
//! `SameSite=Strict` cookie), and the sign-in at Dex is bound to the browser
//! that pressed Continue, so a forwarded Dex link approves nothing.

use axum::body::Bytes;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use sekisho_core::keys::{hash_secret, random_secret, verify_secret};
use sekisho_store::{DEVICE_CODE_LIFETIME, Device, DevicePoll, StoreError};
use serde_json::json;
use tracing::{error, info};

use crate::AuthState;
use crate::clients::{self, Client, DEVICE_CODE};
use crate::oauth::{Params, finish_sign_in, to_dex};
use crate::pages::{self, escape};
use crate::tokens::{
    form as form_params, invalid_grant, invalid_request, no_store, oauth_error, server_error,
    signing_key, tokens,
};

const TITLE: &str = "Sign in a device";

/// The confirmation form's token against cross-site posts, as a cookie.
fn csrf_cookie(state: &AuthState) -> String {
    pages::cookie_name("sekisho_device", state.https())
}

fn database_failed(e: &StoreError) -> Response {
    error!(error = %e, "the store failed on the device pages");
    pages::error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Something went wrong",
        "Sekisho couldn't finish this step. Try again.",
    )
}

/// The page for a code that is wrong, expired or already used.
fn unknown_code(state: &AuthState) -> Response {
    pages::page(
        StatusCode::BAD_REQUEST,
        "That code didn't work",
        &format!(
            "<p>The code is wrong, has expired, or was already used. Check the code your \
             terminal shows, or start again there.</p>\n<p><a href=\"{}\">Enter a code</a></p>",
            escape(&state.url("/oauth/device"))
        ),
    )
}

/// `POST /oauth/device_authorization`: a device code for a client with the
/// device grant.
pub(crate) async fn device_authorization(
    State(state): State<AuthState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let params = match form_params(&headers, &body) {
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
    if !client.allows(DEVICE_CODE) {
        return oauth_error(StatusCode::BAD_REQUEST, "unauthorized_client", None);
    }
    let started = match state.store.create_device(client.id, state.now()).await {
        Ok(started) => started,
        Err(e) => return server_error(&e),
    };
    let verification_uri = state.url("/oauth/device");
    no_store(
        StatusCode::OK,
        json!({
            "device_code": started.device_code,
            "user_code": started.user_code,
            "verification_uri_complete":
                format!("{verification_uri}?user_code={}", started.user_code),
            "verification_uri": verification_uri,
            "expires_in": started.expires_in,
            "interval": started.interval,
        }),
    )
}

/// `GET /oauth/device[?user_code=]`: the form for the code, filled in from
/// the link the terminal showed. Nothing happens until the person submits
/// it and confirms (RFC 8628 section 5.4).
pub(crate) async fn form(State(state): State<AuthState>, RawQuery(query): RawQuery) -> Response {
    let params = Params::parse(query.unwrap_or_default().as_bytes()).ok();
    let typed = params
        .as_ref()
        .and_then(|p| p.get("user_code"))
        .unwrap_or("");
    pages::page(
        StatusCode::OK,
        TITLE,
        &format!(
            "<p>Enter the code your terminal shows.</p>\n\
             <form method=\"post\" action=\"{action}\">\n\
             <p><label for=\"user_code\">Code</label><br>\
             <input id=\"user_code\" name=\"user_code\" value=\"{typed}\" autocomplete=\"off\" \
             autocapitalize=\"characters\" spellcheck=\"false\" required></p>\n\
             <p><button type=\"submit\">Next</button></p>\n</form>",
            action = escape(&state.url("/oauth/device")),
            typed = escape(typed),
        ),
    )
}

/// `POST /oauth/device`: the confirmation page for the typed code, naming
/// the client that asked.
pub(crate) async fn confirmation(
    State(state): State<AuthState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(params) = form_params(&headers, &body) else {
        return unknown_code(&state);
    };
    let device = match state
        .store
        .device_by_user_code(params.get("user_code").unwrap_or(""), state.now())
        .await
    {
        Ok(Some(device)) => device,
        Ok(None) => return unknown_code(&state),
        Err(e) => return database_failed(&e),
    };
    let client = clients::find(&device.client_id).map_or("An unregistered application", |c| c.name);
    let token = random_secret();
    let mut response = pages::page(
        StatusCode::OK,
        TITLE,
        &format!(
            "<p><strong>{client}</strong> asks to sign in as you with the code \
             <strong>{code}</strong>.</p>\n\
             <p>Continue only if you started this sign-in yourself, and the code matches the one \
             your terminal shows. Someone who sent you this code is trying to sign in as \
             you.</p>\n\
             <form method=\"post\" action=\"{action}\">\n\
             <input type=\"hidden\" name=\"user_code\" value=\"{code}\">\n\
             <input type=\"hidden\" name=\"csrf\" value=\"{token}\">\n\
             <p><button type=\"submit\" name=\"decision\" value=\"continue\">Continue</button> \
             <button type=\"submit\" name=\"decision\" value=\"deny\">Deny</button></p>\n</form>",
            client = escape(client),
            code = escape(&device.user_code),
            action = escape(&state.url("/oauth/device/confirm")),
            token = escape(&token),
        ),
    );
    // Strict: a post from any other site comes without it.
    response.headers_mut().append(
        header::SET_COOKIE,
        pages::set_cookie(
            &csrf_cookie(&state),
            &token,
            "Strict",
            DEVICE_CODE_LIFETIME.whole_seconds(),
            state.https(),
        ),
    );
    response
}

/// Whether the form's token is the one this browser's cookie holds.
fn same_form(state: &AuthState, headers: &HeaderMap, params: &Params) -> bool {
    match (
        pages::cookie(headers, &csrf_cookie(state)),
        params.get("csrf"),
    ) {
        (Some(cookie), Some(field)) => verify_secret(field, &hash_secret(cookie)),
        _ => false,
    }
}

/// `POST /oauth/device/confirm`: *Deny* denies the device; *Continue* starts
/// the sign-in at Dex, bound to this browser.
pub(crate) async fn decide(
    State(state): State<AuthState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(params) = form_params(&headers, &body) else {
        return unknown_code(&state);
    };
    if !same_form(&state, &headers, &params) {
        info!("a device confirmation came without its own form's token");
        return pages::error(
            StatusCode::FORBIDDEN,
            "This form has expired",
            "This page only accepts its own form. Open the code page again from your terminal.",
        );
    }
    let mut response = decided(&state, &params).await;
    response.headers_mut().append(
        header::SET_COOKIE,
        pages::set_cookie(&csrf_cookie(&state), "", "Strict", 0, state.https()),
    );
    response
}

async fn decided(state: &AuthState, params: &Params) -> Response {
    let device = match state
        .store
        .device_by_user_code(params.get("user_code").unwrap_or(""), state.now())
        .await
    {
        Ok(Some(device)) => device,
        Ok(None) => return unknown_code(state),
        Err(e) => return database_failed(&e),
    };
    match params.get("decision") {
        Some("deny") => match state.store.deny_device(&device).await {
            Ok(true) => cancelled(),
            Ok(false) => unknown_code(state),
            Err(e) => database_failed(&e),
        },
        Some("continue") => {
            let client_id = device.client_id.clone();
            match to_dex(state, &client_id, sekisho_store::AuthFlow::Device(device)).await {
                Ok(response) => response,
                Err(e) => database_failed(&e),
            }
        }
        _ => pages::error(
            StatusCode::BAD_REQUEST,
            TITLE,
            "Choose Continue or Deny. Open the code page again from your terminal.",
        ),
    }
}

fn cancelled() -> Response {
    pages::page(
        StatusCode::OK,
        "Sign-in cancelled",
        "<p>Nothing was granted. You can close this tab.</p>",
    )
}

/// The callback for a device: Dex's code (`None` when Dex answered with an
/// error) exchanged, and the device approved, on a task the browser can't
/// cancel.
pub(crate) async fn finish(
    state: &AuthState,
    dex_code: Option<&str>,
    dex_verifier: String,
    nonce: String,
    device: Device,
) -> Response {
    if device.expires_at <= state.now() {
        return unknown_code(state);
    }
    let Some(dex_code) = dex_code else {
        return match state.store.deny_device(&device).await {
            Ok(_) => cancelled(),
            Err(e) => database_failed(&e),
        };
    };
    let outcome = {
        let state = state.clone();
        let dex_code = dex_code.to_owned();
        tokio::spawn(async move {
            match finish_sign_in(&state, &dex_code, &dex_verifier, &nonce).await {
                Ok(user) => state
                    .store
                    .approve_device(&device, user)
                    .await
                    .map(|approved| (approved, user))
                    .map_err(|e| {
                        error!(%user, error = %e, "couldn't approve a device");
                        "server_error"
                    }),
                Err("access_denied") => {
                    state.store.deny_device(&device).await.map_err(|e| {
                        error!(error = %e, "couldn't deny a device");
                        "server_error"
                    })?;
                    Err("access_denied")
                }
                Err(other) => Err(other),
            }
        })
        .await
    };
    match outcome {
        Ok(Ok((true, user))) => {
            info!(%user, "a device was approved");
            pages::page(
                StatusCode::OK,
                "Signed in",
                "<p>Signed in. Return to your terminal.</p>\n<p>You can close this tab.</p>",
            )
        }
        Ok(Ok((false, _))) => unknown_code(state),
        Ok(Err("access_denied")) => pages::error(
            StatusCode::FORBIDDEN,
            "Sign-in refused",
            "Sekisho refused this sign-in, so the device got nothing.",
        ),
        Ok(Err("temporarily_unavailable")) => pages::error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Try again",
            "The sign-in service can't be reached right now. Enter the code again in a minute.",
        ),
        Ok(Err(_)) | Err(_) => pages::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Something went wrong",
            "Sekisho couldn't finish the sign-in. Enter the code again.",
        ),
    }
}

/// The token endpoint's device grant (RFC 8628 section 3.4).
pub(crate) async fn redeem(state: &AuthState, client: &Client, params: &Params) -> Response {
    let Some(device_code) = params.get("device_code") else {
        return invalid_request("device_code is missing");
    };
    let now = state.now();
    // The key first: an approved device's tokens are issued only once.
    let key = match signing_key(state, now).await {
        Ok(key) => key,
        Err(no_key) => return no_key.answer(),
    };
    let user = match state.store.poll_device(device_code, client.id, now).await {
        Ok(DevicePoll::Approved(user)) => user,
        Ok(DevicePoll::Pending) => {
            return oauth_error(StatusCode::BAD_REQUEST, "authorization_pending", None);
        }
        Ok(DevicePoll::SlowDown) => {
            return oauth_error(StatusCode::BAD_REQUEST, "slow_down", None);
        }
        Ok(DevicePoll::Denied) => {
            return oauth_error(StatusCode::BAD_REQUEST, "access_denied", None);
        }
        Ok(DevicePoll::Expired) => {
            return oauth_error(StatusCode::BAD_REQUEST, "expired_token", None);
        }
        Ok(DevicePoll::Unknown) => return invalid_grant(),
        Err(e) => return server_error(&e),
    };
    match state.store.user(&user).await {
        Ok(found) if found.disabled_at.is_none() => {}
        Ok(_) | Err(StoreError::NotFound) => return invalid_grant(),
        Err(e) => return server_error(&e),
    }
    let (session, refresh_token) = match state.store.create_session(user, client.id, now).await {
        Ok(created) => created,
        Err(e) => return server_error(&e),
    };
    info!(%user, session = %session.id, "opened a session for a device");
    tokens(state, &key, user, session.id, client, &refresh_token, now)
}
