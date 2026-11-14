//! The browser's side of sign-in: RFC 8414 metadata, the signing keys, and
//! the authorisation code flow, whose sign-in happens at Dex.
//!
//! `/oauth/authorize` checks the client and its redirect URI before anything
//! else: until both check out, errors are pages, never redirects. Every
//! redirect back to a client carries `iss` (RFC 9207).
//!
//! Every sign-in at Dex (this flow's, and the device flow's) is bound to the
//! browser that started it: a cookie whose SHA-256 is the Dex `nonce`
//! (OpenID Connect Core section 15.5.2). Someone who forwards Dex's link
//! can't have another person's sign-in finish as their own.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sekisho_core::keys::{hash_secret, random_secret, verify_secret};
use sekisho_store::{
    AUTH_REQUEST_LIFETIME, AuthFlow, AuthRequest, CodeGrant, NewAuthRequest, SignInOutcome,
    StoreError,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::time::timeout;
use tracing::{error, info, warn};
use url::Url;
use uuid::Uuid;

use crate::AuthState;
use crate::clients::{self, AUTHORIZATION_CODE, DEVICE_CODE, REFRESH_TOKEN};
use crate::dex::{DEX_TIMEOUT, DexError};
use crate::revalidate::sign_in_of;
use crate::{device, pages};

/// The longest client `state` Sekisho keeps for a sign-in.
const MAX_CLIENT_STATE: usize = 1024;

/// Form or query parameters (RFC 6749 section 3.1): a parameter given twice
/// is an error, and one with an empty value counts as absent.
pub(crate) struct Params(HashMap<String, String>);

impl Params {
    /// Parses `application/x-www-form-urlencoded`. `Err` names a parameter
    /// given twice.
    pub(crate) fn parse(raw: &[u8]) -> Result<Self, String> {
        let mut params = HashMap::new();
        for (name, value) in url::form_urlencoded::parse(raw) {
            if params
                .insert(name.clone().into_owned(), value.into_owned())
                .is_some()
            {
                return Err(name.into_owned());
            }
        }
        Ok(Self(params))
    }

    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.0
            .get(name)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
}

/// The PKCE `S256` challenge for `verifier` (RFC 7636).
pub(crate) fn s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// An `S256` challenge: 32 bytes in unpadded base64url.
fn is_challenge(s: &str) -> bool {
    s.len() == 43
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// A `302` to `location`, never cached, sending no referrer on.
pub(crate) fn redirect(location: &str) -> Response {
    let mut response = StatusCode::FOUND.into_response();
    let headers = response.headers_mut();
    match HeaderValue::from_str(location) {
        Ok(location) => {
            headers.insert(header::LOCATION, location);
        }
        Err(_) => {
            return pages::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Sign-in failed",
                "Sekisho couldn't build the address to send you on to.",
            );
        }
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

/// Back to the client's `redirect_uri` with `params`, its `state` and `iss`.
fn to_client(
    state: &AuthState,
    redirect_uri: &str,
    params: &[(&str, &str)],
    client_state: Option<&str>,
) -> Response {
    let Ok(mut url) = Url::parse(redirect_uri) else {
        error!("a stored redirect URI doesn't parse");
        return pages::error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Sign-in failed",
            "Sekisho couldn't send you back to your application.",
        );
    };
    {
        let mut query = url.query_pairs_mut();
        query.extend_pairs(params);
        if let Some(s) = client_state {
            query.append_pair("state", s);
        }
        query.append_pair("iss", &state.issuer());
    }
    redirect(url.as_str())
}

fn bad_request(message: &str) -> Response {
    pages::error(StatusCode::BAD_REQUEST, "Sign-in can't start", message)
}

/// RFC 8414 authorisation server metadata.
pub(crate) async fn metadata(State(state): State<AuthState>) -> Response {
    Json(json!({
        "issuer": state.issuer(),
        "authorization_endpoint": state.url("/oauth/authorize"),
        "token_endpoint": state.url("/oauth/token"),
        "device_authorization_endpoint": state.url("/oauth/device_authorization"),
        "revocation_endpoint": state.url("/oauth/revoke"),
        "jwks_uri": state.url("/oauth/jwks"),
        "response_types_supported": ["code"],
        "grant_types_supported": [AUTHORIZATION_CODE, REFRESH_TOKEN, DEVICE_CODE],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "revocation_endpoint_auth_methods_supported": ["none"],
        "authorization_response_iss_parameter_supported": true,
    }))
    .into_response()
}

/// The keys session tokens are signed with, including one about to sign
/// and retired ones whose tokens may not have expired yet.
pub(crate) async fn jwks(State(state): State<AuthState>) -> Response {
    match state.store.published_keys(state.now()).await {
        Ok(keys) => {
            let keys: Vec<_> = keys.iter().map(|k| k.to_jwk()).collect();
            (
                [(header::CONTENT_TYPE, "application/jwk-set+json")],
                json!({ "keys": keys }).to_string(),
            )
                .into_response()
        }
        Err(e) => {
            error!(error = %e, "couldn't read the signing keys");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// The authorisation endpoint: checks the request, keeps it, and sends the
/// browser to Dex.
pub(crate) async fn authorize(
    State(state): State<AuthState>,
    RawQuery(query): RawQuery,
) -> Response {
    let params = match Params::parse(query.unwrap_or_default().as_bytes()) {
        Ok(params) => params,
        Err(name) => {
            return bad_request(&format!(
                "The parameter {name} was given twice. Start the sign-in again from your \
                 application."
            ));
        }
    };
    let Some(client) = params.get("client_id").and_then(clients::find) else {
        return bad_request("The application that sent you here isn't registered with Sekisho.");
    };
    let Some(redirect_uri) = params
        .get("redirect_uri")
        .filter(|uri| clients::redirect_allowed(client, uri))
    else {
        return bad_request(
            "The application asked to be sent the sign-in at an address Sekisho doesn't allow.",
        );
    };
    let client_state = params.get("state");
    let deny = |error: &str, description: &str| {
        to_client(
            &state,
            redirect_uri,
            &[("error", error), ("error_description", description)],
            client_state,
        )
    };
    if !client.allows(AUTHORIZATION_CODE) {
        return deny(
            "unauthorized_client",
            "this client may not use the authorization code flow",
        );
    }
    if params.get("response_type") != Some("code") {
        return deny(
            "unsupported_response_type",
            "only response_type=code is supported",
        );
    }
    let Some(code_challenge) = params.get("code_challenge").filter(|c| is_challenge(c)) else {
        return deny("invalid_request", "PKCE with S256 is required");
    };
    if params.get("code_challenge_method") != Some("S256") {
        return deny("invalid_request", "PKCE with S256 is required");
    }
    if client_state.is_some_and(|s| s.len() > MAX_CLIENT_STATE) {
        return deny("invalid_request", "state is too long");
    }

    let flow = AuthFlow::Code {
        redirect_uri: redirect_uri.to_owned(),
        client_state: client_state.map(str::to_owned),
        code_challenge: code_challenge.to_owned(),
    };
    match to_dex(&state, client.id, flow).await {
        Ok(response) => response,
        Err(e) => {
            error!(error = %e, "couldn't keep a sign-in request");
            deny("server_error", "Sekisho couldn't start the sign-in")
        }
    }
}

/// The cookie that binds a sign-in to its browser, named after its Dex
/// `state` so sign-ins in several tabs don't clash.
fn binding_cookie(state: &AuthState, dex_state: &str) -> String {
    let id: String = hash_secret(dex_state)[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    pages::cookie_name(&format!("sekisho_signin_{id}"), state.https())
}

/// Starts the sign-in at Dex for `flow`: keeps the request, binds it to this
/// browser, and sends the browser to Dex with Sekisho's own PKCE and nonce.
pub(crate) async fn to_dex(
    state: &AuthState,
    client_id: &str,
    flow: AuthFlow,
) -> Result<Response, StoreError> {
    let binding = random_secret();
    let nonce = s256(&binding);
    let dex_verifier = random_secret();
    let request = NewAuthRequest {
        client_id: client_id.to_owned(),
        flow,
        dex_verifier: dex_verifier.clone(),
        nonce: nonce.clone(),
    };
    let dex_state = state
        .store
        .save_auth_request(&state.secret_box, request, state.now())
        .await?;
    let mut response = redirect(
        state
            .dex
            .authorize_url(&dex_state, &nonce, &s256(&dex_verifier))
            .as_str(),
    );
    // Lax: Dex sends the browser back with a top-level GET from its own site.
    response.headers_mut().append(
        header::SET_COOKIE,
        pages::set_cookie(
            &binding_cookie(state, &dex_state),
            &binding,
            "Lax",
            AUTH_REQUEST_LIFETIME.whole_seconds(),
            state.https(),
        ),
    );
    Ok(response)
}

/// Whether this browser holds the cookie the sign-in with this `nonce` set.
fn bound(headers: &HeaderMap, cookie: &str, nonce: &str) -> bool {
    let Some(binding) = pages::cookie(headers, cookie) else {
        return false;
    };
    let Some(expected) = URL_SAFE_NO_PAD
        .decode(nonce)
        .ok()
        .and_then(|n| <[u8; 32]>::try_from(n).ok())
    else {
        return false;
    };
    verify_secret(binding, &expected)
}

/// Dex's answer to a sign-in.
pub(crate) async fn callback(
    State(state): State<AuthState>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    let Ok(params) = Params::parse(query.unwrap_or_default().as_bytes()) else {
        return bad_request("The answer from the sign-in page was malformed. Start again.");
    };
    let Some(dex_state) = params.get("state") else {
        return bad_request("The answer from the sign-in page was incomplete. Start again.");
    };
    // Taken first: a forwarded link is used up whoever follows it.
    let request = match state
        .store
        .take_auth_request(&state.secret_box, dex_state, state.now())
        .await
    {
        Ok(Some(request)) => request,
        Ok(None) => {
            return bad_request(
                "This sign-in has expired or was already used. Start again from your application.",
            );
        }
        Err(e) => {
            error!(error = %e, "couldn't read a sign-in request");
            return pages::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Sign-in failed",
                "Sekisho couldn't finish the sign-in. Try again.",
            );
        }
    };
    let cookie = binding_cookie(&state, dex_state);
    let mut response = finish(&state, &headers, &cookie, &params, request).await;
    response.headers_mut().append(
        header::SET_COOKIE,
        pages::set_cookie(&cookie, "", "Lax", 0, state.https()),
    );
    response
}

/// The rest of the callback, once the sign-in request is taken.
async fn finish(
    state: &AuthState,
    headers: &HeaderMap,
    cookie: &str,
    params: &Params,
    request: AuthRequest,
) -> Response {
    if !bound(headers, cookie, &request.nonce) {
        warn!(client = %request.client_id, "a sign-in came back to a browser that didn't start it");
        return bad_request(
            "This sign-in was started in another browser, or its link was forwarded. Start \
             again from your application or terminal, in this browser.",
        );
    }
    let dex_code = if params.get("error").is_some() {
        info!("Dex answered a sign-in with an error");
        None
    } else {
        params.get("code")
    };
    let (redirect_uri, client_state, code_challenge) = match request.flow {
        AuthFlow::Code {
            redirect_uri,
            client_state,
            code_challenge,
        } => (redirect_uri, client_state, code_challenge),
        AuthFlow::Device(found) => {
            return device::finish(state, dex_code, request.dex_verifier, request.nonce, found)
                .await;
        }
    };
    let back =
        |params: &[(&str, &str)]| to_client(state, &redirect_uri, params, client_state.as_deref());
    let Some(dex_code) = dex_code else {
        return back(&[("error", "access_denied")]);
    };
    // Dex keeps one refresh token per person, and this exchange replaces it:
    // finish on a task the browser can't cancel, so the new token is kept.
    let finish = {
        let state = state.clone();
        let dex_code = dex_code.to_owned();
        let (client_id, redirect_uri) = (request.client_id, redirect_uri.clone());
        let (dex_verifier, nonce) = (request.dex_verifier, request.nonce);
        tokio::spawn(async move {
            let user = finish_sign_in(&state, &dex_code, &dex_verifier, &nonce).await?;
            let grant = CodeGrant {
                client_id,
                redirect_uri,
                code_challenge,
                user,
            };
            state
                .store
                .save_code(grant, state.now())
                .await
                .map_err(|e| {
                    error!(%user, error = %e, "couldn't make an authorisation code");
                    "server_error"
                })
        })
    };
    match finish.await {
        Ok(Ok(code)) => back(&[("code", &code)]),
        Ok(Err(error)) => back(&[("error", error)]),
        Err(e) => {
            error!(error = %e, "a sign-in failed");
            back(&[("error", "server_error")])
        }
    }
}

/// Exchanges Dex's code, and records the person and their Dex token. `Err`
/// is the OAuth error for the client. Run it on a task the browser can't
/// cancel: Dex has replaced the person's refresh token by the time it
/// answers.
pub(crate) async fn finish_sign_in(
    state: &AuthState,
    dex_code: &str,
    dex_verifier: &str,
    nonce: &str,
) -> Result<Uuid, &'static str> {
    let identity = match timeout(
        DEX_TIMEOUT,
        state.dex.sign_in(dex_code, dex_verifier, nonce),
    )
    .await
    {
        Ok(Ok(identity)) => identity,
        Ok(Err(e @ (DexError::Refused(_) | DexError::Invalid(_)))) => {
            warn!(error = %e, "Dex's sign-in was refused");
            return Err("access_denied");
        }
        Ok(Err(e @ (DexError::Unavailable(_) | DexError::Unreachable(_)))) => {
            warn!(error = %e, "Dex couldn't finish a sign-in");
            return Err("temporarily_unavailable");
        }
        Err(_) => {
            warn!("Dex didn't finish a sign-in within {DEX_TIMEOUT:?}");
            return Err("temporarily_unavailable");
        }
    };
    let Some(dex_token) = identity.refresh_token.as_deref() else {
        warn!("Dex signed someone in without a refresh token");
        return Err("access_denied");
    };
    let now = state.now();
    let user = match state.store.sign_in(&sign_in_of(&identity), now).await {
        Ok(SignInOutcome::User(user)) => user,
        Ok(SignInOutcome::Disabled) => {
            info!("a disabled user tried to sign in");
            return Err("access_denied");
        }
        Ok(SignInOutcome::UnknownLocalAccount) => {
            info!("Dex signed in a local account Sekisho doesn't know");
            return Err("access_denied");
        }
        Err(e) => {
            error!(error = %e, "couldn't record a sign-in");
            return Err("server_error");
        }
    };
    if let Err(e) = state
        .store
        .save_dex_token(&state.secret_box, user.id, dex_token)
        .await
    {
        error!(user = %user.id, error = %e, "couldn't keep the Dex token of a sign-in");
        return Err("server_error");
    }
    info!(user = %user.id, "signed in at Dex");
    Ok(user.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_refuse_repeats_and_drop_empty_values() {
        let p = Params::parse(b"a=1&b=&c=%20x").unwrap();
        assert_eq!(p.get("a"), Some("1"));
        assert_eq!(p.get("b"), None);
        assert_eq!(p.get("c"), Some(" x"));
        assert_eq!(p.get("d"), None);
        assert_eq!(Params::parse(b"a=1&a=2").err().as_deref(), Some("a"));
    }

    #[test]
    fn s256_matches_rfc_7636_appendix_b() {
        assert_eq!(
            s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(is_challenge("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
        assert!(!is_challenge("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-c"));
        assert!(!is_challenge("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw+cM"));
    }
}
