//! Session tokens: EdDSA JWTs that Sekisho signs for a signed-in person.
//!
//! An access token lives an hour. Its `sub` is the person's user ID and its
//! `sid` their session, which can end before the token expires. [`verify`]
//! checks the signature, against the published key its `kid` names, before
//! anything else in the token.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::keys::os_random;

/// How long an access token works.
pub const ACCESS_TOKEN_LIFETIME: Duration = Duration::HOUR;
/// Every session token's `aud`.
pub const AUDIENCE: &str = "sekisho";

/// PKCS#8 v1 DER of an Ed25519 private key up to its 32-byte seed (RFC 8410).
const PKCS8_ED25519_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// What a session token says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionClaims {
    /// `SEKISHO_PUBLIC_URL`.
    pub iss: String,
    /// The person's user ID.
    pub sub: String,
    /// Always [`AUDIENCE`].
    pub aud: String,
    /// The session ID.
    pub sid: String,
    /// The OAuth client the person signed in with.
    pub client_id: String,
    pub iat: i64,
    pub exp: i64,
}

/// A key Sekisho signs session tokens with. Store its seed only sealed.
pub struct SigningKey {
    kid: String,
    seed: [u8; 32],
}

/// Shows the kid only, so a stray log line never carries the seed.
impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SigningKey")
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

impl SigningKey {
    /// A new key: a random seed and a random kid, both from the OS.
    pub fn generate() -> Self {
        let kid = os_random::<8>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Self {
            kid,
            seed: os_random(),
        }
    }

    /// A key read back from the database.
    pub fn from_seed(kid: String, seed: [u8; 32]) -> Self {
        Self { kid, seed }
    }

    /// 16 lowercase hex characters for a generated key.
    pub fn kid(&self) -> &str {
        &self.kid
    }

    pub fn seed(&self) -> &[u8; 32] {
        &self.seed
    }

    pub fn public(&self) -> PublicKey {
        PublicKey {
            kid: self.kid.clone(),
            x: ed25519_dalek::SigningKey::from_bytes(&self.seed)
                .verifying_key()
                .to_bytes(),
        }
    }

    fn encoding_key(&self) -> EncodingKey {
        let mut der = Vec::with_capacity(PKCS8_ED25519_PREFIX.len() + self.seed.len());
        der.extend_from_slice(&PKCS8_ED25519_PREFIX);
        der.extend_from_slice(&self.seed);
        EncodingKey::from_ed_der(&der)
    }
}

/// A signing key's public half, as published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey {
    pub kid: String,
    pub x: [u8; 32],
}

impl PublicKey {
    /// The key as an RFC 8037 JWK, for Sekisho's JWKS.
    pub fn to_jwk(&self) -> serde_json::Value {
        serde_json::json!({
            "kty": "OKP",
            "crv": "Ed25519",
            "use": "sig",
            "alg": "EdDSA",
            "kid": self.kid,
            "x": URL_SAFE_NO_PAD.encode(self.x),
        })
    }
}

/// Why a token was refused. The messages never quote the token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("not a session token")]
    Malformed,
    #[error("session token signed by an unknown key")]
    UnknownKey,
    #[error("session token refused")]
    Invalid,
    #[error("session token expired")]
    Expired,
}

/// Signs `claims` with `key`: header `alg: EdDSA`, `typ: JWT` and the key's
/// `kid`.
pub fn issue(key: &SigningKey, claims: &SessionClaims) -> String {
    let mut header = Header::new(Algorithm::EdDSA);
    header.typ = Some("JWT".to_owned());
    header.kid = Some(key.kid.clone());
    jsonwebtoken::encode(&header, claims, &key.encoding_key())
        .expect("an Ed25519 seed always signs, and the claims always serialize")
}

/// Checks a session token at `now`. `key` finds a published key by kid.
///
/// Only EdDSA, signed by the key the `kid` names; then `iss` and `aud` exactly
/// as expected, and `exp` after `now` with no leeway. A token signed by
/// someone else is `Invalid` even when it has also expired.
pub fn verify(
    token: &str,
    key: impl Fn(&str) -> Option<PublicKey>,
    issuer: &str,
    now: OffsetDateTime,
) -> Result<SessionClaims, TokenError> {
    if !looks_like_jwt(token) {
        return Err(TokenError::Malformed);
    }
    let header = jsonwebtoken::decode_header(token).map_err(|_| TokenError::Malformed)?;
    if header.alg != Algorithm::EdDSA {
        return Err(TokenError::Invalid);
    }
    let kid = header.kid.ok_or(TokenError::Invalid)?;
    let public = key(&kid).ok_or(TokenError::UnknownKey)?;
    let mut validation = Validation::new(Algorithm::EdDSA);
    validation.leeway = 0;
    // Checked below against `now`, not the system clock.
    validation.validate_exp = false;
    validation.set_issuer(&[issuer]);
    validation.set_audience(&[AUDIENCE]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    let claims = jsonwebtoken::decode::<SessionClaims>(
        token,
        &DecodingKey::from_ed_der(&public.x),
        &validation,
    )
    .map_err(|_| TokenError::Invalid)?
    .claims;
    if claims.exp <= now.unix_timestamp() {
        return Err(TokenError::Expired);
    }
    Ok(claims)
}

/// Whether `s` is shaped like a JWT: three non-empty base64url parts, the
/// first starting `eyJ` (a JSON object). Tells session tokens from
/// environment keys before any check.
pub fn looks_like_jwt(s: &str) -> bool {
    let base64url = |p: &str| {
        !p.is_empty()
            && p.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    };
    let mut parts = s.split('.');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(header), Some(payload), Some(signature), None) => {
            header.starts_with("eyJ") && [header, payload, signature].into_iter().all(base64url)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ed25519_dalek::Signer;
    use serde_json::{Value, json};
    use time::macros::datetime;

    const ISSUER: &str = "https://sekisho.test";
    const USER: &str = "0194c4f0-7b7e-7000-8000-000000000001";
    const SID: &str = "0194c4f0-7b7e-7000-8000-000000000002";

    fn now() -> OffsetDateTime {
        datetime!(2026-11-12 12:00 UTC)
    }

    fn at(seconds_from_now: i64) -> i64 {
        now().unix_timestamp() + seconds_from_now
    }

    fn claims(iss: &str, exp: i64) -> SessionClaims {
        SessionClaims {
            iss: iss.into(),
            sub: USER.into(),
            aud: AUDIENCE.into(),
            sid: SID.into(),
            client_id: "sekisho-cli".into(),
            iat: exp - 3600,
            exp,
        }
    }

    /// The key cache of an install that has published only `key`.
    fn only(key: &SigningKey) -> impl Fn(&str) -> Option<PublicKey> + '_ {
        move |kid| (kid == key.kid()).then(|| key.public())
    }

    fn b64(bytes: &[u8]) -> String {
        URL_SAFE_NO_PAD.encode(bytes)
    }

    fn part(v: &Value) -> String {
        b64(v.to_string().as_bytes())
    }

    /// A token with any header and payload, and any signature bytes.
    fn hand_made(header: Value, payload: Value, signature: &[u8]) -> String {
        format!("{}.{}.{}", part(&header), part(&payload), b64(signature))
    }

    /// A token with any header and payload, properly signed by `key`.
    fn signed_by(key: &SigningKey, header: Value, payload: Value) -> String {
        let message = format!("{}.{}", part(&header), part(&payload));
        let signature = ed25519_dalek::SigningKey::from_bytes(key.seed()).sign(message.as_bytes());
        format!("{message}.{}", b64(&signature.to_bytes()))
    }

    fn hs256(kid: &str, payload: &SessionClaims, secret: &[u8]) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some(kid.to_owned());
        jsonwebtoken::encode(
            &header,
            payload,
            &jsonwebtoken::EncodingKey::from_secret(secret),
        )
        .unwrap()
    }

    fn header_of(token: &str) -> Value {
        let first = token.split('.').next().unwrap();
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(first).unwrap()).unwrap()
    }

    #[test]
    fn issued_tokens_verify() {
        let key = SigningKey::generate();
        let issued = claims(ISSUER, at(3600));
        let t = issue(&key, &issued);
        assert!(looks_like_jwt(&t), "{t}");
        assert_eq!(
            header_of(&t),
            json!({"alg": "EdDSA", "typ": "JWT", "kid": key.kid()})
        );
        let c = verify(&t, only(&key), ISSUER, now()).unwrap();
        assert_eq!((c.sub.as_str(), c.aud.as_str()), (USER, "sekisho"));
        assert_eq!(c, issued);
    }

    #[test]
    fn a_token_works_until_the_second_it_expires() {
        let key = SigningKey::generate();
        let t = issue(&key, &claims(ISSUER, at(1)));
        assert!(verify(&t, only(&key), ISSUER, now()).is_ok());
        let later = now() + time::Duration::SECOND;
        assert_eq!(
            verify(&t, only(&key), ISSUER, later),
            Err(TokenError::Expired)
        );
    }

    #[test]
    fn a_key_rebuilt_from_its_seed_verifies_its_tokens() {
        let key = SigningKey::generate();
        let again = SigningKey::from_seed(key.kid().to_owned(), *key.seed());
        assert_eq!(again.public(), key.public());
        let t = issue(&again, &claims(ISSUER, at(3600)));
        assert!(verify(&t, only(&key), ISSUER, now()).is_ok());
    }

    #[test]
    fn signing_keys_have_hex_kids_and_hide_their_seed() {
        let key = SigningKey::generate();
        let kid = key.kid();
        assert_eq!(kid.len(), 16, "{kid}");
        assert!(
            kid.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
            "{kid}"
        );
        let other = SigningKey::generate();
        assert_ne!(other.kid(), kid);
        assert_ne!(other.seed(), key.seed());
        let shown = format!("{key:?}");
        assert!(shown.contains(kid), "{shown}");
        assert!(!shown.contains(&b64(key.seed())), "{shown}");
        assert!(!shown.contains(&format!("{:?}", key.seed())), "{shown}");
    }

    #[test]
    fn jwk_matches_rfc_8037() {
        let seed: [u8; 32] = URL_SAFE_NO_PAD
            .decode("nWGxne_9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A")
            .unwrap()
            .try_into()
            .unwrap();
        let key = SigningKey::from_seed("0123456789abcdef".into(), seed);
        let jwk = key.public().to_jwk();
        assert_eq!(
            jwk,
            json!({
                "kty": "OKP",
                "crv": "Ed25519",
                "use": "sig",
                "alg": "EdDSA",
                "kid": "0123456789abcdef",
                "x": "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo",
            })
        );
        // Anyone holding only the published JWK can check Sekisho's tokens.
        let t = issue(&key, &claims(ISSUER, at(3600)));
        let jwk: jsonwebtoken::jwk::Jwk = serde_json::from_value(jwk).unwrap();
        let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::EdDSA);
        v.validate_exp = false;
        v.set_audience(&[AUDIENCE]);
        let decoded = jsonwebtoken::decode::<SessionClaims>(
            &t,
            &jsonwebtoken::DecodingKey::from_jwk(&jwk).unwrap(),
            &v,
        );
        assert_eq!(decoded.unwrap().claims.sub, USER);
    }

    #[test]
    fn forged_and_foreign_tokens_are_refused() {
        use TokenError::{Expired, Invalid, Malformed, UnknownKey};
        let key = SigningKey::generate();
        let kid = key.kid();
        let x = key.public().x;
        let same_kid = SigningKey::from_seed(kid.to_owned(), [7; 32]);
        let good = claims(ISSUER, at(3600));
        let payload = serde_json::to_value(&good).unwrap();
        let header = json!({"alg": "EdDSA", "typ": "JWT", "kid": kid});
        let mut no_sid = payload.clone();
        no_sid.as_object_mut().unwrap().remove("sid");
        let mut aud_list = payload.clone();
        aud_list["aud"] = json!([AUDIENCE]);
        let swapped = {
            let mine = issue(&key, &good);
            let theirs = issue(
                &key,
                &SessionClaims {
                    sub: "someone-else".into(),
                    ..good.clone()
                },
            );
            let sig = mine.rsplit('.').next().unwrap().to_owned();
            let unsigned = theirs.rsplit_once('.').unwrap().0.to_owned();
            format!("{unsigned}.{sig}")
        };
        let cases: Vec<(&str, String, TokenError)> = vec![
            (
                "alg none, no signature",
                format!(
                    "{}.{}.",
                    part(&json!({"alg": "none", "kid": kid})),
                    part(&payload)
                ),
                Malformed,
            ),
            (
                "alg none, some signature",
                hand_made(json!({"alg": "none", "kid": kid}), payload.clone(), b"sig"),
                Malformed,
            ),
            ("HS256 keyed with x", hs256(kid, &good, &x), Invalid),
            (
                "HS256 keyed with the JWK's x",
                hs256(kid, &good, b64(&x).as_bytes()),
                Invalid,
            ),
            (
                "RS256 header",
                hand_made(
                    json!({"alg": "RS256", "typ": "JWT", "kid": kid}),
                    payload.clone(),
                    &[1; 256],
                ),
                Invalid,
            ),
            (
                "another key with the same kid",
                issue(&same_kid, &good),
                Invalid,
            ),
            ("a signature over other claims", swapped, Invalid),
            (
                "wrong iss",
                issue(&key, &claims("https://evil.test", at(3600))),
                Invalid,
            ),
            (
                "iss with a trailing slash",
                issue(&key, &claims("https://sekisho.test/", at(3600))),
                Invalid,
            ),
            (
                "wrong aud",
                issue(
                    &key,
                    &SessionClaims {
                        aud: "other".into(),
                        ..good.clone()
                    },
                ),
                Invalid,
            ),
            (
                "aud as a list",
                signed_by(&key, header.clone(), aud_list),
                Invalid,
            ),
            ("no sid", signed_by(&key, header.clone(), no_sid), Invalid),
            (
                "no kid",
                signed_by(&key, json!({"alg": "EdDSA", "typ": "JWT"}), payload.clone()),
                Invalid,
            ),
            ("exp == now", issue(&key, &claims(ISSUER, at(0))), Expired),
            (
                "exp long past",
                issue(&key, &claims(ISSUER, at(-86_400))),
                Expired,
            ),
            (
                "expired and signed by another key",
                issue(&same_kid, &claims(ISSUER, at(-86_400))),
                Invalid,
            ),
            (
                "unknown kid",
                issue(&SigningKey::generate(), &good),
                UnknownKey,
            ),
            (
                "two parts",
                "eyJhbGciOiJFZERTQSJ9.eyJzdWIiOiJ4In0".into(),
                Malformed,
            ),
            ("a header that isn't JSON", "eyJ.eyJ.c2ln".into(), Malformed),
            ("garbage", "garbage".into(), Malformed),
            ("an environment key", "sek_abcdefghijkl_x".into(), Malformed),
            ("empty", String::new(), Malformed),
        ];
        for (what, token, expected) in cases {
            let err = verify(&token, only(&key), ISSUER, now()).expect_err(what);
            assert_eq!(err, expected, "{what}");
            let shown = format!("{err} {err:?}");
            for piece in token.split('.').filter(|p| p.len() >= 8) {
                assert!(!shown.contains(piece), "{what}: {shown}");
            }
        }
    }

    #[test]
    fn jwt_shape() {
        let key = SigningKey::generate();
        assert!(looks_like_jwt(&issue(&key, &claims(ISSUER, at(60)))));
        assert!(looks_like_jwt("eyJa.b.c"));
        for bad in [
            "",
            "sek_abcdefghijkl_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "a.b.c",
            "eyJa.b",
            "eyJa.b.c.d",
            "eyJa..c",
            "eyJa.b.",
            ".eyJa.b",
            "eyJa.b+.c",
            "eyJa.b.c=",
            " eyJa.b.c",
            "Bearer eyJa.b.c",
        ] {
            assert!(!looks_like_jwt(bad), "{bad:?}");
        }
    }
}
