//! Each user's one Dex refresh token, sealed, and the daily check that
//! replays it for people who own active environment keys.
//!
//! Lock order everywhere: a session row before the user's Dex token row.

use std::fmt;

use sekisho_core::secret_box::SecretBox;
use sqlx::{PgConnection, Postgres, Transaction};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::error::bad_column;
use crate::notify::{ChangeKind, notify};
use crate::sessions::end_user_sessions;
use crate::{Result, Store, StoreError};

/// How often each owner of an active environment key is checked at Dex.
pub const IDP_CHECK_EVERY: Duration = Duration::hours(24);

fn open(secret_box: &SecretBox, sealed: &[u8]) -> Result<String> {
    let plain = secret_box.open(sealed)?;
    String::from_utf8(plain).map_err(|_| bad_column("Dex refresh token", "not UTF-8"))
}

async fn put(
    conn: &mut PgConnection,
    secret_box: &SecretBox,
    user: &Uuid,
    token: &str,
) -> Result<()> {
    sqlx::query(
        "insert into dex_tokens (user_id, refresh_ciphertext) values ($1, $2) \
         on conflict (user_id) do update \
         set refresh_ciphertext = excluded.refresh_ciphertext, updated_at = now()",
    )
    .bind(user)
    .bind(secret_box.seal(token.as_bytes()))
    .execute(conn)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(d) if d.is_foreign_key_violation() => StoreError::NotFound,
        e => e.into(),
    })?;
    Ok(())
}

pub(crate) async fn delete_in(conn: &mut PgConnection, user: &Uuid) -> Result<()> {
    sqlx::query("delete from dex_tokens where user_id = $1")
        .bind(user)
        .execute(conn)
        .await?;
    Ok(())
}

/// A user's Dex token row, locked (`FOR UPDATE`) in a transaction of its
/// own, so only one refresh replays it at a time. Dropping it without
/// [`DexTokenLock::commit`] changes nothing.
pub struct DexTokenLock {
    tx: Transaction<'static, Postgres>,
    secret_box: SecretBox,
    user: Uuid,
    token: Option<String>,
}

/// Shows whether there is a token, not the token.
impl fmt::Debug for DexTokenLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DexTokenLock")
            .field("user", &self.user)
            .field("has_token", &self.token.is_some())
            .finish_non_exhaustive()
    }
}

impl DexTokenLock {
    /// The stored token, or `None` when the user has none.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Stores the token Dex rotated to.
    pub async fn replace(&mut self, token: &str) -> Result<()> {
        put(&mut self.tx, &self.secret_box, &self.user, token).await?;
        self.token = Some(token.to_owned());
        Ok(())
    }

    pub async fn delete(&mut self) -> Result<()> {
        delete_in(&mut self.tx, &self.user).await?;
        self.token = None;
        Ok(())
    }

    pub async fn commit(self) -> Result<()> {
        self.tx.commit().await?;
        Ok(())
    }
}

impl Store {
    /// Stores the user's Dex refresh token, replacing any earlier one.
    pub async fn save_dex_token(
        &self,
        secret_box: &SecretBox,
        user: Uuid,
        token: &str,
    ) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        put(&mut conn, secret_box, &user, token).await
    }

    pub async fn delete_dex_token(&self, user: &Uuid) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        delete_in(&mut conn, user).await
    }

    /// Locks the user's Dex token for a refresh. A second lock waits until
    /// the first is committed or dropped, then sees what it left.
    pub async fn lock_dex_token(&self, secret_box: &SecretBox, user: Uuid) -> Result<DexTokenLock> {
        let mut tx = self.pool.begin().await?;
        let sealed: Option<Vec<u8>> = sqlx::query_scalar(
            "select refresh_ciphertext from dex_tokens where user_id = $1 for update",
        )
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?;
        let token = sealed.map(|s| open(secret_box, &s)).transpose()?;
        Ok(DexTokenLock {
            tx,
            secret_box: secret_box.clone(),
            user,
            token,
        })
    }

    /// Claims up to `limit` users due for the daily check at Dex: enabled,
    /// with an active, unexpired environment key and a Dex token, and not
    /// checked within [`IDP_CHECK_EVERY`]. Claimed users are marked checked
    /// at `now`; concurrent sweeps claim different users.
    pub async fn claim_idp_checks(&self, now: OffsetDateTime, limit: i64) -> Result<Vec<Uuid>> {
        let mut claimed: Vec<Uuid> = sqlx::query_scalar(
            "update users set idp_checked_at = $1 where id in ( \
               select u.id from users u \
               where u.disabled_at is null \
                 and (u.idp_checked_at is null or u.idp_checked_at <= $2) \
                 and exists (select 1 from dex_tokens t where t.user_id = u.id) \
                 and exists (select 1 from env_keys k where k.owner_id = u.id \
                             and k.revoked_at is null and not k.suspended \
                             and (k.expires_at is null or k.expires_at > $1)) \
               order by u.idp_checked_at nulls first, u.id \
               limit $3 \
               for update of u skip locked) \
             returning id",
        )
        .bind(now)
        .bind(now - IDP_CHECK_EVERY)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        claimed.sort();
        Ok(claimed)
    }

    /// Gives a claim back, so the next sweep checks the user again.
    pub async fn release_idp_check(&self, user: &Uuid) -> Result<()> {
        sqlx::query("update users set idp_checked_at = null where id = $1")
            .bind(user)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The identity provider refused the user: ends their sessions
    /// (`idp_refused`), deletes their Dex token, and makes their keys invalid
    /// until they sign in again.
    pub async fn mark_idp_refused(&self, user: Uuid, now: OffsetDateTime) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "update users set idp_refused_at = coalesce(idp_refused_at, $2) where id = $1",
        )
        .bind(user)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 0 {
            return Err(StoreError::NotFound);
        }
        // Sessions before the Dex token, as everywhere.
        end_user_sessions(&mut tx, &user, "idp_refused", now).await?;
        delete_in(&mut tx, &user).await?;
        notify(&mut tx, ChangeKind::Users).await?;
        tx.commit().await?;
        Ok(())
    }
}
