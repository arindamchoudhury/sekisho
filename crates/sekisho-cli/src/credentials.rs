//! Where the CLI keeps its tokens: the OS keyring, the kernel's keyutils on
//! Linux without Secret Service, else memory for one process (D195). One
//! entry per server holds the access token, its expiry and the refresh token
//! as UTF-8 JSON, written in one `set_secret` (D213), so a rotation can't
//! tear. On Windows the entry is a generic credential `sekisho:<server>`
//! with local-machine persistence: it never roams with the profile.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use keyring_core::api::CredentialStore;
use keyring_core::{Entry, Error as KeyringError};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// The keyring service every entry is under.
pub const SERVICE: &str = "sekisho";

/// The account `open_store` reads to see whether the keyring answers.
const PROBE: &str = "sekisho-probe";

/// A signed-in session's tokens.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access: String,
    #[serde(with = "time::serde::rfc3339")]
    pub access_expires_at: OffsetDateTime,
    pub refresh: String,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("access", &"<hidden>")
            .field("access_expires_at", &self.access_expires_at)
            .field("refresh", &"<hidden>")
            .finish()
    }
}

/// What a request to Sekisho carries.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// A session token from `sekisho login` or `sekisho run`.
    Session(String),
    /// An environment key, `SEKISHO_KEY`.
    Key(String),
}

impl Credential {
    pub fn secret(&self) -> &str {
        match self {
            Credential::Session(s) | Credential::Key(s) => s,
        }
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Credential::Session(_) => f.write_str("Session(<hidden>)"),
            Credential::Key(_) => f.write_str("Key(<hidden>)"),
        }
    }
}

/// Why the credential store failed. Built from the kind of failure only:
/// keyring errors can carry the stored bytes, which are tokens.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the credential store {0}")]
pub struct StoreError(String);

impl From<KeyringError> for StoreError {
    fn from(e: KeyringError) -> Self {
        Self(match e {
            KeyringError::PlatformFailure(p) => format!("failed: {p}"),
            KeyringError::NoStorageAccess(p) => format!("can't be reached: {p}"),
            KeyringError::NoEntry => "has no entry".to_owned(),
            KeyringError::BadEncoding(_) | KeyringError::BadDataFormat(_, _) => {
                "holds an entry that isn't Sekisho's tokens".to_owned()
            }
            KeyringError::BadStoreFormat(why) => format!("is damaged: {why}"),
            KeyringError::TooLong(what, max) => format!("refused a {what} over {max}"),
            KeyringError::Invalid(what, why) => format!("refused the {what}: {why}"),
            KeyringError::Ambiguous(_) => "holds more than one matching entry".to_owned(),
            KeyringError::NoDefaultStore => "isn't set up".to_owned(),
            KeyringError::NotSupportedByStore(why) => format!("can't do that: {why}"),
            _ => "failed".to_owned(),
        })
    }
}

/// Where tokens are kept, by server.
pub trait Store: Send + Sync {
    fn load(&self, server: &str) -> Result<Option<Tokens>, StoreError>;
    fn save(&self, server: &str, tokens: &Tokens) -> Result<(), StoreError>;
    fn delete(&self, server: &str) -> Result<(), StoreError>;
}

/// Tokens for one process's life: `sekisho run` without a keyring, and
/// tests.
#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<String, Tokens>>);

impl Store for MemoryStore {
    fn load(&self, server: &str) -> Result<Option<Tokens>, StoreError> {
        Ok(self.0.lock().unwrap().get(server).cloned())
    }

    fn save(&self, server: &str, tokens: &Tokens) -> Result<(), StoreError> {
        self.0
            .lock()
            .unwrap()
            .insert(server.to_owned(), tokens.clone());
        Ok(())
    }

    fn delete(&self, server: &str) -> Result<(), StoreError> {
        self.0.lock().unwrap().remove(server);
        Ok(())
    }
}

/// A keyring: one entry per server, service `sekisho`, account the server
/// URL.
pub struct KeyringStore {
    store: Arc<CredentialStore>,
    /// Windows' modifiers: a named target kept on this machine (D213).
    windows: bool,
}

impl KeyringStore {
    pub fn new(store: Arc<CredentialStore>, windows: bool) -> Self {
        Self { store, windows }
    }

    fn entry(&self, server: &str) -> Result<Entry, StoreError> {
        if self.windows {
            let target = format!("{SERVICE}:{server}");
            let modifiers = HashMap::from([("target", target.as_str()), ("persistence", "Local")]);
            Ok(self.store.build(SERVICE, server, Some(&modifiers))?)
        } else {
            Ok(self.store.build(SERVICE, server, None)?)
        }
    }
}

impl Store for KeyringStore {
    fn load(&self, server: &str) -> Result<Option<Tokens>, StoreError> {
        match self.entry(server)?.get_secret() {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| StoreError("holds an entry that isn't Sekisho's tokens".to_owned())),
            Err(KeyringError::NoEntry) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, server: &str, tokens: &Tokens) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec(tokens).expect("tokens serialise");
        Ok(self.entry(server)?.set_secret(&bytes)?)
    }

    fn delete(&self, server: &str) -> Result<(), StoreError> {
        match self.entry(server)?.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// The store the CLI uses, and whether it is only memory.
pub struct OpenedStore {
    pub store: Box<dyn Store>,
    /// No keyring answered: tokens last only as long as this process.
    pub no_keyring: bool,
}

/// The platform's keyring, then (Linux) keyutils; memory when none answers,
/// including `NoStorageAccess` (Windows without the person's password: SSH
/// with a key, a service) and `NoDefaultStore` (D195).
pub fn open_store() -> OpenedStore {
    open_store_from(platform_stores(), cfg!(windows))
}

/// `open_store` over these candidate stores, in order.
pub fn open_store_from(
    candidates: Vec<Result<Arc<CredentialStore>, KeyringError>>,
    windows: bool,
) -> OpenedStore {
    for candidate in candidates {
        let Ok(store) = candidate else { continue };
        let answers =
            store
                .build(SERVICE, PROBE, None)
                .and_then(|entry| match entry.get_secret() {
                    Ok(_) | Err(KeyringError::NoEntry) => Ok(()),
                    Err(e) => Err(e),
                });
        if answers.is_ok() {
            return OpenedStore {
                store: Box::new(KeyringStore::new(store, windows)),
                no_keyring: false,
            };
        }
    }
    OpenedStore {
        store: Box::new(MemoryStore::default()),
        no_keyring: true,
    }
}

/// The platform's stores: keyring's own choice for the platform, which it
/// sets as keyring-core's default store on first use; on Linux, keyutils
/// after it.
fn platform_stores() -> Vec<Result<Arc<CredentialStore>, KeyringError>> {
    let platform = match keyring::Entry::store_status() {
        Ok(()) => keyring_core::get_default_store().ok_or(KeyringError::NoDefaultStore),
        Err(_) => Err(KeyringError::NoDefaultStore),
    };
    #[allow(unused_mut)]
    let mut stores = vec![platform];
    #[cfg(target_os = "linux")]
    stores.push(linux_keyutils_keyring_store::Store::new().map(|s| s as Arc<CredentialStore>));
    stores
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyring_core::mock;

    fn tokens(n: u8) -> Tokens {
        Tokens {
            access: format!("access-{n}"),
            access_expires_at: OffsetDateTime::from_unix_timestamp(1_800_000_000 + i64::from(n))
                .unwrap(),
            refresh: format!("refresh-{n}"),
        }
    }

    fn mock_store() -> Arc<CredentialStore> {
        mock::Store::new().unwrap()
    }

    fn platform_error() -> keyring_core::error::PlatformError {
        Box::new(std::io::Error::other("no logon session"))
    }

    #[test]
    fn tokens_are_saved_in_one_write() {
        let raw = mock_store();
        let store = KeyringStore::new(Arc::clone(&raw), false);
        let server = "https://s.example.com";
        store.save(server, &tokens(1)).unwrap();

        // One secret holds all three, as UTF-8 JSON.
        let entry = raw.build(SERVICE, server, None).unwrap();
        let bytes = entry.get_secret().unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["access"], "access-1");
        assert_eq!(json["refresh"], "refresh-1");
        assert!(json["access_expires_at"].is_string());
        assert_eq!(store.load(server).unwrap(), Some(tokens(1)));

        // A failed write leaves the previous tokens whole.
        let cred: &mock::Cred = entry.as_any().downcast_ref().unwrap();
        cred.set_error(KeyringError::PlatformFailure(platform_error()));
        assert!(store.save(server, &tokens(2)).is_err());
        assert_eq!(store.load(server).unwrap(), Some(tokens(1)));

        store.delete(server).unwrap();
        assert_eq!(store.load(server).unwrap(), None);
        store.delete(server).unwrap();
    }

    #[test]
    fn no_keyring_falls_back_to_memory() {
        // Windows' ERROR_NO_SUCH_LOGON_SESSION reaches keyring as
        // NoStorageAccess.
        let raw = mock_store();
        let probe = raw.build(SERVICE, PROBE, None).unwrap();
        let cred: &mock::Cred = probe.as_any().downcast_ref().unwrap();
        cred.set_error(KeyringError::NoStorageAccess(platform_error()));
        let opened = open_store_from(vec![Ok(raw)], false);
        assert!(opened.no_keyring);
        opened.store.save("https://s", &tokens(1)).unwrap();
        assert_eq!(opened.store.load("https://s").unwrap(), Some(tokens(1)));

        let opened = open_store_from(vec![Err(KeyringError::NoDefaultStore)], false);
        assert!(opened.no_keyring);

        // The next candidate is used when the first doesn't answer, as
        // keyutils is after Secret Service.
        let opened = open_store_from(
            vec![Err(KeyringError::NoDefaultStore), Ok(mock_store())],
            false,
        );
        assert!(!opened.no_keyring);
    }

    #[test]
    fn errors_never_hold_the_stored_bytes() {
        let secret = b"a-refresh-token-in-the-bytes".to_vec();
        for e in [
            KeyringError::BadEncoding(secret.clone()),
            KeyringError::BadDataFormat(secret.clone(), platform_error()),
        ] {
            let text = format!("{0} {0:?}", StoreError::from(e));
            assert!(!text.contains("a-refresh-token"), "{text}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_entries_stay_on_this_machine() {
        let opened = open_store();
        assert!(!opened.no_keyring, "Credential Manager didn't answer");
        let store = opened.store;
        let server = format!(
            "https://sekisho-test-{}.invalid",
            sekisho_core::keys::random_secret().to_lowercase()
        );
        let mut long = tokens(1);
        long.access = "a".repeat(1_000);
        store.save(&server, &long).unwrap();

        let raw = keyring_core::get_default_store().unwrap();
        let target = format!("{SERVICE}:{server}");
        let modifiers = HashMap::from([("target", target.as_str()), ("persistence", "Local")]);
        let entry = raw.build(SERVICE, &server, Some(&modifiers)).unwrap();
        let attributes = entry.get_attributes().unwrap();
        assert_eq!(attributes["persistence"], "Local");
        assert_eq!(attributes["target_name"], target);
        let blob = entry.get_secret().unwrap();
        assert!(std::str::from_utf8(&blob).is_ok(), "not UTF-8");
        assert!(blob.len() < 2_560, "{} bytes", blob.len());
        assert_eq!(store.load(&server).unwrap(), Some(long));

        store.delete(&server).unwrap();
        assert!(matches!(entry.get_secret(), Err(KeyringError::NoEntry)));
    }
}
