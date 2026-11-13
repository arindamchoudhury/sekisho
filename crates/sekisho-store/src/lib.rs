//! Sekisho's PostgreSQL store, shared by every replica.
//!
//! Each write that changes what the gateway or the hooks see announces a
//! [`ChangeKind`] in the same transaction (see [`notify`]).

pub mod address;
mod budgets;
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
use sqlx::postgres::PgPoolOptions;

pub use budgets::{Budget, BudgetScope, BudgetStatus, Charged, NewBudget};
pub use dex_tokens::{DexTokenLock, IDP_CHECK_EVERY};
pub use environments::{Environment, EnvironmentSpec};
pub use error::{FieldError, StoreError};
pub use keys::{AuthSnapshot, EnvAccess, EnvKey};
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
    ENDED_SESSIONS_KEPT, REUSE_GRACE, RefreshCheck, RefreshLock, SESSION_IDLE, Session,
    end_user_sessions,
};
pub use setup_links::SETUP_LINK_LIFETIME;
pub use signing_keys::{KEY_LEAD, KEY_ROTATION, KEY_TAIL};
pub use users::{LOCAL_CONNECTOR, SignIn, SignInOutcome, User, UserAccess};

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// A handle on the database. Cheap to clone; clones share one pool.
#[derive(Clone, Debug)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new().connect(url).await?;
        Ok(Self { pool })
    }

    /// Applies the migrations embedded at build time. Already-applied ones
    /// are skipped.
    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .map_err(sqlx::Error::from)?;
        Ok(())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}
