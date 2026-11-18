//! Catalog entries: reading them and their files, resolving references,
//! owners and quarantine.

use sekisho_catalog::model::EntrySpec;
use sekisho_catalog::names::{EntryKind, EntryRef};
use sqlx::Row;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    Entry, EntryKey, FileMeta, Quarantine, QuarantineRecord, ResolveError, Revision, entry_kind,
    entry_name, source_name,
};
use crate::error::{FieldError, bad_column, check};
use crate::notify::{ChangeKind, notify};
use crate::{Result, Store, StoreError};

const MAX_REASON: usize = 500;

macro_rules! entry_select {
    () => {
        "select e.id, e.kind, e.name, e.path, e.owner_id, e.removed_at, e.quarantined_at, \
         e.quarantine_reason, e.quarantined_by, e.applied_revision_id, e.latest_revision_id, \
         s.name as source \
         from catalog_entries e join catalog_sources s on s.id = e.source_id"
    };
}

pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() != 64 || !text.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

impl Store {
    async fn revision(&self, id: Uuid, with_files: bool) -> Result<Revision> {
        let row = sqlx::query(
            "select id, commit, digest, spec, applied_at from catalog_revisions where id = $1",
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        let spec: serde_json::Value = row.try_get("spec")?;
        let spec: EntrySpec =
            serde_json::from_value(spec).map_err(|e| bad_column("spec", &e.to_string()))?;
        let files = if with_files {
            sqlx::query(
                "select path, sha256, size, executable from catalog_revision_files \
                 where revision_id = $1 order by path collate \"C\"",
            )
            .bind(id)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|f| {
                Ok(FileMeta {
                    path: f.try_get("path")?,
                    sha256: hex(&f.try_get::<Vec<u8>, _>("sha256")?),
                    size: f.try_get::<i64, _>("size")? as u64,
                    executable: f.try_get("executable")?,
                })
            })
            .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        Ok(Revision {
            id: row.try_get("id")?,
            commit: row.try_get("commit")?,
            digest: row.try_get("digest")?,
            spec,
            applied_at: row.try_get("applied_at")?,
            files,
        })
    }

    async fn entry_from_row(&self, row: &PgRow, with_files: bool) -> Result<Entry> {
        let applied_id: Option<Uuid> = row.try_get("applied_revision_id")?;
        let latest_id: Option<Uuid> = row.try_get("latest_revision_id")?;
        let applied = match applied_id {
            Some(id) => Some(self.revision(id, with_files).await?),
            None => None,
        };
        let pending = match latest_id {
            Some(id) if Some(id) != applied_id => Some(self.revision(id, with_files).await?),
            _ => None,
        };
        let quarantine = match row.try_get::<Option<OffsetDateTime>, _>("quarantined_at")? {
            Some(at) => Some(Quarantine {
                reason: row
                    .try_get::<Option<String>, _>("quarantine_reason")?
                    .unwrap_or_default(),
                by: row
                    .try_get::<Option<Uuid>, _>("quarantined_by")?
                    .ok_or_else(|| bad_column("quarantined_by", "NULL"))?,
                at,
            }),
            None => None,
        };
        Ok(Entry {
            id: row.try_get("id")?,
            key: EntryKey {
                kind: entry_kind(&row.try_get::<String, _>("kind")?)?,
                source: source_name(&row.try_get::<String, _>("source")?)?,
                name: entry_name(&row.try_get::<String, _>("name")?)?,
            },
            owner: row.try_get("owner_id")?,
            path: row.try_get("path")?,
            applied,
            pending,
            removed_at: row.try_get("removed_at")?,
            quarantine,
            toolkits: self.toolkits_listing(row.try_get("id")?).await?,
            environments: self.environments_listing(row.try_get("id")?).await?,
        })
    }

    /// The environments that list entry `id` themselves, by name.
    async fn environments_listing(&self, id: Uuid) -> Result<Vec<sekisho_core::names::EnvName>> {
        let names: Vec<String> = sqlx::query_scalar(
            "select e.name from environment_entries ee              join environments e on e.id = ee.environment_id              where ee.entry_id = $1 order by e.name",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        names
            .iter()
            .map(|n| {
                sekisho_core::names::EnvName::parse(n)
                    .map_err(|_| bad_column("environment name", n))
            })
            .collect()
    }

    /// Every entry, or every entry of one kind, by source and name; their
    /// revisions' files are left out.
    pub async fn list_entries(&self, kind: Option<EntryKind>) -> Result<Vec<Entry>> {
        let rows = sqlx::query(concat!(
            entry_select!(),
            " where $1::text is null or e.kind = $1 order by s.name, e.kind, e.name"
        ))
        .bind(kind.map(EntryKind::as_str))
        .fetch_all(&self.pool)
        .await?;
        let mut entries = Vec::with_capacity(rows.len());
        for row in &rows {
            entries.push(self.entry_from_row(row, false).await?);
        }
        Ok(entries)
    }

    /// One entry with its revisions' files.
    pub async fn get_entry(&self, key: &EntryKey) -> Result<Entry> {
        let row = sqlx::query(concat!(
            entry_select!(),
            " where e.kind = $1 and s.name = $2 and e.name = $3"
        ))
        .bind(key.kind.as_str())
        .bind(key.source.as_str())
        .bind(key.name.as_str())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound)?;
        self.entry_from_row(&row, true).await
    }

    async fn entry_id(&self, key: &EntryKey) -> Result<Uuid> {
        sqlx::query_scalar(
            "select e.id from catalog_entries e join catalog_sources s on s.id = e.source_id \
             where e.kind = $1 and s.name = $2 and e.name = $3",
        )
        .bind(key.kind.as_str())
        .bind(key.source.as_str())
        .bind(key.name.as_str())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound)
    }

    /// The entry a reference names (D167): `name@source` exactly; a bare
    /// name only when one source of that kind has it.
    pub async fn resolve_entry(
        &self,
        kind: EntryKind,
        reference: &EntryRef,
    ) -> std::result::Result<Uuid, ResolveError> {
        let mut conn = self.pool.acquire().await?;
        super::toolkits::resolve_in(&mut conn, kind, reference).await
    }

    /// A file of the entry's applied revision.
    pub async fn entry_file(&self, key: &EntryKey, path: &str) -> Result<Vec<u8>> {
        let id = self.entry_id(key).await?;
        sqlx::query_scalar(
            "select b.bytes from catalog_entries e \
             join catalog_revision_files f on f.revision_id = e.applied_revision_id \
             join catalog_blobs b on b.sha256 = f.sha256 \
             where e.id = $1 and f.path = $2",
        )
        .bind(id)
        .bind(path)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound)
    }

    /// A stored file by its SHA-256 in hex.
    pub async fn blob(&self, sha256: &str) -> Result<Vec<u8>> {
        let sha = unhex(sha256).ok_or(StoreError::NotFound)?;
        sqlx::query_scalar("select bytes from catalog_blobs where sha256 = $1")
            .bind(sha)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)
    }

    /// Hands an entry to another owner, who must be an enabled user.
    pub async fn set_entry_owner(&self, key: &EntryKey, owner: Uuid) -> Result<()> {
        let id = self.entry_id(key).await?;
        let mut tx = self.pool.begin().await?;
        // Users before entries, as offboarding locks them.
        let disabled: Option<bool> =
            sqlx::query_scalar("select disabled_at is not null from users where id = $1 for share")
                .bind(owner)
                .fetch_optional(&mut *tx)
                .await?;
        match disabled {
            None => check(vec![FieldError::new("owner", format!("no user {owner}"))])?,
            Some(true) => check(vec![FieldError::new(
                "owner",
                format!("{owner} is disabled"),
            )])?,
            Some(false) => {}
        }
        let updated = sqlx::query("update catalog_entries set owner_id = $2 where id = $1")
            .bind(id)
            .bind(owner)
            .execute(&mut *tx)
            .await?;
        if updated.rows_affected() == 0 {
            return Err(StoreError::NotFound);
        }
        notify(&mut tx, ChangeKind::Catalog).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Quarantines an entry in every environment at once, keeping the
    /// period in its history. Conflict if it is quarantined already.
    pub async fn quarantine(
        &self,
        key: &EntryKey,
        reason: &str,
        actor: Uuid,
        now: OffsetDateTime,
    ) -> Result<()> {
        let reason = reason.trim();
        if reason.is_empty()
            || reason.chars().count() > MAX_REASON
            || reason.chars().any(|c| c.is_control() && c != '\n')
        {
            check(vec![FieldError::new(
                "reason",
                "give a reason of 1-500 characters",
            )])?;
        }
        let id = self.entry_id(key).await?;
        let mut tx = self.pool.begin().await?;
        let quarantined: Option<OffsetDateTime> = sqlx::query_scalar(
            "select quarantined_at from catalog_entries where id = $1 for update",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::NotFound)?;
        if quarantined.is_some() {
            return Err(StoreError::Conflict(format!(
                "{}/{}/{} is quarantined already",
                key.kind.path_segment(),
                key.source,
                key.name
            )));
        }
        sqlx::query(
            "update catalog_entries set quarantined_at = $2, quarantine_reason = $3, \
             quarantined_by = $4 where id = $1",
        )
        .bind(id)
        .bind(now)
        .bind(reason)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "insert into catalog_quarantines (entry_id, reason, quarantined_by, quarantined_at) \
             values ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(reason)
        .bind(actor)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        notify(&mut tx, ChangeKind::Catalog).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Lifts an entry's quarantine. Conflict if it isn't quarantined.
    pub async fn lift_quarantine(
        &self,
        key: &EntryKey,
        actor: Uuid,
        now: OffsetDateTime,
    ) -> Result<()> {
        let id = self.entry_id(key).await?;
        let mut tx = self.pool.begin().await?;
        let quarantined: Option<OffsetDateTime> = sqlx::query_scalar(
            "select quarantined_at from catalog_entries where id = $1 for update",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::NotFound)?;
        if quarantined.is_none() {
            return Err(StoreError::Conflict(format!(
                "{}/{}/{} isn't quarantined",
                key.kind.path_segment(),
                key.source,
                key.name
            )));
        }
        sqlx::query(
            "update catalog_entries set quarantined_at = null, quarantine_reason = null, \
             quarantined_by = null where id = $1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "update catalog_quarantines set lifted_by = $2, lifted_at = $3 \
             where entry_id = $1 and lifted_at is null",
        )
        .bind(id)
        .bind(actor)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        notify(&mut tx, ChangeKind::Catalog).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Every period the entry spent in quarantine, oldest first.
    pub async fn quarantine_history(&self, key: &EntryKey) -> Result<Vec<QuarantineRecord>> {
        let id = self.entry_id(key).await?;
        sqlx::query(
            "select reason, quarantined_by, quarantined_at, lifted_by, lifted_at \
             from catalog_quarantines where entry_id = $1 order by quarantined_at, id",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| {
            Ok(QuarantineRecord {
                reason: r.try_get("reason")?,
                quarantined_by: r.try_get("quarantined_by")?,
                quarantined_at: r.try_get("quarantined_at")?,
                lifted_by: r.try_get("lifted_by")?,
                lifted_at: r.try_get("lifted_at")?,
            })
        })
        .collect()
    }
}
