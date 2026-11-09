//! Turning an `Authorization` value into a known key, the same way in every
//! hook.

use sekisho_core::keys::{KeyState, effective_state, parse_token, verify_secret};
use time::OffsetDateTime;

use crate::cache::{AuthCache, CachedKey};

pub(crate) enum Credential {
    /// Not `Bearer sek_…`.
    Malformed,
    /// A well-formed token whose key is unknown or whose secret is wrong:
    /// the two look the same, so a wrong secret reveals nothing.
    Unknown,
    Known(CachedKey),
}

/// Resolves an `Authorization` header value (`Bearer <token>`, any case of
/// `Bearer`).
pub(crate) fn resolve(cache: &AuthCache, authorization: Option<&str>) -> Credential {
    let Some((id, secret)) = authorization
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .and_then(|(_, token)| parse_token(token))
    else {
        return Credential::Malformed;
    };
    match cache.key(&id) {
        Some(key) if verify_secret(&secret, &key.secret_sha256) => Credential::Known(key),
        _ => Credential::Unknown,
    }
}

/// Why the key may not be used at `now`, if it may not.
pub(crate) fn refusal(key: &CachedKey, now: OffsetDateTime) -> Option<&'static str> {
    match effective_state(
        key.revoked.then_some(now),
        key.suspended,
        key.expires_at,
        now,
    ) {
        KeyState::Active => None,
        KeyState::Revoked => Some("revoked"),
        KeyState::Expired => Some("expired"),
        KeyState::Suspended => Some("suspended"),
    }
}
