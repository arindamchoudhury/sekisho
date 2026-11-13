//! Values made once per install.

use sekisho_core::keys::random_secret;
use sekisho_core::secret_box::SecretBox;
use time::OffsetDateTime;

use crate::error::bad_column;
use crate::{Result, Store, StoreError};

const SETUP_COMPLETED: &str = "setup_completed";
const DEX_CLIENT_SECRET: &str = "dex_client_secret";

impl Store {
    /// Whether the first admin has been made on `/setup`.
    pub async fn setup_completed(&self) -> Result<bool> {
        Ok(
            sqlx::query_scalar("select exists (select 1 from install_settings where name = $1)")
                .bind(SETUP_COMPLETED)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    /// Records that the first admin has been made. Refused (`Conflict`) when
    /// it already was, so `/setup` works once.
    pub async fn complete_setup(&self, now: OffsetDateTime) -> Result<()> {
        let inserted = sqlx::query(
            "insert into install_settings (name, value, updated_at) values ($1, '', $2) \
             on conflict (name) do nothing",
        )
        .bind(SETUP_COMPLETED)
        .bind(now)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if inserted == 0 {
            return Err(StoreError::Conflict("setup is already complete".into()));
        }
        Ok(())
    }

    /// Sekisho's client secret at Dex: made on first use, kept sealed, the
    /// same for every replica.
    pub async fn dex_client_secret(&self, secret_box: &SecretBox) -> Result<String> {
        let candidate = random_secret();
        sqlx::query(
            "insert into install_settings (name, value) values ($1, $2) \
             on conflict (name) do nothing",
        )
        .bind(DEX_CLIENT_SECRET)
        .bind(secret_box.seal(candidate.as_bytes()))
        .execute(&self.pool)
        .await?;
        let sealed: Vec<u8> =
            sqlx::query_scalar("select value from install_settings where name = $1")
                .bind(DEX_CLIENT_SECRET)
                .fetch_one(&self.pool)
                .await?;
        String::from_utf8(secret_box.open(&sealed)?)
            .map_err(|_| bad_column("Dex client secret", "not UTF-8"))
    }
}
