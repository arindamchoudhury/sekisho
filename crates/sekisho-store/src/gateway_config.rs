//! Published agentgateway configs, one version per distinct content.

use crate::{Result, Store};

/// The advisory lock that serialises config writers across replicas: the
/// ASCII bytes of "sekisho".
const GATEWAY_CONFIG_LOCK: i64 = 0x0073_656b_6973_686f;

impl Store {
    /// Saves `yaml` as a new version unless it equals the latest one, and
    /// returns the latest version either way.
    pub async fn save_gateway_config(&self, yaml: &str) -> Result<i64> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("select pg_advisory_xact_lock($1)")
            .bind(GATEWAY_CONFIG_LOCK)
            .execute(&mut *tx)
            .await?;
        let unchanged: Option<i64> = sqlx::query_scalar(
            "select version from gateway_config \
             where version = (select max(version) from gateway_config) \
             and sha256 = sha256(convert_to($1, 'UTF8'))",
        )
        .bind(yaml)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(version) = unchanged {
            return Ok(version);
        }
        let version: i64 = sqlx::query_scalar(
            "insert into gateway_config (yaml, sha256) \
             values ($1, sha256(convert_to($1, 'UTF8'))) returning version",
        )
        .bind(yaml)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(version)
    }

    /// The newest config if it is newer than `after`.
    pub async fn gateway_config_after(&self, after: i64) -> Result<Option<(i64, String)>> {
        Ok(sqlx::query_as(
            "select version, yaml from gateway_config where version > $1 \
             order by version desc limit 1",
        )
        .bind(after)
        .fetch_optional(&self.pool)
        .await?)
    }
}
