//! Keys, environments, people, recently ended sessions and Sekisho's
//! published signing keys in memory, for checking requests without a
//! database round trip. Every replica keeps its own copy and refreshes it on
//! change notifications from any replica.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};
use std::time::Duration;

use sekisho_core::access::Access;
use sekisho_core::keys::EnvKeyId;
use sekisho_core::names::EnvName;
use sekisho_core::session_token::PublicKey;
use sekisho_store::{ChangeKind, Store, StoreError};
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use uuid::Uuid;

/// How often the cache reloads even without notifications, in case one was
/// lost.
pub const SAFETY_NET: Duration = Duration::from_secs(30);

/// What the hooks need to know about a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedKey {
    pub id: EnvKeyId,
    pub environment: EnvName,
    /// The user the key belongs to.
    pub owner: Uuid,
    pub secret_sha256: [u8; 32],
    pub expires_at: Option<OffsetDateTime>,
    pub suspended: bool,
    pub revoked: bool,
}

/// What the hooks need to know about a person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedUser {
    pub id: Uuid,
    pub access: Access,
    /// The admin's mark only; admin groups are applied by the server.
    pub admin_mark: bool,
}

/// What the hooks need to know about an environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedEnvironment {
    pub name: EnvName,
    pub access_groups: Vec<String>,
    pub owner: Option<Uuid>,
}

#[derive(Default)]
struct Data {
    keys: HashMap<EnvKeyId, CachedKey>,
    environments: HashMap<EnvName, CachedEnvironment>,
    users: HashMap<Uuid, CachedUser>,
    /// Sessions ended within the last 65 minutes: any access token of an
    /// earlier-ended session has expired anyway.
    ended_sessions: HashSet<Uuid>,
    /// Sekisho's published signing keys, by `kid`.
    public_keys: HashMap<String, PublicKey>,
}

pub struct AuthCache {
    /// Replaced as a whole on reload, so readers never see a partial update.
    data: RwLock<Arc<Data>>,
    /// Users read from the database since the last reload (they signed in on
    /// another replica a moment ago).
    loaded: Mutex<HashMap<Uuid, CachedUser>>,
    /// `SEKISHO_PUBLIC_URL`, every session token's `iss`.
    issuer: String,
}

fn changes_auth(kind: ChangeKind) -> bool {
    matches!(
        kind,
        ChangeKind::Keys
            | ChangeKind::Environments
            | ChangeKind::Users
            | ChangeKind::Sessions
            | ChangeKind::SigningKeys
    )
}

impl AuthCache {
    /// Loads everything; session tokens must name `issuer`.
    pub async fn load(store: &Store, issuer: String) -> Result<Arc<Self>, StoreError> {
        let cache = Arc::new(Self {
            data: RwLock::new(Arc::default()),
            loaded: Mutex::new(HashMap::new()),
            issuer,
        });
        cache.reload(store).await?;
        Ok(cache)
    }

    /// Reads everything again and swaps it in. On error the old data stays.
    pub async fn reload(&self, store: &Store) -> Result<(), StoreError> {
        let snapshot = store.auth_snapshot(OffsetDateTime::now_utc()).await?;
        let keys = snapshot
            .keys
            .into_iter()
            .map(|(key, secret_sha256)| {
                let cached = CachedKey {
                    id: key.id.clone(),
                    environment: key.environment,
                    owner: key.owner,
                    secret_sha256,
                    expires_at: key.expires_at,
                    suspended: key.suspended,
                    revoked: key.revoked_at.is_some(),
                };
                (key.id, cached)
            })
            .collect();
        let data = Arc::new(Data {
            keys,
            environments: snapshot
                .environments
                .into_iter()
                .map(|e| {
                    let cached = CachedEnvironment {
                        name: e.name.clone(),
                        access_groups: e.access_groups,
                        owner: e.owner,
                    };
                    (e.name, cached)
                })
                .collect(),
            users: snapshot
                .users
                .into_iter()
                .map(|u| {
                    let cached = CachedUser {
                        id: u.id,
                        access: u.access,
                        admin_mark: u.admin_mark,
                    };
                    (u.id, cached)
                })
                .collect(),
            ended_sessions: snapshot.ended_sessions.into_iter().collect(),
            public_keys: snapshot
                .public_keys
                .into_iter()
                .map(|k| (k.kid.clone(), k))
                .collect(),
        });
        *self.data.write().unwrap_or_else(PoisonError::into_inner) = data;
        // The snapshot has everyone read before it.
        self.loaded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        Ok(())
    }

    fn data(&self) -> Arc<Data> {
        Arc::clone(&self.data.read().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn key(&self, id: &EnvKeyId) -> Option<CachedKey> {
        self.data().keys.get(id).cloned()
    }

    pub fn environment_exists(&self, name: &EnvName) -> bool {
        self.data().environments.contains_key(name)
    }

    pub fn environment(&self, name: &EnvName) -> Option<CachedEnvironment> {
        self.data().environments.get(name).cloned()
    }

    /// The user as cached, or as read since the last reload.
    pub fn user(&self, id: &Uuid) -> Option<CachedUser> {
        if let Some(user) = self.data().users.get(id) {
            return Some(user.clone());
        }
        self.loaded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    /// The user, read from the database once if the cache doesn't have them
    /// yet (they signed in since the last reload); kept until the next
    /// reload. `None` for an unknown user.
    pub async fn user_or_load(
        &self,
        store: &Store,
        id: &Uuid,
    ) -> Result<Option<CachedUser>, StoreError> {
        if let Some(user) = self.user(id) {
            return Ok(Some(user));
        }
        let user = match store.user(id).await {
            Ok(user) => user,
            Err(StoreError::NotFound) => return Ok(None),
            Err(error) => return Err(error),
        };
        let cached = CachedUser {
            id: user.id,
            access: user.access(),
            admin_mark: user.admin,
        };
        self.loaded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(cached.id, cached.clone());
        Ok(Some(cached))
    }

    /// Whether the session ended within the last 65 minutes.
    pub fn session_ended(&self, id: &Uuid) -> bool {
        self.data().ended_sessions.contains(id)
    }

    /// A published signing key by its `kid`.
    pub fn public_key(&self, kid: &str) -> Option<PublicKey> {
        self.data().public_keys.get(kid).cloned()
    }

    /// Every session token's `iss`.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Reloads on `Keys`, `Environments`, `Users`, `Sessions` and
    /// `SigningKeys` notifications from any replica, and every
    /// [`SAFETY_NET`] regardless.
    pub fn spawn_refresh(self: &Arc<Self>, store: Store) -> JoinHandle<()> {
        self.spawn_refresh_every(store, SAFETY_NET)
    }

    /// [`AuthCache::spawn_refresh`] with another safety-net period. The task
    /// ends when the cache is dropped or the store's pool is closed.
    pub fn spawn_refresh_every(
        self: &Arc<Self>,
        store: Store,
        safety_net: Duration,
    ) -> JoinHandle<()> {
        let cache = Arc::downgrade(self);
        tokio::spawn(refresh(cache, store, safety_net))
    }
}

/// Reloads the cache if it still exists; `false` once it is gone.
async fn reload(cache: &Weak<AuthCache>, store: &Store) -> bool {
    let Some(cache) = cache.upgrade() else {
        return false;
    };
    if let Err(error) = cache.reload(store).await {
        tracing::warn!(%error, "auth cache reload failed; keeping the previous data");
    }
    true
}

async fn refresh(cache: Weak<AuthCache>, store: Store, safety_net: Duration) {
    let mut changes = loop {
        match store.listen().await {
            Ok(changes) => break changes,
            Err(error) => {
                tracing::warn!(%error, "auth cache cannot listen for changes; retrying");
                if store.pool().is_closed() || cache.strong_count() == 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    };
    // Changes between load() and the subscription were not announced to us.
    if !reload(&cache, &store).await {
        return;
    }
    let mut ticker = tokio::time::interval(safety_net);
    ticker.tick().await; // the first tick is immediate
    loop {
        tokio::select! {
            change = changes.next() => match change {
                Some(kind) if changes_auth(kind) => {
                    // Take whatever else is already queued, so a burst reloads once.
                    while let Ok(Some(_)) = tokio::time::timeout(Duration::ZERO, changes.next()).await {}
                    if !reload(&cache, &store).await {
                        return;
                    }
                }
                Some(_) => {}
                None => return, // the pool was closed
            },
            _ = ticker.tick() => {
                if !reload(&cache, &store).await {
                    return;
                }
            }
        }
    }
}
