//! Sekisho's PostgreSQL store, shared by every replica.
//!
//! Each write that changes what the gateway or the hooks see announces a
//! [`ChangeKind`] in the same transaction (see [`notify`]).

pub mod address;
mod budgets;
mod catalog;
mod dex_tokens;
mod environments;
mod error;
mod gateway_config;
mod groups;
mod install;
mod keys;
mod models;
pub mod notify;
mod oauth_state;
mod offboarding;
mod providers;
mod sessions;
mod setup_links;
mod signing_keys;
mod users;

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

pub use budgets::{Budget, BudgetScope, BudgetStatus, Charged, NewBudget};
pub use catalog::{
    Applied, Entry, EntryKey, FileMeta, FoundEntry, NewSource, Quarantine, QuarantineRecord,
    ResolveError, Revision, Source, SourcePatch, SyncFound, SyncOutcome, SyncOutcomeKind,
    SyncRecord, SyncReport, SyncSummary, ToolkitMember,
};
pub use dex_tokens::{DexTokenLock, IDP_CHECK_EVERY};
pub use environments::{Environment, EnvironmentSpec};
pub use error::{FieldError, StoreError};
pub use keys::{AuthSnapshot, EnvAccess, EnvKey, EnvModel};
pub use models::{Model, ModelPricing, NewModel};
pub use notify::ChangeKind;
pub use oauth_state::{
    AUTH_REQUEST_LIFETIME, AuthFlow, AuthRequest, CODE_LIFETIME, CodeGrant, DEVICE_CODE_LIFETIME,
    DEVICE_INTERVAL, Device, DevicePoll, DeviceStart, NewAuthRequest, NewCode, SLOW_DOWN_STEP,
};
pub use offboarding::Offboarded;
pub use providers::{
    CredentialKind, NewProvider, Provider, ProviderCredential, ProviderOptions, ProviderUpdate,
};
pub use sessions::{
    ENDED_SESSIONS_KEPT, REUSE_GRACE, RefreshCheck, RefreshLock, SESSION_IDLE,
    SIGN_IN_RECORDS_KEPT, Session, end_user_sessions,
};
pub use setup_links::{SETUP_LINK_LIFETIME, SetupClaim};
pub use signing_keys::{KEY_LEAD, KEY_ROTATION, KEY_TAIL};
pub use users::{Confirmation, LOCAL_CONNECTOR, SignIn, SignInOutcome, User, UserAccess};

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// How Sekisho connects to PostgreSQL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreOptions {
    /// The pool's size, shared by the API, the hooks and sign-in
    /// (`SEKISHO_DATABASE_MAX_CONNECTIONS`).
    pub max_connections: u32,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            max_connections: 20,
        }
    }
}

/// Every connection's limits: a transaction left idle this long, or a lock
/// waited on this long, ends in an error instead of holding a connection.
/// A backstop for a wait that passes through Sekisho, which PostgreSQL
/// can't see as a deadlock; a refresh waits on Dex for at most about 20 s.
const CONNECTION_LIMITS: [(&str, &str); 2] = [
    ("idle_in_transaction_session_timeout", "60s"),
    ("lock_timeout", "30s"),
];

/// A handle on the database. Cheap to clone; clones share one pool.
#[derive(Clone, Debug)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    pub async fn connect(url: &str, options: StoreOptions) -> Result<Self> {
        let connect = url.parse::<PgConnectOptions>()?.options(CONNECTION_LIMITS);
        let pool = PgPoolOptions::new()
            .max_connections(options.max_connections)
            .connect_with(connect)
            .await?;
        Ok(Self { pool })
    }

    /// Applies the migrations embedded at build time. Already-applied ones
    /// are skipped.
    pub async fn migrate(&self) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        // Another replica may hold the migration lock for longer than the
        // usual lock limit.
        sqlx::query("set lock_timeout = 0")
            .execute(&mut *conn)
            .await?;
        let ran = sqlx::migrate!("./migrations")
            .run(&mut *conn)
            .await
            .map_err(sqlx::Error::from);
        sqlx::query("reset lock_timeout")
            .execute(&mut *conn)
            .await?;
        ran?;
        Ok(())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}
