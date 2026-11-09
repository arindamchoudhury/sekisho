//! Sekisho's PostgreSQL store, shared by every replica.
//!
//! Each write that changes what the gateway or the hooks see announces a
//! [`ChangeKind`] in the same transaction (see [`notify`]).

mod budgets;
mod environments;
mod error;
mod gateway_config;
mod keys;
mod models;
pub mod notify;
mod providers;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

pub use budgets::{Budget, BudgetScope, BudgetStatus, Charged, NewBudget};
pub use environments::{Environment, EnvironmentSpec};
pub use error::{FieldError, StoreError};
pub use keys::{AuthSnapshot, EnvKey};
pub use models::{Model, NewModel};
pub use notify::ChangeKind;
pub use providers::{NewProvider, Provider};

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
