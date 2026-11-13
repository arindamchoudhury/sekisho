//! Sessions: made at sign-in, kept alive by rotating refresh tokens, ended
//! by logout, disabling, offboarding, reuse, idleness or a refusal at the
//! identity provider.
//!
//! Refresh tokens are 32 random bytes, stored only as SHA-256. Each refresh
//! rotates the token and keeps the previous hash, so a token presented again
//! is recognised: within [`REUSE_GRACE`] (two processes refreshing at once)
//! it is only refused; later it ends the session.

use std::fmt;

use sekisho_core::keys::{hash_secret, random_secret};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Postgres, Row, Transaction};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::notify::{ChangeKind, notify};
use crate::{Result, Store};

/// How long an ended session stays in the auth cache: the 1-hour access
/// token's life plus a margin, so a token of an ended session is refused
/// until it expires anyway.
pub const ENDED_SESSIONS_KEPT: Duration = Duration::minutes(65);
/// A session ends after this long without a refresh.
pub const SESSION_IDLE: Duration = Duration::days(30);
/// A rotated-away refresh token presented within this is refused, not
/// treated as theft.
pub const REUSE_GRACE: Duration = Duration::seconds(10);

/// An open or ended session, without its refresh token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub id: Uuid,
    pub user_id: Uuid,
    pub client_id: String,
    pub created_at: OffsetDateTime,
    pub idle_expires_at: OffsetDateTime,
    pub last_refreshed_at: Option<OffsetDateTime>,
}

/// The columns [`from_row`] reads, with `ended_at` and `rotated_at`.
macro_rules! session_columns {
    () => {
        "id, user_id, client_id, created_at, idle_expires_at, last_refreshed_at, ended_at, \
         rotated_at"
    };
}

fn from_row(row: &PgRow) -> Result<Session> {
    Ok(Session {
        id: row.try_get("id")?,
        user_id: row.try_get("user_id")?,
        client_id: row.try_get("client_id")?,
        created_at: row.try_get("created_at")?,
        idle_expires_at: row.try_get("idle_expires_at")?,
        last_refreshed_at: row.try_get("last_refreshed_at")?,
    })
}

/// What a presented refresh token is.
#[derive(Debug)]
pub enum RefreshCheck {
    /// The session's current token: its row is locked until the lock is
    /// committed or dropped.
    Live(RefreshLock),
    Unknown,
    /// Issued to another client.
    OtherClient,
    /// Its session has ended.
    Ended,
    /// Its session went 30 days without a refresh, and has now ended.
    Idle,
    /// The previous token, presented again within the grace: refused.
    ReusedInGrace,
    /// The previous token, presented again after the grace: the session has
    /// ended.
    Reused,
}

/// A live session's row, locked (`FOR UPDATE`) in a transaction of its own.
/// Dropping it without [`RefreshLock::commit`] changes nothing.
pub struct RefreshLock {
    tx: Transaction<'static, Postgres>,
    session: Session,
}

/// Shows the session only.
impl fmt::Debug for RefreshLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefreshLock")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl RefreshLock {
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Gives the session a new refresh token, keeping the old one's hash as
    /// the previous, and moves its idle limit to `now` + [`SESSION_IDLE`].
    pub async fn rotate(&mut self, now: OffsetDateTime) -> Result<String> {
        let token = random_secret();
        let idle_expires_at = now + SESSION_IDLE;
        sqlx::query(
            "update sessions set previous_refresh_sha256 = refresh_sha256, \
             refresh_sha256 = $2, rotated_at = $3, last_refreshed_at = $3, \
             idle_expires_at = $4 where id = $1",
        )
        .bind(self.session.id)
        .bind(&hash_secret(&token)[..])
        .bind(now)
        .bind(idle_expires_at)
        .execute(&mut *self.tx)
        .await?;
        self.session.idle_expires_at = idle_expires_at;
        self.session.last_refreshed_at = Some(now);
        Ok(token)
    }

    /// Ends the session.
    pub async fn end(&mut self, reason: &str, now: OffsetDateTime) -> Result<()> {
        end_session_in(&mut self.tx, &self.session.id, reason, now).await?;
        Ok(())
    }

    pub async fn commit(self) -> Result<()> {
        self.tx.commit().await?;
        Ok(())
    }
}

async fn end_session_in(
    conn: &mut PgConnection,
    session: &Uuid,
    reason: &str,
    now: OffsetDateTime,
) -> Result<bool> {
    let ended = sqlx::query(
        "update sessions set ended_at = $3, end_reason = $2 \
         where id = $1 and ended_at is null",
    )
    .bind(session)
    .bind(reason)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if ended > 0 {
        notify(conn, ChangeKind::Sessions).await?;
    }
    Ok(ended > 0)
}

/// Ends every open session of the user, inside the caller's transaction.
/// Announces `Sessions` if any ended.
pub async fn end_user_sessions(
    conn: &mut PgConnection,
    user: &Uuid,
    reason: &str,
    now: OffsetDateTime,
) -> Result<u64> {
    let ended = sqlx::query(
        "update sessions set ended_at = $3, end_reason = $2 \
         where user_id = $1 and ended_at is null",
    )
    .bind(user)
    .bind(reason)
    .bind(now)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if ended > 0 {
        notify(conn, ChangeKind::Sessions).await?;
    }
    Ok(ended)
}

impl Store {
    /// Opens a session for `user` through `client_id`, and returns it with
    /// its first refresh token, which is shown once.
    pub async fn create_session(
        &self,
        user: Uuid,
        client_id: &str,
        now: OffsetDateTime,
    ) -> Result<(Session, String)> {
        let token = random_secret();
        let row = sqlx::query(concat!(
            "insert into sessions (user_id, client_id, refresh_sha256, idle_expires_at, \
             created_at) values ($1, $2, $3, $4, $5) returning ",
            session_columns!()
        ))
        .bind(user)
        .bind(client_id)
        .bind(&hash_secret(&token)[..])
        .bind(now + SESSION_IDLE)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;
        Ok((from_row(&row)?, token))
    }

    /// Looks up a presented refresh token. A live session comes back locked,
    /// for the caller to re-check the person and rotate the token.
    pub async fn begin_refresh(
        &self,
        presented: &str,
        client_id: &str,
        now: OffsetDateTime,
    ) -> Result<RefreshCheck> {
        let sha = hash_secret(presented);
        let mut tx = self.pool.begin().await?;
        let current = sqlx::query(concat!(
            "select ",
            session_columns!(),
            " from sessions where refresh_sha256 = $1 for update"
        ))
        .bind(&sha[..])
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = current {
            let session = from_row(&row)?;
            let ended_at: Option<OffsetDateTime> = row.try_get("ended_at")?;
            if session.client_id != client_id {
                return Ok(RefreshCheck::OtherClient);
            }
            if ended_at.is_some() {
                return Ok(RefreshCheck::Ended);
            }
            if session.idle_expires_at <= now {
                end_session_in(&mut tx, &session.id, "idle", now).await?;
                tx.commit().await?;
                return Ok(RefreshCheck::Idle);
            }
            return Ok(RefreshCheck::Live(RefreshLock { tx, session }));
        }

        let previous = sqlx::query(concat!(
            "select ",
            session_columns!(),
            " from sessions where previous_refresh_sha256 = $1 limit 1 for update"
        ))
        .bind(&sha[..])
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = previous else {
            return Ok(RefreshCheck::Unknown);
        };
        let session = from_row(&row)?;
        let ended_at: Option<OffsetDateTime> = row.try_get("ended_at")?;
        let rotated_at: Option<OffsetDateTime> = row.try_get("rotated_at")?;
        if session.client_id != client_id {
            return Ok(RefreshCheck::OtherClient);
        }
        if ended_at.is_some() {
            return Ok(RefreshCheck::Ended);
        }
        if rotated_at.is_some_and(|at| now < at + REUSE_GRACE) {
            return Ok(RefreshCheck::ReusedInGrace);
        }
        end_session_in(&mut tx, &session.id, "reused", now).await?;
        tx.commit().await?;
        Ok(RefreshCheck::Reused)
    }

    /// RFC 7009: ends the session whose current refresh token this is, if it
    /// was issued to `client_id`. `false` for anything else.
    pub async fn end_session_by_refresh(
        &self,
        presented: &str,
        client_id: &str,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let session: Option<Uuid> = sqlx::query_scalar(
            "select id from sessions where refresh_sha256 = $1 and client_id = $2 for update",
        )
        .bind(&hash_secret(presented)[..])
        .bind(client_id)
        .fetch_optional(&mut *tx)
        .await?;
        let ended = match session {
            Some(id) => end_session_in(&mut tx, &id, "revoked", now).await?,
            None => false,
        };
        tx.commit().await?;
        Ok(ended)
    }

    /// Ends one session; `false` if it had already ended or doesn't exist.
    pub async fn end_session(
        &self,
        session: &Uuid,
        reason: &str,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let ended = end_session_in(&mut tx, session, reason, now).await?;
        tx.commit().await?;
        Ok(ended)
    }

    /// Ends every open session of the users whose latest sign-in came
    /// through this connector.
    pub async fn end_connector_sessions(
        &self,
        connector_id: &str,
        reason: &str,
        now: OffsetDateTime,
    ) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let ended = sqlx::query(
            "update sessions s set ended_at = $3, end_reason = $2 from users u \
             where u.id = s.user_id and u.dex_connector = $1 and s.ended_at is null",
        )
        .bind(connector_id)
        .bind(reason)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if ended > 0 {
            notify(&mut tx, ChangeKind::Sessions).await?;
        }
        tx.commit().await?;
        Ok(ended)
    }
}
