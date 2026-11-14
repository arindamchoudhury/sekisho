//! Shared server state, the HTTP router, the gateway config publisher, and
//! sign-in's background work: registering with Dex, rotating signing keys
//! and the daily check of key owners at Dex.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use sekisho_auth::AuthState;
use sekisho_auth::dex::{DEX_TIMEOUT, Dex};
use sekisho_core::keys::hash_secret;
use sekisho_core::secret_box::SecretBox;
use sekisho_gateway::{HookTargets, RenderOptions, build_snapshot, render, validate};
use sekisho_hooks::cache::AuthCache;
use sekisho_store::{ChangeKind, Store, StoreError};
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use url::Url;

/// What the server needs besides the store and the encryption key.
#[derive(Clone)]
pub struct Settings {
    /// Only for `/setup`, until the first admin exists.
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
    /// `SEKISHO_PUBLIC_URL`, without a trailing slash: the issuer.
    pub public_url: Url,
    /// `SEKISHO_ADMIN_GROUPS`: members of these groups are admins.
    pub admin_groups: Vec<String>,
}

impl fmt::Debug for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Settings")
            .field("hooks", &self.hooks)
            .field("render", &self.render)
            .field("long_poll", &self.long_poll)
            .field("republish", &self.republish)
            .field("public_url", &self.public_url.as_str())
            .field("admin_groups", &self.admin_groups)
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
    /// The sign-in routes' state: Dex, the hooks' cache, the public URL.
    auth: AuthState,
    admin_groups: Vec<String>,
    /// Whether Sekisho's client is registered at Dex; sign-ins wait for it.
    dex_ready: AtomicBool,
    hooks: HookTargets,
    render: RenderOptions,
    long_poll: Duration,
    republish: Duration,
    /// The newest gateway config version this replica has seen saved.
    published: watch::Sender<i64>,
}

impl AppState {
    pub fn new(
        store: Store,
        secret_box: SecretBox,
        settings: Settings,
        cache: Arc<AuthCache>,
        dex: Arc<dyn Dex>,
    ) -> Self {
        let bootstrap_sha256 = hash_secret(&settings.bootstrap_token);
        let auth = AuthState {
            store: store.clone(),
            secret_box: secret_box.clone(),
            dex,
            cache,
            public_url: settings.public_url,
            bootstrap_sha256,
            clock: Arc::new(OffsetDateTime::now_utc),
        };
        Self {
            inner: Arc::new(Inner {
                store,
                secret_box,
                bootstrap_sha256,
                gateway_sha256: hash_secret(&settings.gateway_token),
                auth,
                admin_groups: settings.admin_groups,
                dex_ready: AtomicBool::new(false),
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

    /// The sign-in routes' state.
    pub fn auth(&self) -> &AuthState {
        &self.inner.auth
    }

    pub(crate) fn cache(&self) -> &AuthCache {
        &self.inner.auth.cache
    }

    pub(crate) fn dex(&self) -> &dyn Dex {
        self.inner.auth.dex.as_ref()
    }

    pub(crate) fn admin_groups(&self) -> &[String] {
        &self.inner.admin_groups
    }

    /// Whether Sekisho's client is registered at Dex, so sign-ins can start.
    pub fn dex_ready(&self) -> bool {
        self.inner.dex_ready.load(Ordering::Acquire)
    }
}

/// The admin API, the gateway config endpoint, and the sign-in routes
/// (`sekisho_auth::router`), whose sign-ins wait until Sekisho is registered
/// at Dex.
pub fn router(state: AppState) -> Router {
    let sign_in = sekisho_auth::router(state.auth().clone())
        .layer(middleware::from_fn_with_state(state.clone(), wait_for_dex));
    crate::api::router(state).merge(sign_in)
}

/// The routes that send a browser to Dex, or start a device's sign-in.
fn starts_a_sign_in(path: &str) -> bool {
    matches!(
        path,
        "/oauth/authorize" | "/oauth/device_authorization" | "/oauth/device/confirm"
    )
}

/// Until Sekisho's client is registered at Dex, no sign-in can start: 503, a
/// page for browsers and RFC 6749 JSON for the device endpoint.
async fn wait_for_dex(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if state.dex_ready() || !starts_a_sign_in(req.uri().path()) {
        return next.run(req).await;
    }
    if req.uri().path() == "/oauth/device_authorization" {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CACHE_CONTROL, "no-store")],
            axum::Json(serde_json::json!({
                "error": "temporarily_unavailable",
                "error_description": "Sekisho is still connecting to its sign-in service",
            })),
        )
            .into_response();
    }
    sekisho_auth::pages::error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Sign-in is starting",
        "Sekisho is still connecting to its sign-in service. Try again in a minute.",
    )
}

/// Registers Sekisho's client at Dex, trying again every `retry` until it
/// works; sign-ins can start from then on.
pub fn register_with_dex(state: &AppState, retry: Duration) -> JoinHandle<()> {
    let state = state.clone();
    tokio::spawn(async move {
        loop {
            match tokio::time::timeout(DEX_TIMEOUT, state.dex().ensure_client()).await {
                Ok(Ok(())) => {
                    state.inner.dex_ready.store(true, Ordering::Release);
                    tracing::info!("registered with Dex; sign-in is open");
                    return;
                }
                Ok(Err(error)) => {
                    tracing::warn!(%error, "couldn't register with Dex; trying again");
                }
                Err(_) => tracing::warn!("Dex didn't answer the registration; trying again"),
            }
            tokio::time::sleep(retry).await;
        }
    })
}

/// How often the signing key's age is checked (it rotates every 30 days).
pub const KEY_ROTATION_CHECK: Duration = Duration::from_secs(60 * 60);
/// How often key owners due their daily check are checked at Dex.
pub const IDP_SWEEP_EVERY: Duration = Duration::from_secs(10 * 60);

/// Rotates the signing key when it is due, every `rotation_check`, and
/// checks key owners at Dex every `sweep_every`.
pub fn start_sign_in_upkeep(
    state: &AppState,
    rotation_check: Duration,
    sweep_every: Duration,
) -> JoinHandle<()> {
    let state = state.clone();
    tokio::spawn(async move {
        let mut rotation = tokio::time::interval(rotation_check);
        let mut sweep = tokio::time::interval(sweep_every);
        for timer in [&mut rotation, &mut sweep] {
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        }
        loop {
            tokio::select! {
                _ = rotation.tick() => {
                    let now = OffsetDateTime::now_utc();
                    let rotated = state.store().rotate_signing_key(state.secret_box(), now, false).await;
                    if let Err(error) = rotated {
                        tracing::error!(%error, "rotating the signing key failed");
                    }
                }
                _ = sweep.tick() => {
                    let swept = sekisho_auth::sweep::check_key_owners(state.auth(), OffsetDateTime::now_utc()).await;
                    if let Err(error) = swept {
                        tracing::error!(%error, "checking key owners at Dex failed");
                    }
                }
            }
        }
    })
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
