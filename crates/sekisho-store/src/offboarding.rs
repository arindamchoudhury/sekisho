//! Offboarding: one transaction that disables a person, ends their
//! sessions, revokes their keys and hands what they own to a new owner.

use sekisho_core::keys::EnvKeyId;
use sekisho_core::names::EnvName;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{FieldError, bad_column};
use crate::notify::{ChangeKind, notify};
use crate::sessions::end_user_sessions;
use crate::{Result, Store, StoreError};

/// What offboarding did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offboarded {
    /// For revoking the person's refresh token at Dex, after the commit.
    pub dex_subject: Option<String>,
    pub ended_sessions: u64,
    pub revoked_keys: Vec<EnvKeyId>,
    /// The environments handed to the new owner, by name.
    pub handed_over: Vec<EnvName>,
}

fn refuse(message: impl Into<String>) -> StoreError {
    StoreError::Validation(vec![FieldError::new("new_owner", message)])
}

impl Store {
    /// Offboards `user`: disables them, ends their sessions (`offboarded`),
    /// revokes every key of theirs not yet revoked, and hands their
    /// environments to `new_owner`. All or nothing.
    ///
    /// Refused (`Validation` on `new_owner`) when they own environments and no
    /// new owner is named, or the new owner is the person, unknown or
    /// disabled.
    pub async fn offboard(
        &self,
        user: &Uuid,
        new_owner: Option<Uuid>,
        now: OffsetDateTime,
    ) -> Result<Offboarded> {
        let mut tx = self.pool.begin().await?;
        // Users are locked before environments and keys everywhere, in ID
        // order, so concurrent saves and offboardings can't deadlock.
        let mut ids = vec![*user];
        ids.extend(new_owner.filter(|n| n != user));
        let people: Vec<(Uuid, Option<String>, bool)> = sqlx::query_as(
            "select id, dex_subject, disabled_at is not null from users \
             where id = any($1) order by id for update",
        )
        .bind(&ids)
        .fetch_all(&mut *tx)
        .await?;
        let dex_subject = people
            .iter()
            .find(|p| p.0 == *user)
            .ok_or(StoreError::NotFound)?
            .1
            .clone();
        let owned: Vec<String> =
            sqlx::query_scalar("select name from environments where owner_id = $1 order by name")
                .bind(user)
                .fetch_all(&mut *tx)
                .await?;
        match new_owner {
            None if !owned.is_empty() => {
                return Err(refuse(format!(
                    "they own {}; name a new owner for them",
                    owned.join(", ")
                )));
            }
            None => {}
            Some(n) if n == *user => {
                return Err(refuse("must be someone other than the person offboarded"));
            }
            Some(n) => match people.iter().find(|p| p.0 == n) {
                None => return Err(refuse(format!("no user {n}"))),
                Some((_, _, true)) => return Err(refuse(format!("{n} is disabled"))),
                Some(_) => {}
            },
        }

        sqlx::query("update users set disabled_at = coalesce(disabled_at, $2) where id = $1")
            .bind(user)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        let ended_sessions = end_user_sessions(&mut tx, user, "offboarded", now).await?;
        let revoked: Vec<String> = sqlx::query_scalar(
            "update env_keys set revoked_at = $2 where owner_id = $1 and revoked_at is null \
             returning id",
        )
        .bind(user)
        .bind(now)
        .fetch_all(&mut *tx)
        .await?;
        let mut handed: Vec<String> = match new_owner {
            Some(n) => {
                sqlx::query_scalar(
                    "update environments set owner_id = $2, updated_at = now() \
                     where owner_id = $1 returning name",
                )
                .bind(user)
                .bind(n)
                .fetch_all(&mut *tx)
                .await?
            }
            None => Vec::new(),
        };
        handed.sort();
        notify(&mut tx, ChangeKind::Users).await?;
        if !revoked.is_empty() {
            notify(&mut tx, ChangeKind::Keys).await?;
        }
        if !handed.is_empty() {
            notify(&mut tx, ChangeKind::Environments).await?;
        }
        tx.commit().await?;
        Ok(Offboarded {
            dex_subject,
            ended_sessions,
            revoked_keys: revoked
                .iter()
                .map(|id| EnvKeyId::parse(id).ok_or_else(|| bad_column("key id", id)))
                .collect::<Result<_>>()?,
            handed_over: handed
                .iter()
                .map(|n| EnvName::parse(n).map_err(|_| bad_column("environment name", n)))
                .collect::<Result<_>>()?,
        })
    }
}
