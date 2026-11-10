//! Shared server state, the HTTP router and the gateway config publisher.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use sekisho_core::keys::hash_secret;
use sekisho_core::secret_box::SecretBox;
use sekisho_gateway::{HookTargets, RenderOptions, build_snapshot, render, validate};
use sekisho_store::{ChangeKind, Store, StoreError};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;

/// What the server needs besides the store and the encryption key.
#[derive(Clone)]
pub struct Settings {
    /// Admin API bearer token, until sign-in arrives in plan 2.
    pub bootstrap_token: String,
    /// Bearer token of the gateway agents.
    pub gateway_token: String,
    pub hooks: HookTargets,
    pub render: RenderOptions,
    /// How long a gateway config long poll waits before answering 204.
    pub long_poll: Duration,
    /// How often the publisher re-renders even without a change, so a lost
    /// notification or a failed publish is caught up.
    pub republish: Duration,
}

impl fmt::Debug for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Settings")
            .field("hooks", &self.hooks)
            .field("render", &self.render)
            .field("long_poll", &self.long_poll)
            .field("republish", &self.republish)
            .finish_non_exhaustive()
    }
}

/// Cheap to clone; clones share one state.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    secret_box: SecretBox,
    /// Tokens are kept only as hashes and compared in constant time.
    bootstrap_sha256: [u8; 32],
    gateway_sha256: [u8; 32],
    hooks: HookTargets,
    render: RenderOptions,
    long_poll: Duration,
    republish: Duration,
    /// The newest gateway config version this replica has seen saved.
    published: watch::Sender<i64>,
}

impl AppState {
    pub fn new(store: Store, secret_box: SecretBox, settings: Settings) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                secret_box,
                bootstrap_sha256: hash_secret(&settings.bootstrap_token),
                gateway_sha256: hash_secret(&settings.gateway_token),
                hooks: settings.hooks,
                render: settings.render,
                long_poll: settings.long_poll,
                republish: settings.republish,
                published: watch::channel(0).0,
            }),
        }
    }

    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    pub(crate) fn secret_box(&self) -> &SecretBox {
        &self.inner.secret_box
    }

    pub(crate) fn bootstrap_sha256(&self) -> &[u8; 32] {
        &self.inner.bootstrap_sha256
    }

    pub(crate) fn gateway_sha256(&self) -> &[u8; 32] {
        &self.inner.gateway_sha256
    }

    pub(crate) fn long_poll(&self) -> Duration {
        self.inner.long_poll
    }

    pub(crate) fn published(&self) -> watch::Receiver<i64> {
        self.inner.published.subscribe()
    }
}

/// The admin API and the gateway config endpoint.
pub fn router(state: AppState) -> Router {
    crate::api::router(state)
}

/// Kinds of change that alter the gateway config. Keys and budgets are
/// checked by the hooks at request time instead.
fn changes_config(kind: ChangeKind) -> bool {
    matches!(
        kind,
        ChangeKind::Providers | ChangeKind::Models | ChangeKind::Environments
    )
}

/// How soon a failed publish is tried again.
const PUBLISH_RETRY: Duration = Duration::from_secs(1);

/// Publishes the current config, then again after every change that alters
/// it, every `republish` regardless (saves are de-duplicated, so this costs a
/// render and a compare), and [`PUBLISH_RETRY`] after a failure. Subscribes
/// before the first publish, so no change is missed.
pub async fn start_publisher(state: &AppState) -> Result<JoinHandle<()>, StoreError> {
    let mut changes = state.store().listen().await?;
    let mut failed = publish(state).await.is_none();
    let state = state.clone();
    Ok(tokio::spawn(async move {
        let mut timer = tokio::time::interval(state.inner.republish);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        timer.tick().await; // the first tick is immediate
        loop {
            tokio::select! {
                change = changes.next() => match change {
                    Some(kind) if changes_config(kind) => {
                        // Take whatever else is already queued, so a burst renders once.
                        while let Ok(Some(_)) = tokio::time::timeout(Duration::ZERO, changes.next()).await {}
                    }
                    Some(_) => continue,
                    None => break,
                },
                _ = timer.tick() => {}
                () = tokio::time::sleep(PUBLISH_RETRY), if failed => {}
            }
            failed = publish(&state).await.is_none();
        }
        tracing::warn!("change notifications ended; gateway config publishing stopped");
    }))
}

/// Renders, validates and saves the gateway config. An invalid render keeps
/// the previous version. Returns the latest version, or `None` on failure.
pub async fn publish(state: &AppState) -> Option<i64> {
    let inner = &state.inner;
    let snapshot = match build_snapshot(&inner.store, &inner.secret_box, &inner.hooks).await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::error!(%error, "reading the store for the gateway config failed");
            return None;
        }
    };
    let yaml = render(&snapshot, &inner.render);
    if let Err(errors) = validate(&yaml) {
        tracing::error!(
            ?errors,
            "rendered gateway config fails agentgateway's schema; keeping the previous version"
        );
        return None;
    }
    match inner
        .store
        .save_gateway_config(&inner.secret_box, &yaml)
        .await
    {
        Ok(version) => {
            let newer = inner.published.send_if_modified(|current| {
                let newer = version > *current;
                if newer {
                    *current = version;
                }
                newer
            });
            if newer {
                tracing::info!(version, "gateway config published");
            }
            Some(version)
        }
        Err(error) => {
            tracing::error!(%error, "saving the gateway config failed");
            None
        }
    }
}
