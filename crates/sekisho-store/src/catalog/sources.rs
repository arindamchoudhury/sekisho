//! Catalog sources: adding, changing, reading, deleting and claiming them.

use sekisho_catalog::names::{SourceKind, SourceName};
use sekisho_catalog::source::{Folder, GitRef, SourceUrl};
use sekisho_core::secret_box::SecretBox;
use sqlx::Row;
use sqlx::postgres::PgRow;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::{
    NewSource, Source, SourcePatch, SyncOutcomeKind, SyncSummary, source_kind, source_name,
};
use crate::error::{FieldError, bad_column, check, is_unique_violation};
use crate::notify::{ChangeKind, notify};
use crate::{Result, Store, StoreError};

/// Every source's columns, its latest sync, whether its latest successful
/// sync waits for apply, and whether a replica holds it.
macro_rules! source_select {
    () => {
        "select s.id, s.name, s.kind, s.url, s.git_ref, s.folder, s.username, s.updated_at, \
         s.sealed_token is not null as has_token, s.allow_private_network, s.added_by, \
         s.applied_commit, coalesce(s.claimed_until > now(), false) as syncing, \
         l.id as sync_id, l.started_at, l.finished_at, l.commit as sync_commit, \
         l.outcome, l.error, l.applied_at as sync_applied_at, \
         coalesce((select y.applied_at is null from catalog_syncs y \
                   where y.source_id = s.id and y.outcome = 'ok' \
                   order by y.seq desc limit 1), false) as pending \
         from catalog_sources s \
         left join lateral (select * from catalog_syncs y where y.source_id = s.id \
                            order by y.seq desc limit 1) l on true"
    };
}

fn from_row(row: &PgRow) -> Result<Source> {
    let url: String = row.try_get("url")?;
    let git_ref: Option<String> = row.try_get("git_ref")?;
    let folder: String = row.try_get("folder")?;
    let latest_sync = match row.try_get::<Option<Uuid>, _>("sync_id")? {
        None => None,
        Some(id) => Some(SyncSummary {
            id,
            started_at: row.try_get("started_at")?,
            finished_at: row.try_get("finished_at")?,
            commit: row.try_get("sync_commit")?,
            outcome: SyncOutcomeKind::parse(&row.try_get::<String, _>("outcome")?)?,
            error: row.try_get("error")?,
            applied_at: row.try_get("sync_applied_at")?,
        }),
    };
    Ok(Source {
        id: row.try_get("id")?,
        name: source_name(&row.try_get::<String, _>("name")?)?,
        kind: source_kind(&row.try_get::<String, _>("kind")?)?,
        // Stored URLs were checked when saved; plain http only where the
        // install allowed it, so it is read back as allowed.
        url: SourceUrl::parse(&url, true).map_err(|_| bad_column("url", &url))?,
        git_ref: git_ref
            .map(|r| GitRef::parse(&r).map_err(|_| bad_column("git_ref", &r)))
            .transpose()?,
        folder: Folder::parse(&folder).map_err(|_| bad_column("folder", &folder))?,
        username: row.try_get("username")?,
        has_token: row.try_get("has_token")?,
        allow_private_network: row.try_get("allow_private_network")?,
        added_by: row.try_get("added_by")?,
        applied_commit: row.try_get("applied_commit")?,
        latest_sync,
        pending: row.try_get("pending")?,
        syncing: row.try_get("syncing")?,
        updated_at: row.try_get("updated_at")?,
    })
}

const MAX_USERNAME: usize = 255;
const MAX_TOKEN: usize = 4096;

/// A username and token travel in an `Authorization: Basic` header that
/// `git` sets from its environment, so neither may hold a control
/// character or whitespace, and the username no `:`.
fn check_credentials(errors: &mut Vec<FieldError>, username: Option<&str>, token: Option<&str>) {
    let bad_username = |u: &str| {
        u.is_empty()
            || u.chars().count() > MAX_USERNAME
            || u.chars()
                .any(|c| c.is_control() || c.is_whitespace() || c == ':')
    };
    let bad_token = |t: &str| {
        t.is_empty()
            || t.len() > MAX_TOKEN
            || t.chars().any(|c| c.is_control() || c.is_whitespace())
    };
    if username.is_some_and(bad_username) {
        errors.push(FieldError::new(
            "username",
            "use 1-255 characters without spaces, control characters or :",
        ));
    }
    if token.is_some_and(bad_token) {
        errors.push(FieldError::new(
            "token",
            "use 1-4096 characters without spaces or control characters",
        ));
    }
}

/// Git mode's source alone has the `environments` kind and name.
fn check_kind(errors: &mut Vec<FieldError>, name: &SourceName, kind: SourceKind) {
    let git_mode = name == &SourceName::git_mode();
    if (kind == SourceKind::Environments) != git_mode {
        errors.push(FieldError::new(
            "kind",
            "only Git mode has an environments source",
        ));
    }
}

impl Store {
    /// Adds a source, sealing its token. Its first sync is due at once.
    pub async fn add_source(
        &self,
        secret_box: &SecretBox,
        new: NewSource,
        actor: Uuid,
    ) -> Result<Source> {
        let mut errors = Vec::new();
        check_kind(&mut errors, &new.name, new.kind);
        check_credentials(&mut errors, new.username.as_deref(), new.token.as_deref());
        check(errors)?;
        let sealed = new.token.as_ref().map(|t| secret_box.seal(t.as_bytes()));
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "insert into catalog_sources (name, kind, url, git_ref, folder, username, \
             sealed_token, allow_private_network, added_by) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(new.name.as_str())
        .bind(new.kind.as_str())
        .bind(new.url.as_str())
        .bind(new.git_ref.as_ref().map(GitRef::as_str))
        .bind(new.folder.as_str())
        .bind(&new.username)
        .bind(sealed)
        .bind(new.allow_private_network)
        .bind(actor)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                StoreError::Conflict(format!("a source named {} already exists", new.name))
            } else {
                e.into()
            }
        })?;
        notify(&mut tx, ChangeKind::Catalog).await?;
        tx.commit().await?;
        self.get_source(&new.name).await
    }

    /// Changes a source's settings. A new URL needs the token again while
    /// one is stored (D69), so no one redirects a token they don't have. A
    /// change to what is read makes the next sync due at once.
    pub async fn update_source(
        &self,
        secret_box: &SecretBox,
        name: &SourceName,
        patch: SourcePatch,
    ) -> Result<Source> {
        let current = self.get_source(name).await?;
        let mut errors = Vec::new();
        check_credentials(
            &mut errors,
            patch.username.as_ref().and_then(Option::as_deref),
            patch.token.as_ref().and_then(Option::as_deref),
        );
        let moves = patch.url.as_ref().is_some_and(|u| u != &current.url);
        // Giving the token again, or clearing it, lets the URL move.
        if moves && current.has_token && patch.token.is_none() {
            errors.push(FieldError::new(
                "token",
                "changing the URL needs the token again",
            ));
        }
        check(errors)?;
        let rereads = moves
            || patch
                .git_ref
                .as_ref()
                .is_some_and(|r| r != &current.git_ref)
            || patch.folder.as_ref().is_some_and(|f| f != &current.folder);

        let mut tx = self.pool.begin().await?;
        sqlx::query("select id from catalog_sources where id = $1 for update")
            .bind(current.id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        let url = patch.url.unwrap_or(current.url);
        let git_ref = patch.git_ref.unwrap_or(current.git_ref);
        let folder = patch.folder.unwrap_or(current.folder);
        let username = patch.username.unwrap_or(current.username);
        let allow_private_network = patch
            .allow_private_network
            .unwrap_or(current.allow_private_network);
        // Some(new) seals it; Some(None) clears it; None keeps it.
        let (set_token, sealed) = match &patch.token {
            Some(Some(t)) => (true, Some(secret_box.seal(t.as_bytes()))),
            Some(None) => (true, None),
            None => (false, None),
        };
        sqlx::query(
            "update catalog_sources set url = $2, git_ref = $3, folder = $4, username = $5, \
             allow_private_network = $6, \
             sealed_token = case when $7 then $8 else sealed_token end, \
             next_sync_at = case when $9 then now() else next_sync_at end, \
             updated_at = now() \
             where id = $1",
        )
        .bind(current.id)
        .bind(url.as_str())
        .bind(git_ref.as_ref().map(GitRef::as_str))
        .bind(folder.as_str())
        .bind(&username)
        .bind(allow_private_network)
        .bind(set_token)
        .bind(sealed)
        .bind(rereads)
        .execute(&mut *tx)
        .await?;
        notify(&mut tx, ChangeKind::Catalog).await?;
        tx.commit().await?;
        self.get_source(name).await
    }

    /// Deletes a source with its syncs, entries and their revisions.
    pub async fn delete_source(&self, name: &SourceName) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let id: Uuid =
            sqlx::query_scalar("select id from catalog_sources where name = $1 for update")
                .bind(name.as_str())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(StoreError::NotFound)?;
        // Revisions reference their entry and entries reference revisions:
        // the links go first.
        sqlx::query(
            "update catalog_entries set applied_revision_id = null, latest_revision_id = null \
             where source_id = $1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("delete from catalog_entries where source_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("delete from catalog_sources where id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        notify(&mut tx, ChangeKind::Catalog).await?;
        notify(&mut tx, ChangeKind::Environments).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn get_source(&self, name: &SourceName) -> Result<Source> {
        let row = sqlx::query(concat!(source_select!(), " where s.name = $1"))
            .bind(name.as_str())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        from_row(&row)
    }

    /// Every source, by name.
    pub async fn list_sources(&self) -> Result<Vec<Source>> {
        sqlx::query(concat!(source_select!(), " order by s.name"))
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(from_row)
            .collect()
    }

    /// The source's token, for the sync alone.
    pub async fn source_token(
        &self,
        secret_box: &SecretBox,
        name: &SourceName,
    ) -> Result<Option<String>> {
        let sealed: Option<Vec<u8>> =
            sqlx::query_scalar("select sealed_token from catalog_sources where name = $1")
                .bind(name.as_str())
                .fetch_optional(&self.pool)
                .await?
                .ok_or(StoreError::NotFound)?;
        open_token(secret_box, sealed)
    }

    /// Claims the source whose sync is most overdue, if any is due and no
    /// replica holds it, for `lease`.
    pub async fn claim_due_source(
        &self,
        now: OffsetDateTime,
        lease: Duration,
    ) -> Result<Option<SourceName>> {
        let name: Option<String> = sqlx::query_scalar(
            "update catalog_sources set claimed_until = $1 + $2 \
             where id = (select id from catalog_sources \
                         where next_sync_at <= $1 \
                           and (claimed_until is null or claimed_until < $1) \
                         order by next_sync_at limit 1 for update skip locked) \
             returning name",
        )
        .bind(now)
        .bind(lease)
        .fetch_optional(&self.pool)
        .await?;
        name.map(|n| source_name(&n)).transpose()
    }

    /// Claims `name` for `lease`, as an admin's sync does; `false` when a
    /// replica holds it.
    pub async fn claim_source(
        &self,
        name: &SourceName,
        now: OffsetDateTime,
        lease: Duration,
    ) -> Result<bool> {
        let claimed = sqlx::query(
            "update catalog_sources set claimed_until = $2 + $3 \
             where name = $1 and (claimed_until is null or claimed_until < $2) \
             returning id",
        )
        .bind(name.as_str())
        .bind(now)
        .bind(lease)
        .fetch_optional(&self.pool)
        .await?;
        if claimed.is_none() && !self.source_exists(name).await? {
            return Err(StoreError::NotFound);
        }
        Ok(claimed.is_some())
    }

    async fn source_exists(&self, name: &SourceName) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, bool>(
            "select exists (select 1 from catalog_sources where name = $1)",
        )
        .bind(name.as_str())
        .fetch_one(&self.pool)
        .await?)
    }
}

pub(super) fn open_token(
    secret_box: &SecretBox,
    sealed: Option<Vec<u8>>,
) -> Result<Option<String>> {
    sealed
        .map(|s| {
            let bytes = secret_box.open(&s)?;
            String::from_utf8(bytes).map_err(|_| bad_column("sealed_token", "not UTF-8"))
        })
        .transpose()
}
