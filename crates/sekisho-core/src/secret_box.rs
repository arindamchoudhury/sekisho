//! AES-256-GCM for secrets at rest, such as provider API keys.

use std::fmt;

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, Generate, KeyInit, Nonce};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

const NONCE_LEN: usize = 12;

/// Seals and opens secrets with one install-wide key
/// (`SEKISHO_ENCRYPTION_KEY`). Sealed data is `nonce (12 bytes) ‖ ciphertext ‖ tag`.
#[derive(Clone)]
pub struct SecretBox {
    cipher: Aes256Gcm,
}

/// Messages never include key material or plaintext.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    #[error("the encryption key must be the base64 of exactly 32 bytes")]
    BadKey,
    #[error("a stored secret could not be decrypted: wrong encryption key or damaged data")]
    Decrypt,
}

impl SecretBox {
    /// Takes standard base64 of 32 bytes. Surrounding whitespace, such as a
    /// key file's trailing newline, is ignored.
    pub fn from_base64_key(key: &str) -> Result<Self, SecretError> {
        let bytes = STANDARD
            .decode(key.trim())
            .map_err(|_| SecretError::BadKey)?;
        let cipher = Aes256Gcm::new_from_slice(&bytes).map_err(|_| SecretError::BadKey)?;
        Ok(Self { cipher })
    }

    /// Encrypts with a fresh random nonce from the OS.
    pub fn seal(&self, plaintext: &[u8]) -> Vec<u8> {
        let nonce = Nonce::<Aes256Gcm>::generate();
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext)
            .expect("AES-GCM only fails on plaintexts over 64 GiB");
        let mut sealed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);
        sealed
    }

    pub fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, SecretError> {
        let (nonce, ciphertext) = sealed
            .split_at_checked(NONCE_LEN)
            .ok_or(SecretError::Decrypt)?;
        let nonce = Nonce::<Aes256Gcm>::try_from(nonce).map_err(|_| SecretError::Decrypt)?;
        self.cipher
            .decrypt(&nonce, ciphertext)
            .map_err(|_| SecretError::Decrypt)
    }
}

impl fmt::Debug for SecretBox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretBox(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as B64;

    #[test]
    fn secret_box_round_trip_and_tamper() {
        let b = SecretBox::from_base64_key(&B64.encode([7u8; 32])).unwrap();
        let sealed = b.seal(b"sk-test");
        assert_eq!(b.open(&sealed).unwrap(), b"sk-test");
        let mut bad = sealed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(b.open(&bad).is_err());
        assert!(SecretBox::from_base64_key("too-short").is_err());
    }

    #[test]
    fn seal_uses_a_fresh_nonce_each_time() {
        let b = SecretBox::from_base64_key(&B64.encode([7u8; 32])).unwrap();
        let (one, two) = (b.seal(b"sk-test"), b.seal(b"sk-test"));
        assert_ne!(one[..12], two[..12]);
        assert_eq!(one.len(), 12 + b"sk-test".len() + 16); // nonce ‖ ciphertext ‖ tag
        assert_eq!(b.open(&two).unwrap(), b"sk-test");
    }

    #[test]
    fn open_refuses_short_tampered_or_foreign_input() {
        let b = SecretBox::from_base64_key(&B64.encode([7u8; 32])).unwrap();
        let other = SecretBox::from_base64_key(&B64.encode([8u8; 32])).unwrap();
        let sealed = b.seal(b"sk-test");
        assert!(other.open(&sealed).is_err());
        let mut nonce_flipped = sealed.clone();
        nonce_flipped[0] ^= 1;
        assert!(b.open(&nonce_flipped).is_err());
        for short in [&[][..], &sealed[..11], &sealed[..12 + 15]] {
            assert!(b.open(short).is_err(), "{} bytes", short.len());
        }
        assert_eq!(b.open(&b.seal(b"")).unwrap(), b"");
    }

    #[test]
    fn key_must_be_base64_of_32_bytes() {
        assert!(SecretBox::from_base64_key(&B64.encode([7u8; 31])).is_err());
        assert!(SecretBox::from_base64_key(&B64.encode([7u8; 33])).is_err());
        assert!(SecretBox::from_base64_key("").is_err());
        assert!(SecretBox::from_base64_key("!!!!").is_err());
        // A trailing newline, as from a key file, is tolerated.
        assert!(SecretBox::from_base64_key(&format!("{}\n", B64.encode([7u8; 32]))).is_ok());
    }

    #[test]
    fn debug_and_errors_never_show_the_key() {
        let key = B64.encode([7u8; 32]);
        let b = SecretBox::from_base64_key(&key).unwrap();
        let shown = format!("{b:?}");
        assert!(!shown.contains(&key) && !shown.contains("7, 7"), "{shown}");
        let bad_key = B64.encode([9u8; 31]);
        let err = SecretBox::from_base64_key(&bad_key).unwrap_err();
        assert!(!err.to_string().contains(&bad_key) && !format!("{err:?}").contains(&bad_key));
    }
}
