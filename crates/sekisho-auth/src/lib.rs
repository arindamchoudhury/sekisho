//! Sekisho's sign-in. Sekisho is the OAuth 2.1 server for its own clients;
//! Dex, reached through [`dex`], is its only upstream identity provider.
//!
//! [`router`] serves RFC 8414 metadata, the signing keys, the authorisation
//! code flow with PKCE (every sign-in goes through Dex), the token endpoint
//! (codes and refresh tokens, each refresh re-checked at Dex) and RFC 7009
//! revocation. [`sweep`] re-checks the owners of environment keys at Dex
//! once a day.

pub mod clients;
pub mod dex;
mod oauth;
pub mod pages;
pub mod revalidate;
pub mod sweep;
mod tokens;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use sekisho_core::secret_box::SecretBox;
use sekisho_hooks::cache::AuthCache;
use sekisho_store::Store;
use time::OffsetDateTime;
use url::Url;

use crate::dex::Dex;

/// What the sign-in routes share.
#[derive(Clone)]
pub struct AuthState {
    pub store: Store,
    pub secret_box: SecretBox,
    pub dex: Arc<dyn Dex>,
    /// The hooks' cache: Sekisho's published signing keys.
    pub cache: Arc<AuthCache>,
    /// `SEKISHO_PUBLIC_URL`: the issuer, and where browsers reach Sekisho.
    pub public_url: Url,
    /// SHA-256 of `SEKISHO_BOOTSTRAP_TOKEN`, for `/setup`.
    pub bootstrap_sha256: [u8; 32],
    pub clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
}

impl AuthState {
    /// Sekisho's issuer: `SEKISHO_PUBLIC_URL` without a trailing slash. Every
    /// session token's `iss`, and the `iss` of every authorisation response.
    pub fn issuer(&self) -> String {
        self.public_url.as_str().trim_end_matches('/').to_owned()
    }

    /// The public URL of one of Sekisho's paths, such as `/oauth/token`.
    pub(crate) fn url(&self, path: &str) -> String {
        format!("{}{path}", self.issuer())
    }

    pub(crate) fn now(&self) -> OffsetDateTime {
        (self.clock)()
    }
}

/// The sign-in routes.
pub fn router(state: AuthState) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(oauth::metadata),
        )
        .route("/oauth/jwks", get(oauth::jwks))
        .route("/oauth/authorize", get(oauth::authorize))
        .route("/oauth/callback", get(oauth::callback))
        .route("/oauth/token", post(tokens::token))
        .route("/oauth/revoke", post(tokens::revoke))
        .with_state(state)
}
