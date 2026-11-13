//! Environment keys: `sek_<id>_<secret>` tokens, their hashes and states;
//! and random secrets for other tokens.
//!
//! The id is public and indexes the key. Only the SHA-256 of the secret is
//! stored; the token is shown to its owner once.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::TryRng;
use rand::rngs::SysRng;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

const PREFIX: &str = "sek_";
const ID_LEN: usize = 12;
const SECRET_BYTES: usize = 32;
/// Base64url without padding of 32 bytes.
const SECRET_LEN: usize = 43;
const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// An environment key's public id: 12 lowercase base32 characters (60 bits).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EnvKeyId(String);

impl EnvKeyId {
    pub fn parse(s: &str) -> Option<Self> {
        (s.len() == ID_LEN && s.bytes().all(|c| BASE32.contains(&c))).then(|| Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EnvKeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A newly made key. Give `token` to the owner once and store only
/// `secret_sha256`.
pub struct GeneratedKey {
    pub id: EnvKeyId,
    pub token: String,
    pub secret_sha256: [u8; 32],
}

/// Shows the id only, so a stray log line never carries the token.
impl fmt::Debug for GeneratedKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeneratedKey")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

pub(crate) fn os_random<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    SysRng
        .try_fill_bytes(&mut buf)
        .expect("the operating system's random number generator failed");
    buf
}

/// Makes a key with a random id and a 32-byte random secret, both from the OS.
pub fn generate_key() -> GeneratedKey {
    // 256 is a multiple of 32, so masking each byte keeps every letter equally likely.
    let id: String = os_random::<ID_LEN>()
        .iter()
        .map(|b| char::from(BASE32[usize::from(b & 31)]))
        .collect();
    let secret = URL_SAFE_NO_PAD.encode(os_random::<SECRET_BYTES>());
    GeneratedKey {
        token: format!("{PREFIX}{id}_{secret}"),
        id: EnvKeyId(id),
        secret_sha256: hash_secret(&secret),
    }
}

/// `N` random bytes from the OS, such as for device user codes.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    os_random()
}

/// 32 random bytes from the OS, base64url without padding: the secret part of
/// refresh tokens, authorisation codes and setup links.
pub fn random_secret() -> String {
    URL_SAFE_NO_PAD.encode(os_random::<SECRET_BYTES>())
}

/// Splits `sek_<id>_<secret>` into its id and secret. Anything else, including
/// a `Bearer ` prefix or surrounding whitespace, is `None`. The secret may
/// itself contain `_`, so the split is by position, not by separator.
pub fn parse_token(token: &str) -> Option<(EnvKeyId, String)> {
    let rest = token.strip_prefix(PREFIX)?;
    let (id, rest) = rest.split_at_checked(ID_LEN)?;
    let secret = rest.strip_prefix('_')?;
    let id = EnvKeyId::parse(id)?;
    let base64url = |c: u8| c.is_ascii_alphanumeric() || c == b'-' || c == b'_';
    (secret.len() == SECRET_LEN && secret.bytes().all(base64url)).then(|| (id, secret.to_owned()))
}

pub fn hash_secret(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

/// Compares in constant time, so response timing reveals nothing about the hash.
pub fn verify_secret(secret: &str, expected: &[u8; 32]) -> bool {
    let actual = hash_secret(secret);
    let diff = actual
        .iter()
        .zip(expected)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    std::hint::black_box(diff) == 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyState {
    Active,
    /// The owner may not use the environment (disabled, refused at the
    /// identity provider, or outside its access groups). The key works again
    /// once access returns.
    Invalid,
    Suspended,
    Expired,
    Revoked,
}

/// A key's state at `now` from its own fields, by priority Revoked → Expired →
/// Suspended → Active; [`with_access`] then adds the owner's access.
/// A key is expired from its expiry instant on. Any revocation counts, even
/// one stamped slightly after `now` by another replica's clock.
pub fn effective_state(
    revoked_at: Option<OffsetDateTime>,
    suspended: bool,
    expires_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> KeyState {
    if revoked_at.is_some() {
        KeyState::Revoked
    } else if expires_at.is_some_and(|at| at <= now) {
        KeyState::Expired
    } else if suspended {
        KeyState::Suspended
    } else {
        KeyState::Active
    }
}

/// A key's state once its owner's access is known: an active key whose owner
/// may not use the environment is `Invalid`. Revoked, expired and suspended
/// rank above invalid, so they stay as they are.
pub fn with_access(state: KeyState, owner_can_use: bool) -> KeyState {
    match state {
        KeyState::Active if !owner_can_use => KeyState::Invalid,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Duration;
    use time::macros::datetime;

    const ID: &str = "abcdefghijk2"; // 12 chars of [a-z2-7]
    const SECRET: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"; // 43 chars

    #[test]
    fn token_round_trip() {
        let k = generate_key();
        assert!(k.token.starts_with("sek_"));
        let (id, secret) = parse_token(&k.token).unwrap();
        assert_eq!(id, k.id);
        assert!(verify_secret(&secret, &k.secret_sha256));
        assert!(!verify_secret("wrong", &k.secret_sha256));
    }

    #[test]
    fn generated_key_shape() {
        let k = generate_key();
        let id = k.id.as_str();
        assert_eq!(id.len(), 12);
        assert!(
            id.bytes().all(|c| matches!(c, b'a'..=b'z' | b'2'..=b'7')),
            "{id}"
        );
        let secret = k.token.strip_prefix(&format!("sek_{id}_")).unwrap();
        assert_eq!(URL_SAFE_NO_PAD.decode(secret).unwrap().len(), 32);
        assert_eq!(k.secret_sha256, hash_secret(secret));
        assert_eq!(k.token.len(), 4 + 12 + 1 + 43);
        let other = generate_key();
        assert_ne!(other.id, k.id);
        assert_ne!(other.token, k.token);
    }

    #[test]
    fn generated_key_debug_hides_the_token() {
        let k = generate_key();
        let (_, secret) = parse_token(&k.token).unwrap();
        let shown = format!("{k:?}");
        assert!(
            shown.contains(k.id.as_str()) && !shown.contains(&secret),
            "{shown}"
        );
    }

    #[test]
    fn parse_rejects_garbage() {
        for t in [
            "",
            "sek_",
            "Bearer sek_x",
            "sek_short_secret!",
            "ek_abcdefghijkl_x",
        ] {
            assert!(parse_token(t).is_none(), "{t}");
        }
    }

    #[test]
    fn parse_is_strict_about_shape() {
        let ok = format!("sek_{ID}_{SECRET}");
        assert!(parse_token(&ok).is_some());
        for bad in [
            format!("Bearer {ok}"),
            format!("{ok} "),
            format!("{ok}\n"),
            format!("SEK_{ID}_{SECRET}"),
            format!("sek_{}_{SECRET}", ID.to_uppercase()),
            format!("sek_abcdefghijk1_{SECRET}"), // 1 is not base32
            format!("sek_abcdefghijk_{SECRET}"),  // 11-char id
            format!("sek_{ID}{SECRET}"),          // missing separator
            format!("sek_{ID}_{}", &SECRET[1..]), // 42-char secret
            format!("sek_{ID}_{SECRET}A"),        // 44-char secret
            format!("sek_{ID}_{}+", &SECRET[1..]), // '+' is not base64url
        ] {
            assert!(parse_token(&bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn parse_keeps_underscores_and_hyphens_in_the_secret() {
        let secret = "ab_cd-ef_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert_eq!(secret.len(), 43);
        let (id, got) = parse_token(&format!("sek_{ID}_{secret}")).unwrap();
        assert_eq!(id.as_str(), ID);
        assert_eq!(got, secret);
    }

    #[test]
    fn key_id_parse() {
        assert_eq!(EnvKeyId::parse(ID).unwrap().as_str(), ID);
        assert_eq!(EnvKeyId::parse(ID).unwrap().to_string(), ID);
        for bad in [
            "",
            "abcdefghijk",
            "abcdefghijk22",
            "ABCDEFGHIJK2",
            "abcdefghijk0",
        ] {
            assert!(EnvKeyId::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn hash_secret_is_sha256() {
        assert_eq!(
            hash_secret("abc"),
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad
            ]
        );
    }

    #[test]
    fn state_priority() {
        let now = datetime!(2026-11-08 12:00 UTC);
        assert_eq!(
            effective_state(Some(now), true, Some(now), now),
            KeyState::Revoked
        );
        assert_eq!(
            effective_state(None, true, Some(now), now),
            KeyState::Expired
        ); // expiry instant counts as expired
        assert_eq!(effective_state(None, true, None, now), KeyState::Suspended);
        assert_eq!(effective_state(None, false, None, now), KeyState::Active);
    }

    #[test]
    fn state_edges() {
        let now = datetime!(2026-11-08 12:00 UTC);
        let later = now + Duration::SECOND;
        // Not yet expired one second before expiry.
        assert_eq!(
            effective_state(None, false, Some(later), now),
            KeyState::Active
        );
        // A revocation stamped slightly ahead (clock skew) still revokes.
        assert_eq!(
            effective_state(Some(later), false, None, now),
            KeyState::Revoked
        );
    }

    #[test]
    fn invalid_ranks_below_revoked_expired_suspended() {
        for s in [KeyState::Revoked, KeyState::Expired, KeyState::Suspended] {
            assert_eq!(with_access(s, false), s);
            assert_eq!(with_access(s, true), s);
        }
        assert_eq!(with_access(KeyState::Active, false), KeyState::Invalid);
        assert_eq!(with_access(KeyState::Active, true), KeyState::Active);
        assert_eq!(with_access(KeyState::Invalid, true), KeyState::Invalid);
    }

    #[test]
    fn random_bytes_come_from_the_os() {
        let a: [u8; 16] = random_bytes();
        let b: [u8; 16] = random_bytes();
        assert_ne!(a, b);
        assert_ne!(a, [0; 16]);
    }

    #[test]
    fn random_secrets_are_32_url_safe_bytes() {
        let s = random_secret();
        assert_eq!(s.len(), 43, "{s}");
        assert_eq!(URL_SAFE_NO_PAD.decode(&s).unwrap().len(), 32);
        assert_ne!(random_secret(), s);
    }
}
