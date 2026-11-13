//! One-time links for setting a local account's password. Only the SHA-256
//! of a link's token is stored.

use sekisho_core::keys::{hash_secret, random_secret};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::users::{User, read_user};
use crate::{Result, Store, StoreError};

/// How long a setup link works, once.
pub const SETUP_LINK_LIFETIME: Duration = Duration::days(7);

impl Store {
    /// Makes a setup link for `user`, replacing their unused ones. Returns the
    /// token, shown once, and when the link expires.
    pub async fn create_setup_link(
        &self,
        user: Uuid,
        make_admin: bool,
        now: OffsetDateTime,
    ) -> Result<(String, OffsetDateTime)> {
        let token = random_secret();
        let expires_at = now + SETUP_LINK_LIFETIME;
        let mut tx = self.pool.begin().await?;
        let known: Option<Uuid> =
            sqlx::query_scalar("select id from users where id = $1 for share")
                .bind(user)
                .fetch_optional(&mut *tx)
                .await?;
        if known.is_none() {
            return Err(StoreError::NotFound);
        }
        sqlx::query("delete from setup_links where user_id = $1 and used_at is null")
            .bind(user)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "insert into setup_links (token_sha256, user_id, make_admin, expires_at, created_at) \
             values ($1, $2, $3, $4, $5)",
        )
        .bind(&hash_secret(&token)[..])
        .bind(user)
        .bind(make_admin)
        .bind(expires_at)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok((token, expires_at))
    }

    /// The user an unused, unexpired link is for, and whether it makes them
    /// an admin.
    pub async fn setup_link(
        &self,
        token: &str,
        now: OffsetDateTime,
    ) -> Result<Option<(User, bool)>> {
        let mut conn = self.pool.acquire().await?;
        let link: Option<(Uuid, bool)> = sqlx::query_as(
            "select user_id, make_admin from setup_links \
             where token_sha256 = $1 and used_at is null and expires_at > $2",
        )
        .bind(&hash_secret(token)[..])
        .bind(now)
        .fetch_optional(&mut *conn)
        .await?;
        match link {
            Some((user, make_admin)) => Ok(Some((read_user(&mut conn, &user).await?, make_admin))),
            None => Ok(None),
        }
    }

    /// Uses a link up; `false` if it was used, expired or unknown.
    pub async fn use_setup_link(&self, token: &str, now: OffsetDateTime) -> Result<bool> {
        let used = sqlx::query(
            "update setup_links set used_at = $2 \
             where token_sha256 = $1 and used_at is null and expires_at > $2",
        )
        .bind(&hash_secret(token)[..])
        .bind(now)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(used > 0)
    }
}
