//! Keys and environments in memory, for checking requests without a database
//! round trip. Every replica keeps its own copy and refreshes it on change
//! notifications from any replica.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, PoisonError, RwLock, Weak};
use std::time::Duration;

use sekisho_core::keys::EnvKeyId;
use sekisho_core::names::EnvName;
use sekisho_store::{ChangeKind, Store, StoreError};
use time::OffsetDateTime;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;

/// How often the cache reloads even without notifications, in case one was
/// lost.
pub const SAFETY_NET: Duration = Duration::from_secs(30);

/// What the hooks need to know about a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedKey {
    pub id: EnvKeyId,
    pub environment: EnvName,
    pub owner: String,
    pub secret_sha256: [u8; 32],
    pub expires_at: Option<OffsetDateTime>,
    pub suspended: bool,
    pub revoked: bool,
}

#[derive(Default)]
struct Data {
    keys: HashMap<EnvKeyId, CachedKey>,
    environments: HashSet<EnvName>,
}

pub struct AuthCache {
    /// Replaced as a whole on reload, so readers never see a partial update.
    data: RwLock<Arc<Data>>,
}

fn changes_auth(kind: ChangeKind) -> bool {
    matches!(kind, ChangeKind::Keys | ChangeKind::Environments)
}

impl AuthCache {
    pub async fn load(store: &Store) -> Result<Arc<Self>, StoreError> {
        let cache = Arc::new(Self {
            data: RwLock::new(Arc::default()),
        });
        cache.reload(store).await?;
        Ok(cache)
    }

    /// Reads everything again and swaps it in. On error the old data stays.
    pub async fn reload(&self, store: &Store) -> Result<(), StoreError> {
        let snapshot = store.auth_snapshot().await?;
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
            environments: snapshot.environments.into_iter().collect(),
        });
        *self.data.write().unwrap_or_else(PoisonError::into_inner) = data;
        Ok(())
    }

    fn data(&self) -> Arc<Data> {
        Arc::clone(&self.data.read().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn key(&self, id: &EnvKeyId) -> Option<CachedKey> {
        self.data().keys.get(id).cloned()
    }

    pub fn environment_exists(&self, name: &EnvName) -> bool {
        self.data().environments.contains(name)
    }

    /// Reloads on `Keys` and `Environments` notifications from any replica,
    /// and every [`SAFETY_NET`] regardless.
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
