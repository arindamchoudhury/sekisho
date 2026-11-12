//! Sessions. This module ends them; signing in and refreshing make and
//! rotate them.

use sqlx::PgConnection;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::notify::{ChangeKind, notify};
use crate::{Result, Store};

/// How long an ended session stays in the auth cache: the 1-hour access
/// token's life plus a margin, so a token of an ended session is refused
/// until it expires anyway.
pub const ENDED_SESSIONS_KEPT: Duration = Duration::minutes(65);

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
    /// Ends one session; `false` if it had already ended or doesn't exist.
    pub async fn end_session(
        &self,
        session: &Uuid,
        reason: &str,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let ended = sqlx::query(
            "update sessions set ended_at = $3, end_reason = $2 \
             where id = $1 and ended_at is null",
        )
        .bind(session)
        .bind(reason)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if ended > 0 {
            notify(&mut tx, ChangeKind::Sessions).await?;
        }
        tx.commit().await?;
        Ok(ended > 0)
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
