//! Turning an `Authorization` value into a key or a session, the same way in
//! every hook.
//!
//! A `Bearer sek_…` token is an environment key, checked against the cache.
//! A `Bearer` JWT is a session token: its signature is checked against
//! Sekisho's published keys before anything else, so a forged token never
//! costs a database read; then its session must not have ended and its user
//! must be enabled.

use sekisho_core::keys::{KeyState, effective_state, parse_token, verify_secret};
use sekisho_core::session_token::{TokenError, looks_like_jwt, verify};
use sekisho_store::Store;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::cache::{AuthCache, CachedKey, CachedUser};

/// Who a request comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Authenticated {
    /// An environment key, and the person who owns it.
    Key {
        key: CachedKey,
        owner: CachedUser,
    },
    /// A signed-in person's session.
    Session {
        user: CachedUser,
        session: Uuid,
    },
    Refused(Refusal),
}

/// Why a credential was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Neither a `sek_` key nor a session token.
    Malformed,
    /// A well-formed key that is unknown or whose secret is wrong: the two
    /// look the same, so a wrong secret reveals nothing.
    UnknownKey,
    /// A session token that fails its checks.
    InvalidToken,
    TokenExpired,
    /// The session ended, or its user is disabled or unknown.
    SessionEnded,
    /// The database couldn't be read for a user the cache doesn't have yet.
    Unavailable,
}

impl Refusal {
    /// The `reason` hooks answer with.
    pub fn reason(self) -> &'static str {
        match self {
            Refusal::Malformed => "unauthorized",
            Refusal::UnknownKey => "unknown_key",
            Refusal::InvalidToken => "invalid_token",
            Refusal::TokenExpired => "token_expired",
            Refusal::SessionEnded => "session_ended",
            Refusal::Unavailable => "unavailable",
        }
    }
}

/// The token of an `Authorization` value (`Bearer <token>`, any case of
/// `Bearer`).
fn bearer(authorization: Option<&str>) -> Option<&str> {
    authorization
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token)
}

/// Who an `Authorization` header value names, at `now`.
pub async fn authenticate(
    cache: &AuthCache,
    store: &Store,
    authorization: Option<&str>,
    now: OffsetDateTime,
) -> Authenticated {
    let Some(token) = bearer(authorization) else {
        return Authenticated::Refused(Refusal::Malformed);
    };
    if let Some((id, secret)) = parse_token(token) {
        let Some(key) = cache
            .key(&id)
            .filter(|key| verify_secret(&secret, &key.secret_sha256))
        else {
            return Authenticated::Refused(Refusal::UnknownKey);
        };
        return match cache.user_or_load(store, &key.owner).await {
            Ok(Some(owner)) => Authenticated::Key { key, owner },
            Ok(None) => Authenticated::Refused(Refusal::UnknownKey),
            Err(error) => {
                tracing::warn!(%error, "reading a key's owner failed");
                Authenticated::Refused(Refusal::Unavailable)
            }
        };
    }
    if !looks_like_jwt(token) {
        return Authenticated::Refused(Refusal::Malformed);
    }
    let claims = match verify(token, |kid| cache.public_key(kid), cache.issuer(), now) {
        Ok(claims) => claims,
        Err(TokenError::Expired) => return Authenticated::Refused(Refusal::TokenExpired),
        Err(_) => return Authenticated::Refused(Refusal::InvalidToken),
    };
    let (Ok(user), Ok(session)) = (Uuid::parse_str(&claims.sub), Uuid::parse_str(&claims.sid))
    else {
        return Authenticated::Refused(Refusal::InvalidToken);
    };
    if cache.session_ended(&session) {
        return Authenticated::Refused(Refusal::SessionEnded);
    }
    match cache.user_or_load(store, &user).await {
        Ok(Some(user)) if !user.access.disabled => Authenticated::Session { user, session },
        Ok(_) => Authenticated::Refused(Refusal::SessionEnded),
        Err(error) => {
            tracing::warn!(%error, "reading a session's user failed");
            Authenticated::Refused(Refusal::Unavailable)
        }
    }
}

/// Why the key may not be used at `now` by its own state, if it may not.
/// Its owner's access is checked separately.
pub(crate) fn refusal(key: &CachedKey, now: OffsetDateTime) -> Option<&'static str> {
    match effective_state(
        key.revoked.then_some(now),
        key.suspended,
        key.expires_at,
        now,
    ) {
        KeyState::Active => None,
        KeyState::Invalid => Some("invalid"),
        KeyState::Revoked => Some("revoked"),
        KeyState::Expired => Some("expired"),
        KeyState::Suspended => Some("suspended"),
    }
}
