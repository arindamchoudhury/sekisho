//! Offboarding: one transaction that disables a person, ends their
//! sessions, revokes their keys and hands what they own (environments and
//! catalog entries) to a new owner.

use sekisho_core::keys::EnvKeyId;
use sekisho_core::names::EnvName;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::catalog::{EntryKey, entry_kind, entry_name, source_name};
use crate::error::{FieldError, bad_column};
use crate::notify::{ChangeKind, notify};
use crate::sessions::end_user_sessions;
use crate::{Result, Store, StoreError};
use crate::{dex_tokens, setup_links};

/// What offboarding did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offboarded {
    /// For revoking the person's refresh token at Dex, after the commit.
    pub dex_subject: Option<String>,
    pub ended_sessions: u64,
    pub revoked_keys: Vec<EnvKeyId>,
    /// The environments handed to the new owner, by name.
    pub handed_over: Vec<EnvName>,
    /// The catalog entries handed to the new owner, by source, kind and
    /// name.
    pub handed_over_entries: Vec<EntryKey>,
}

fn refuse(message: impl Into<String>) -> StoreError {
    StoreError::Validation(vec![FieldError::new("new_owner", message)])
}

impl Store {
    /// Offboards `user`: disables them, ends their sessions (`offboarded`),
    /// revokes every key of theirs not yet revoked, and hands their
    /// environments and catalog entries to `new_owner`. All or nothing.
    ///
    /// Refused (`Validation` on `new_owner`) when they own environments or
    /// entries and no new owner is named, or the new owner is the person,
    /// unknown or disabled.
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
        // Users, then sources, then entries, as the catalog locks them.
        let owned_entries: Vec<(String, String, String)> = sqlx::query_as(
            "select e.kind, s.name, e.name from catalog_entries e              join catalog_sources s on s.id = e.source_id              where e.owner_id = $1 order by s.name, e.kind, e.name for update of e",
        )
        .bind(user)
        .fetch_all(&mut *tx)
        .await?;
        let handed_entries = owned_entries
            .iter()
            .map(|(kind, source, name)| {
                Ok(EntryKey {
                    kind: entry_kind(kind)?,
                    source: source_name(source)?,
                    name: entry_name(name)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        match new_owner {
            None if !owned.is_empty() || !handed_entries.is_empty() => {
                let mut what = Vec::new();
                if !owned.is_empty() {
                    what.push(format!("environments {}", owned.join(", ")));
                }
                if !handed_entries.is_empty() {
                    let entries: Vec<String> = handed_entries
                        .iter()
                        .map(|k| format!("{}/{}/{}", k.kind.path_segment(), k.source, k.name))
                        .collect();
                    what.push(format!("catalog entries {}", entries.join(", ")));
                }
                return Err(refuse(format!(
                    "they own {}; name a new owner for them",
                    what.join(" and ")
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
        // After the sessions, as everywhere: their Dex token goes too.
        dex_tokens::delete_in(&mut tx, user).await?;
        setup_links::delete_unused_in(&mut tx, user).await?;
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
        if let Some(n) = new_owner
            && !handed_entries.is_empty()
        {
            sqlx::query("update catalog_entries set owner_id = $2 where owner_id = $1")
                .bind(user)
                .bind(n)
                .execute(&mut *tx)
                .await?;
            notify(&mut tx, ChangeKind::Catalog).await?;
        }
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
            handed_over_entries: if new_owner.is_some() {
                handed_entries
            } else {
                Vec::new()
            },
        })
    }
}
