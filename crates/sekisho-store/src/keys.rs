use sekisho_core::keys::{EnvKeyId, KeyState, effective_state, generate_key};
use sekisho_core::names::EnvName;
use sqlx::Row;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;

use crate::error::{FieldError, bad_column, check, violates};
use crate::notify::{ChangeKind, notify};
use crate::{Result, Store, StoreError};

/// An environment key, without its secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvKey {
    pub id: EnvKeyId,
    pub environment: EnvName,
    pub owner: String,
    pub name: String,
    pub expires_at: Option<OffsetDateTime>,
    pub suspended: bool,
    pub revoked_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
}

impl EnvKey {
    pub fn state(&self, now: OffsetDateTime) -> KeyState {
        effective_state(self.revoked_at, self.suspended, self.expires_at, now)
    }
}

/// Expands to the key query up to its filter.
macro_rules! select_keys {
    () => {
        "select k.id, e.name as environment, k.owner, k.name, k.expires_at, k.suspended, \
         k.revoked_at, k.created_at, k.secret_sha256 \
         from env_keys k join environments e on e.id = k.environment_id"
    };
}

const NAME_MAX_CHARS: usize = 100;

fn validate(owner: &str, name: &str, expires_at: Option<OffsetDateTime>) -> Vec<FieldError> {
    let mut errors = Vec::new();
    if owner.trim().is_empty() {
        errors.push(FieldError::new("owner", "must not be empty"));
    }
    if name.trim().is_empty() {
        errors.push(FieldError::new("name", "must not be empty"));
    } else if name.chars().count() > NAME_MAX_CHARS {
        errors.push(FieldError::new(
            "name",
            format!("use at most {NAME_MAX_CHARS} characters"),
        ));
    } else if name.chars().any(char::is_control) {
        errors.push(FieldError::new(
            "name",
            "must not contain control characters",
        ));
    }
    if expires_at.is_some_and(|at| at <= OffsetDateTime::now_utc()) {
        errors.push(FieldError::new("expires_at", "must be in the future"));
    }
    errors
}

fn from_row(row: &PgRow) -> Result<(EnvKey, [u8; 32])> {
    let id: String = row.try_get("id")?;
    let environment: String = row.try_get("environment")?;
    let sha: Vec<u8> = row.try_get("secret_sha256")?;
    let key = EnvKey {
        id: EnvKeyId::parse(&id).ok_or_else(|| bad_column("key id", &id))?,
        environment: EnvName::parse(&environment)
            .map_err(|_| bad_column("environment name", &environment))?,
        owner: row.try_get("owner")?,
        name: row.try_get("name")?,
        expires_at: row.try_get("expires_at")?,
        suspended: row.try_get("suspended")?,
        revoked_at: row.try_get("revoked_at")?,
        created_at: row.try_get("created_at")?,
    };
    let sha = sha
        .try_into()
        .map_err(|_| bad_column("secret hash", "not 32 bytes"))?;
    Ok((key, sha))
}

impl Store {
    /// Makes a key for `environment`. The token is returned once, here; only
    /// the hash of its secret is stored.
    pub async fn create_key(
        &self,
        environment: &EnvName,
        owner: &str,
        name: &str,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<(EnvKey, String)> {
        check(validate(owner, name, expires_at))?;
        let generated = generate_key();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "insert into env_keys (id, environment_id, owner, name, secret_sha256, expires_at) \
             select $1, e.id, $3, $4, $5, $6 from environments e where e.name = $2 \
             returning id",
        )
        .bind(generated.id.as_str())
        .bind(environment.as_str())
        .bind(owner)
        .bind(name)
        .bind(&generated.secret_sha256[..])
        .bind(expires_at)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| {
            if violates(&e, "env_keys_environment_id_name_key") {
                StoreError::Conflict(format!("{environment} already has a key named {name}"))
            } else {
                e.into()
            }
        })?
        .ok_or(StoreError::NotFound)?;
        notify(&mut tx, ChangeKind::Keys).await?;
        // Read back so timestamps carry the database's precision.
        let (key, _) = from_row(
            &sqlx::query(concat!(select_keys!(), " where k.id = $1"))
                .bind(generated.id.as_str())
                .fetch_one(&mut *tx)
                .await?,
        )?;
        tx.commit().await?;
        Ok((key, generated.token))
    }

    /// The environment's keys, oldest first.
    pub async fn list_keys(&self, environment: &EnvName) -> Result<Vec<EnvKey>> {
        let mut tx = self.pool.begin().await?;
        let exists: bool =
            sqlx::query_scalar("select exists (select 1 from environments where name = $1)")
                .bind(environment.as_str())
                .fetch_one(&mut *tx)
                .await?;
        if !exists {
            return Err(StoreError::NotFound);
        }
        let rows = sqlx::query(concat!(
            select_keys!(),
            " where e.name = $1 order by k.created_at, k.name"
        ))
        .bind(environment.as_str())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter().map(|r| Ok(from_row(r)?.0)).collect()
    }

    /// The key and the SHA-256 of its secret, for checking a presented token.
    pub async fn find_key(&self, id: &EnvKeyId) -> Result<Option<(EnvKey, [u8; 32])>> {
        sqlx::query(concat!(select_keys!(), " where k.id = $1"))
            .bind(id.as_str())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(from_row)
            .transpose()
    }

    /// Suspends or resumes a key. A revoked key stays revoked: `Conflict("revoked")`.
    pub async fn set_suspended(&self, id: &EnvKeyId, suspended: bool) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let revoked_at: Option<OffsetDateTime> =
            sqlx::query_scalar("select revoked_at from env_keys where id = $1 for update")
                .bind(id.as_str())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(StoreError::NotFound)?;
        if revoked_at.is_some() {
            return Err(StoreError::Conflict("revoked".into()));
        }
        sqlx::query("update env_keys set suspended = $2 where id = $1")
            .bind(id.as_str())
            .bind(suspended)
            .execute(&mut *tx)
            .await?;
        notify(&mut tx, ChangeKind::Keys).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Revokes a key for good. Revoking again keeps the first time.
    pub async fn revoke_key(&self, id: &EnvKeyId) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "update env_keys set revoked_at = coalesce(revoked_at, now()) where id = $1",
        )
        .bind(id.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 0 {
            return Err(StoreError::NotFound);
        }
        notify(&mut tx, ChangeKind::Keys).await?;
        tx.commit().await?;
        Ok(())
    }
}
