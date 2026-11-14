//! One-time links for setting a local account's password. Only the SHA-256
//! of a link's token is stored.

use std::fmt;

use sekisho_core::keys::{hash_secret, random_secret};
use sqlx::{PgConnection, Postgres, Transaction};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::users::{User, read_user};
use crate::{Result, Store, StoreError};

/// A setup link held for its use: another claim of the same link waits
/// until this one is used up, then finds it used, or until this one is
/// dropped, which leaves the link as it was.
pub struct SetupClaim {
    tx: Transaction<'static, Postgres>,
    token_sha256: [u8; 32],
    pub user: User,
    pub make_admin: bool,
}

/// Shows the user and the admin mark only.
impl fmt::Debug for SetupClaim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SetupClaim")
            .field("user", &self.user.id)
            .field("make_admin", &self.make_admin)
            .finish_non_exhaustive()
    }
}

impl SetupClaim {
    /// Uses the link up.
    pub async fn use_up(mut self, now: OffsetDateTime) -> Result<()> {
        sqlx::query("update setup_links set used_at = $2 where token_sha256 = $1")
            .bind(&self.token_sha256[..])
            .bind(now)
            .execute(&mut *self.tx)
            .await?;
        self.tx.commit().await?;
        Ok(())
    }
}

/// Deletes the user's unused links, inside the caller's transaction: when
/// they are disabled, after their sessions.
pub(crate) async fn delete_unused_in(conn: &mut PgConnection, user: &Uuid) -> Result<()> {
    sqlx::query("delete from setup_links where user_id = $1 and used_at is null")
        .bind(user)
        .execute(conn)
        .await?;
    Ok(())
}

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

    /// The enabled user an unused, unexpired link is for, and whether it
    /// makes them an admin.
    pub async fn setup_link(
        &self,
        token: &str,
        now: OffsetDateTime,
    ) -> Result<Option<(User, bool)>> {
        let mut conn = self.pool.acquire().await?;
        let link: Option<(Uuid, bool)> = sqlx::query_as(
            "select l.user_id, l.make_admin from setup_links l join users u on u.id = l.user_id \
             where l.token_sha256 = $1 and l.used_at is null and l.expires_at > $2 \
             and u.disabled_at is null",
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

    /// Holds an unused, unexpired link of an enabled user for its use
    /// ([`SetupClaim`]); `None` if there is none.
    pub async fn claim_setup_link(
        &self,
        token: &str,
        now: OffsetDateTime,
    ) -> Result<Option<SetupClaim>> {
        let token_sha256 = hash_secret(token);
        let mut tx = self.pool.begin().await?;
        let link: Option<(Uuid, bool)> = sqlx::query_as(
            "select l.user_id, l.make_admin from setup_links l join users u on u.id = l.user_id \
             where l.token_sha256 = $1 and l.used_at is null and l.expires_at > $2 \
             and u.disabled_at is null for update of l",
        )
        .bind(&token_sha256[..])
        .bind(now)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((user, make_admin)) = link else {
            return Ok(None);
        };
        let user = read_user(&mut tx, &user).await?;
        Ok(Some(SetupClaim {
            tx,
            token_sha256,
            user,
            make_admin,
        }))
    }
}
