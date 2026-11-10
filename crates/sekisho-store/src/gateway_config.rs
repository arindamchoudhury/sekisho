//! Published agentgateway configs, one version per distinct content.
//!
//! A config holds decrypted provider keys, so it is stored sealed with the
//! install's `SecretBox`, and only the newest [`KEEP_VERSIONS`] are kept.

use sekisho_core::secret_box::{SecretBox, SecretError};

use crate::{Result, Store, StoreError};

/// The advisory lock that serialises config writers across replicas: the
/// ASCII bytes of "sekisho".
const GATEWAY_CONFIG_LOCK: i64 = 0x0073_656b_6973_686f;

/// How many versions stay stored. Agents only ever fetch the newest.
pub const KEEP_VERSIONS: i64 = 10;

fn open(secret_box: &SecretBox, sealed: &[u8]) -> Result<String> {
    let yaml = secret_box.open(sealed)?;
    String::from_utf8(yaml).map_err(|_| StoreError::Secret(SecretError::Decrypt))
}

impl Store {
    /// Saves `yaml` as a new version unless it equals the latest one, and
    /// returns the latest version either way. Older versions beyond
    /// [`KEEP_VERSIONS`] are deleted.
    pub async fn save_gateway_config(&self, secret_box: &SecretBox, yaml: &str) -> Result<i64> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("select pg_advisory_xact_lock($1)")
            .bind(GATEWAY_CONFIG_LOCK)
            .execute(&mut *tx)
            .await?;
        let latest: Option<(i64, Vec<u8>)> = sqlx::query_as(
            "select version, yaml_ciphertext from gateway_config order by version desc limit 1",
        )
        .fetch_optional(&mut *tx)
        .await?;
        // Compared after decrypting, so no plaintext digest is stored. A
        // latest version sealed under another key counts as different.
        if let Some((version, sealed)) = latest
            && open(secret_box, &sealed).ok().as_deref() == Some(yaml)
        {
            return Ok(version);
        }
        let version: i64 = sqlx::query_scalar(
            "insert into gateway_config (yaml_ciphertext) values ($1) returning version",
        )
        .bind(secret_box.seal(yaml.as_bytes()))
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("delete from gateway_config where version <= $1")
            .bind(version - KEEP_VERSIONS)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(version)
    }

    /// The newest config if it is newer than `after`.
    pub async fn gateway_config_after(
        &self,
        secret_box: &SecretBox,
        after: i64,
    ) -> Result<Option<(i64, String)>> {
        let newest: Option<(i64, Vec<u8>)> = sqlx::query_as(
            "select version, yaml_ciphertext from gateway_config where version > $1 \
             order by version desc limit 1",
        )
        .bind(after)
        .fetch_optional(&self.pool)
        .await?;
        newest
            .map(|(version, sealed)| Ok((version, open(secret_box, &sealed)?)))
            .transpose()
    }
}
