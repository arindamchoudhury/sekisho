use std::collections::HashMap;

use sekisho_core::access::{Access, can_use};
use sekisho_core::keys::{EnvKeyId, KeyState, effective_state, generate_key, with_access};
use sekisho_core::names::EnvName;
use sekisho_core::session_token::PublicKey;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{FieldError, bad_column, check, check_label, violates};
use crate::notify::{ChangeKind, notify};
use crate::sessions::ENDED_SESSIONS_KEPT;
use crate::signing_keys;
use crate::users::{UserAccess, accesses, user_accesses};
use crate::{Result, Store, StoreError};

/// An environment key, without its secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvKey {
    pub id: EnvKeyId,
    pub environment: EnvName,
    /// The user the key belongs to.
    pub owner: Uuid,
    pub name: String,
    pub expires_at: Option<OffsetDateTime>,
    pub suspended: bool,
    pub revoked_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
    /// Whether the owner may use the key's environment, as read: not
    /// disabled, not refused at the identity provider, and in one of its
    /// access groups.
    pub owner_can_use: bool,
}

impl EnvKey {
    /// The key's state at `now`; an active key is `Invalid` while its owner
    /// may not use the environment.
    pub fn state(&self, now: OffsetDateTime) -> KeyState {
        with_access(
            effective_state(self.revoked_at, self.suspended, self.expires_at, now),
            self.owner_can_use,
        )
    }
}

/// An environment as the auth cache needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvAccess {
    pub name: EnvName,
    pub access_groups: Vec<String>,
    pub owner: Option<Uuid>,
}

/// Everything the hooks' auth cache holds, read at one point in time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthSnapshot {
    /// Every key, revoked ones included, with the SHA-256 of its secret.
    pub keys: Vec<(EnvKey, [u8; 32])>,
    /// Every environment, by name.
    pub environments: Vec<EnvAccess>,
    /// Every user, by ID.
    pub users: Vec<UserAccess>,
    /// Sessions ended within [`ENDED_SESSIONS_KEPT`], by ID.
    pub ended_sessions: Vec<Uuid>,
    /// The signing keys published at the snapshot's time.
    pub public_keys: Vec<PublicKey>,
}

/// Expands to the key query up to its filter.
macro_rules! select_keys {
    () => {
        "select k.id, e.name as environment, e.access_groups, k.owner_id, k.name, k.expires_at, \
         k.suspended, k.revoked_at, k.created_at, k.secret_sha256 \
         from env_keys k join environments e on e.id = k.environment_id"
    };
}

fn validate(name: &str, expires_at: Option<OffsetDateTime>) -> Vec<FieldError> {
    let mut errors = Vec::new();
    check_label(&mut errors, "name", name);
    if expires_at.is_some_and(|at| at <= OffsetDateTime::now_utc()) {
        errors.push(FieldError::new("expires_at", "must be in the future"));
    }
    errors
}

/// A key as read, before its owner's access is known.
struct KeyRow {
    key: EnvKey,
    secret_sha256: [u8; 32],
    access_groups: Vec<String>,
}

fn from_row(row: &PgRow) -> Result<KeyRow> {
    let id: String = row.try_get("id")?;
    let environment: String = row.try_get("environment")?;
    let sha: Vec<u8> = row.try_get("secret_sha256")?;
    let key = EnvKey {
        id: EnvKeyId::parse(&id).ok_or_else(|| bad_column("key id", &id))?,
        environment: EnvName::parse(&environment)
            .map_err(|_| bad_column("environment name", &environment))?,
        owner: row.try_get("owner_id")?,
        name: row.try_get("name")?,
        expires_at: row.try_get("expires_at")?,
        suspended: row.try_get("suspended")?,
        revoked_at: row.try_get("revoked_at")?,
        created_at: row.try_get("created_at")?,
        owner_can_use: false,
    };
    let secret_sha256 = sha
        .try_into()
        .map_err(|_| bad_column("secret hash", "not 32 bytes"))?;
    Ok(KeyRow {
        key,
        secret_sha256,
        access_groups: row.try_get("access_groups")?,
    })
}

/// Fills in each key's `owner_can_use` from its owner's access.
fn with_owners(rows: Vec<KeyRow>, access: &HashMap<Uuid, Access>) -> Vec<(EnvKey, [u8; 32])> {
    rows.into_iter()
        .map(|mut r| {
            r.key.owner_can_use = access
                .get(&r.key.owner)
                .is_some_and(|a| can_use(a, &r.access_groups));
            (r.key, r.secret_sha256)
        })
        .collect()
}

/// The keys `rows` read, with their owners' access read on `conn`.
async fn keys_with_owners(
    conn: &mut PgConnection,
    rows: &[PgRow],
) -> Result<Vec<(EnvKey, [u8; 32])>> {
    let rows = rows.iter().map(from_row).collect::<Result<Vec<_>>>()?;
    let mut owners: Vec<Uuid> = rows.iter().map(|r| r.key.owner).collect();
    owners.sort();
    owners.dedup();
    let access = accesses(conn, &owners).await?;
    Ok(with_owners(rows, &access))
}

impl Store {
    /// Makes a key for `environment`, owned by `owner`, who must be able to
    /// use it. The token is returned once, here; only the hash of its secret
    /// is stored.
    pub async fn create_key(
        &self,
        environment: &EnvName,
        owner: Uuid,
        name: &str,
        expires_at: Option<OffsetDateTime>,
    ) -> Result<(EnvKey, String)> {
        check(validate(name, expires_at))?;
        let mut tx = self.pool.begin().await?;
        // Users before environments: offboarding the owner waits for this
        // key, then revokes it.
        let known: Option<Uuid> =
            sqlx::query_scalar("select id from users where id = $1 for share")
                .bind(owner)
                .fetch_optional(&mut *tx)
                .await?;
        let env: Option<(Uuid, Vec<String>)> = sqlx::query_as(
            "select id, access_groups from environments where name = $1 for key share",
        )
        .bind(environment.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let (environment_id, access_groups) = env.ok_or(StoreError::NotFound)?;
        if known.is_none() {
            return Err(StoreError::Validation(vec![FieldError::new(
                "owner",
                format!("no user {owner}"),
            )]));
        }
        let access = accesses(&mut tx, &[owner])
            .await?
            .remove(&owner)
            .unwrap_or_default();
        if !can_use(&access, &access_groups) {
            let why = if access.disabled {
                "the owner is disabled".to_owned()
            } else if access.idp_refused {
                "the owner was refused at their identity provider".to_owned()
            } else {
                format!("the owner is in none of {environment}'s access groups")
            };
            return Err(StoreError::Validation(vec![FieldError::new(
                "environment",
                why,
            )]));
        }
        let generated = generate_key();
        sqlx::query(
            "insert into env_keys (id, environment_id, owner_id, name, secret_sha256, expires_at) \
             values ($1, $2, $3, $4, $5, $6)",
        )
        .bind(generated.id.as_str())
        .bind(environment_id)
        .bind(owner)
        .bind(name)
        .bind(&generated.secret_sha256[..])
        .bind(expires_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if violates(&e, "env_keys_environment_id_name_key") {
                StoreError::Conflict(format!("{environment} already has a key named {name}"))
            } else {
                e.into()
            }
        })?;
        notify(&mut tx, ChangeKind::Keys).await?;
        // Read back so timestamps carry the database's precision.
        let rows = sqlx::query(concat!(select_keys!(), " where k.id = $1"))
            .bind(generated.id.as_str())
            .fetch_all(&mut *tx)
            .await?;
        let (key, _) = keys_with_owners(&mut tx, &rows)
            .await?
            .pop()
            .ok_or(StoreError::NotFound)?;
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
        let keys = keys_with_owners(&mut tx, &rows).await?;
        tx.commit().await?;
        Ok(keys.into_iter().map(|(key, _)| key).collect())
    }

    /// The user's keys in every environment, oldest first.
    pub async fn list_user_keys(&self, owner: &Uuid) -> Result<Vec<EnvKey>> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(concat!(
            select_keys!(),
            " where k.owner_id = $1 order by k.created_at, k.name"
        ))
        .bind(owner)
        .fetch_all(&mut *tx)
        .await?;
        let keys = keys_with_owners(&mut tx, &rows).await?;
        tx.commit().await?;
        Ok(keys.into_iter().map(|(key, _)| key).collect())
    }

    /// Every key, environment, user and recently ended session in one
    /// consistent read, for the auth cache.
    pub async fn auth_snapshot(&self, now: OffsetDateTime) -> Result<AuthSnapshot> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("set transaction isolation level repeatable read, read only")
            .execute(&mut *tx)
            .await?;
        let key_rows = sqlx::query(concat!(select_keys!(), " order by k.id"))
            .fetch_all(&mut *tx)
            .await?
            .iter()
            .map(from_row)
            .collect::<Result<Vec<_>>>()?;
        let environments: Vec<(String, Vec<String>, Option<Uuid>)> =
            sqlx::query_as("select name, access_groups, owner_id from environments order by name")
                .fetch_all(&mut *tx)
                .await?;
        let users = user_accesses(&mut tx, None).await?;
        let ended_sessions: Vec<Uuid> =
            sqlx::query_scalar("select id from sessions where ended_at >= $1 order by id")
                .bind(now - ENDED_SESSIONS_KEPT)
                .fetch_all(&mut *tx)
                .await?;
        let public_keys = signing_keys::published(&mut tx, now).await?;
        tx.commit().await?;
        let access: HashMap<Uuid, Access> =
            users.iter().map(|u| (u.id, u.access.clone())).collect();
        let environments = environments
            .into_iter()
            .map(|(name, access_groups, owner)| {
                Ok(EnvAccess {
                    name: EnvName::parse(&name)
                        .map_err(|_| bad_column("environment name", &name))?,
                    access_groups,
                    owner,
                })
            })
            .collect::<Result<_>>()?;
        Ok(AuthSnapshot {
            keys: with_owners(key_rows, &access),
            environments,
            users,
            ended_sessions,
            public_keys,
        })
    }

    /// The key and the SHA-256 of its secret, for checking a presented token.
    pub async fn find_key(&self, id: &EnvKeyId) -> Result<Option<(EnvKey, [u8; 32])>> {
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(concat!(select_keys!(), " where k.id = $1"))
            .bind(id.as_str())
            .fetch_all(&mut *tx)
            .await?;
        let mut keys = keys_with_owners(&mut tx, &rows).await?;
        tx.commit().await?;
        Ok(keys.pop())
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
