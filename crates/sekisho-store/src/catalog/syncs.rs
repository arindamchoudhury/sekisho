//! Recording a sync and applying one.

use std::collections::HashSet;

use base64::Engine;
use sekisho_catalog::model::{ObotVmcp, Refusal, Warning};
use sekisho_catalog::names::{EntryKind, EntryName, SourceName};
use sekisho_core::secret_box::SecretBox;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use super::sources::open_token;
use super::{
    Applied, SyncOutcome, SyncOutcomeKind, SyncRecord, SyncReport, entry_kind, entry_name,
};
use crate::notify::{ChangeKind, notify};
use crate::{Result, Store, StoreError};

/// A stored error is cut to this many bytes.
const MAX_ERROR: usize = 2048;
const REDACTED: &str = "[redacted]";

/// The report kept with a sync.
#[derive(Default, Serialize, Deserialize)]
struct StoredReport {
    #[serde(default)]
    new: Vec<(EntryKind, EntryName)>,
    #[serde(default)]
    updated: Vec<(EntryKind, EntryName)>,
    #[serde(default)]
    unchanged: Vec<(EntryKind, EntryName)>,
    #[serde(default)]
    missing: Vec<(EntryKind, EntryName)>,
    #[serde(default)]
    refused: Vec<Refusal>,
    #[serde(default)]
    warnings: Vec<Warning>,
    #[serde(default)]
    vmcps: Vec<ObotVmcp>,
    #[serde(default)]
    environments: serde_json::Value,
}

/// Removes every form of a source's token from text: the token itself and
/// the `Authorization: Basic` value made from it (D165).
struct Redactor(Vec<String>);

impl Redactor {
    fn new(username: Option<&str>, token: Option<&str>) -> Self {
        let mut secrets = Vec::new();
        if let Some(token) = token.filter(|t| !t.is_empty()) {
            let user = username.unwrap_or("x-access-token");
            let engine = base64::engine::general_purpose::STANDARD;
            secrets.push(engine.encode(format!("{user}:{token}")));
            secrets.push(token.to_owned());
        }
        Self(secrets)
    }

    fn clean(&self, text: &str) -> String {
        self.0.iter().fold(text.to_owned(), |t, secret| {
            t.replace(secret.as_str(), REDACTED)
        })
    }

    /// Cleans every string in a JSON document.
    fn clean_json(&self, value: serde_json::Value) -> serde_json::Value {
        use serde_json::Value;
        match value {
            Value::String(s) => Value::String(self.clean(&s)),
            Value::Array(items) => {
                Value::Array(items.into_iter().map(|v| self.clean_json(v)).collect())
            }
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(k, v)| (self.clean(&k), self.clean_json(v)))
                    .collect(),
            ),
            other => other,
        }
    }
}

/// Cuts text to `max` bytes at a character boundary.
fn cut(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

impl Store {
    /// Records a sync of `name` and makes its next one due at
    /// `next_sync_at`, releasing the claim. A successful sync's entries,
    /// revisions and files are stored; a source's first is applied at once.
    /// The source's token never reaches the stored error or report.
    pub async fn record_sync(
        &self,
        secret_box: &SecretBox,
        name: &SourceName,
        record: SyncRecord,
        next_sync_at: OffsetDateTime,
    ) -> Result<SyncReport> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "select id, username, sealed_token, applied_commit from catalog_sources \
             where name = $1 for update",
        )
        .bind(name.as_str())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::NotFound)?;
        let source_id: Uuid = row.try_get("id")?;
        let username: Option<String> = row.try_get("username")?;
        let token = open_token(secret_box, row.try_get("sealed_token")?)?;
        let first = row
            .try_get::<Option<String>, _>("applied_commit")?
            .is_none();
        let redact = Redactor::new(username.as_deref(), token.as_deref());

        let (outcome, error, found) = match record.outcome {
            SyncOutcome::Ok(found) => (SyncOutcomeKind::Ok, None, Some(found)),
            SyncOutcome::Unchanged => (SyncOutcomeKind::Unchanged, None, None),
            SyncOutcome::Failed(message) => (
                SyncOutcomeKind::Failed,
                Some(cut(&redact.clean(&message), MAX_ERROR)),
                None,
            ),
        };
        let commit = record.commit.map(|c| redact.clean(&c));
        let sync_id: Uuid = sqlx::query_scalar(
            "insert into catalog_syncs (source_id, started_at, finished_at, commit, outcome, error) \
             values ($1, $2, $3, $4, $5, $6) returning id",
        )
        .bind(source_id)
        .bind(record.started_at)
        .bind(record.finished_at)
        .bind(&commit)
        .bind(outcome.as_str())
        .bind(&error)
        .fetch_one(&mut *tx)
        .await?;

        let mut report = StoredReport::default();
        let mut applied = false;
        if let Some(found) = found {
            let commit = commit.clone().unwrap_or_default();
            let mut seen = HashSet::new();
            for entry in &found.entries {
                seen.insert((entry.kind, entry.name.to_string()));
                let change = store_entry(&mut tx, source_id, sync_id, &commit, entry).await?;
                let into = match change {
                    Change::New => &mut report.new,
                    Change::Updated => &mut report.updated,
                    Change::Unchanged => &mut report.unchanged,
                };
                into.push((entry.kind, entry.name.clone()));
            }
            let refused: HashSet<(EntryKind, String)> = found
                .refused
                .iter()
                .filter_map(|r| Some((r.kind?, r.name.clone()?)))
                .collect();
            let refused_paths: HashSet<&str> = found
                .refused
                .iter()
                .filter(|r| r.name.is_none())
                .map(|r| r.path.as_str())
                .collect();
            let rows = sqlx::query(
                "select kind, name, path from catalog_entries where source_id = $1                  order by kind, name",
            )
            .bind(source_id)
            .fetch_all(&mut *tx)
            .await?;
            for row in rows {
                let kind = entry_kind(&row.try_get::<String, _>("kind")?)?;
                let name: String = row.try_get("name")?;
                let path: String = row.try_get("path")?;
                let key = (kind, name.clone());
                if !seen.contains(&key)
                    && !refused.contains(&key)
                    && !refused_paths.contains(path.as_str())
                {
                    report.missing.push((kind, entry_name(&name)?));
                }
            }
            report.refused = found.refused;
            report.warnings = found.warnings;
            report.vmcps = found.vmcps;
            report.environments = found.environments;
        }
        let stored = redact.clean_json(serde_json::to_value(&report).expect("reports serialise"));
        sqlx::query("update catalog_syncs set report = $2 where id = $1")
            .bind(sync_id)
            .bind(&stored)
            .execute(&mut *tx)
            .await?;
        if outcome == SyncOutcomeKind::Ok && first {
            apply_in(&mut tx, source_id, sync_id, record.finished_at).await?;
            applied = true;
        }
        sqlx::query(
            "update catalog_sources set next_sync_at = $2, claimed_until = null where id = $1",
        )
        .bind(source_id)
        .bind(next_sync_at)
        .execute(&mut *tx)
        .await?;
        notify(&mut tx, ChangeKind::Catalog).await?;
        if applied {
            notify(&mut tx, ChangeKind::Environments).await?;
        }
        tx.commit().await?;

        let report: StoredReport = serde_json::from_value(stored).unwrap_or_default();
        Ok(SyncReport {
            id: sync_id,
            commit,
            outcome,
            error,
            new: report.new,
            updated: report.updated,
            unchanged: report.unchanged,
            missing: report.missing,
            refused: report.refused,
            warnings: report.warnings,
            vmcps: report.vmcps,
            environments: report.environments,
            applied,
        })
    }

    /// Applies the latest successful sync of `name`: its entries' found
    /// revisions become the ones environments use. Conflict when there is
    /// none, or it is applied already.
    pub async fn apply_source(&self, name: &SourceName, now: OffsetDateTime) -> Result<Applied> {
        let mut tx = self.pool.begin().await?;
        let source_id: Uuid =
            sqlx::query_scalar("select id from catalog_sources where name = $1 for update")
                .bind(name.as_str())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(StoreError::NotFound)?;
        let latest = sqlx::query(
            "select id, applied_at from catalog_syncs where source_id = $1 and outcome = 'ok' \
             order by seq desc limit 1",
        )
        .bind(source_id)
        .fetch_optional(&mut *tx)
        .await?;
        let sync_id: Uuid = match latest {
            Some(row)
                if row
                    .try_get::<Option<OffsetDateTime>, _>("applied_at")?
                    .is_none() =>
            {
                row.try_get("id")?
            }
            _ => {
                return Err(StoreError::Conflict(format!(
                    "source {name} has no sync waiting to be applied"
                )));
            }
        };
        let applied = apply_in(&mut tx, source_id, sync_id, now).await?;
        notify(&mut tx, ChangeKind::Catalog).await?;
        notify(&mut tx, ChangeKind::Environments).await?;
        tx.commit().await?;
        Ok(applied)
    }
}

enum Change {
    New,
    Updated,
    Unchanged,
}

/// Stores one found entry: its files once by SHA-256, the entry if it is
/// new, a revision if its digest is, and the entry's latest revision.
async fn store_entry(
    conn: &mut PgConnection,
    source_id: Uuid,
    sync_id: Uuid,
    commit: &str,
    entry: &super::FoundEntry,
) -> Result<Change> {
    let inserted: Option<Uuid> = sqlx::query_scalar(
        "insert into catalog_entries (source_id, kind, name, path, owner_id) \
         select $1, $2, $3, $4, added_by from catalog_sources where id = $1 \
         on conflict (source_id, kind, name) do nothing returning id",
    )
    .bind(source_id)
    .bind(entry.kind.as_str())
    .bind(entry.name.as_str())
    .bind(&entry.path)
    .fetch_optional(&mut *conn)
    .await?;
    let (entry_id, latest): (Uuid, Option<Uuid>) = match inserted {
        Some(id) => (id, None),
        None => {
            sqlx::query_as(
                "select id, latest_revision_id from catalog_entries \
             where source_id = $1 and kind = $2 and name = $3 for update",
            )
            .bind(source_id)
            .bind(entry.kind.as_str())
            .bind(entry.name.as_str())
            .fetch_one(&mut *conn)
            .await?
        }
    };

    let existing: Option<Uuid> =
        sqlx::query_scalar("select id from catalog_revisions where entry_id = $1 and digest = $2")
            .bind(entry_id)
            .bind(&entry.digest)
            .fetch_optional(&mut *conn)
            .await?;
    let revision_id = match existing {
        Some(id) => id,
        None => {
            let spec = serde_json::to_value(&entry.spec).expect("specs serialise");
            let id: Uuid = sqlx::query_scalar(
                "insert into catalog_revisions (entry_id, commit, digest, spec) \
                 values ($1, $2, $3, $4) returning id",
            )
            .bind(entry_id)
            .bind(commit)
            .bind(&entry.digest)
            .bind(&spec)
            .fetch_one(&mut *conn)
            .await?;
            for file in &entry.files {
                let sha: Vec<u8> = Sha256::digest(&file.bytes).to_vec();
                sqlx::query(
                    "insert into catalog_blobs (sha256, bytes) values ($1, $2) \
                     on conflict (sha256) do nothing",
                )
                .bind(&sha)
                .bind(&file.bytes)
                .execute(&mut *conn)
                .await?;
                sqlx::query(
                    "insert into catalog_revision_files (revision_id, path, sha256, size, executable) \
                     values ($1, $2, $3, $4, $5)",
                )
                .bind(id)
                .bind(&file.path)
                .bind(&sha)
                .bind(file.bytes.len() as i64)
                .bind(file.executable)
                .execute(&mut *conn)
                .await?;
            }
            id
        }
    };
    sqlx::query(
        "update catalog_entries set latest_revision_id = $2, latest_sync_id = $3, path = $4 \
         where id = $1",
    )
    .bind(entry_id)
    .bind(revision_id)
    .bind(sync_id)
    .bind(&entry.path)
    .execute(&mut *conn)
    .await?;
    Ok(match (inserted, latest) {
        (Some(_), _) => Change::New,
        (None, Some(previous)) if previous == revision_id => Change::Unchanged,
        (None, _) => Change::Updated,
    })
}

/// Applies a successful sync: each entry it found uses its latest revision
/// and is no longer removed; each it neither found nor refused, by name or
/// by its folder, is marked removed and keeps its revision.
async fn apply_in(
    conn: &mut PgConnection,
    source_id: Uuid,
    sync_id: Uuid,
    now: OffsetDateTime,
) -> Result<Applied> {
    let row = sqlx::query("select commit, report from catalog_syncs where id = $1")
        .bind(sync_id)
        .fetch_one(&mut *conn)
        .await?;
    let commit: Option<String> = row.try_get("commit")?;
    let report: StoredReport = serde_json::from_value(row.try_get("report")?).unwrap_or_default();
    let refused_names: HashSet<(EntryKind, String)> = report
        .refused
        .iter()
        .filter_map(|r| Some((r.kind?, r.name.clone()?)))
        .collect();
    // A refusal without a name names a path; an entry last found there is
    // kept rather than guessed removed.
    let refused_paths: HashSet<&str> = report
        .refused
        .iter()
        .filter(|r| r.name.is_none())
        .map(|r| r.path.as_str())
        .collect();

    let mut applied = Applied::default();
    let rows = sqlx::query(
        "select id, kind, name, path, latest_sync_id, applied_revision_id, latest_revision_id, \
         removed_at from catalog_entries where source_id = $1 order by kind, name for update",
    )
    .bind(source_id)
    .fetch_all(&mut *conn)
    .await?;
    for row in rows {
        let id: Uuid = row.try_get("id")?;
        let kind = entry_kind(&row.try_get::<String, _>("kind")?)?;
        let name: String = row.try_get("name")?;
        let path: String = row.try_get("path")?;
        let removed: Option<OffsetDateTime> = row.try_get("removed_at")?;
        let found = row.try_get::<Option<Uuid>, _>("latest_sync_id")? == Some(sync_id);
        if found {
            let applied_rev: Option<Uuid> = row.try_get("applied_revision_id")?;
            let latest_rev: Option<Uuid> = row.try_get("latest_revision_id")?;
            if applied_rev != latest_rev || removed.is_some() {
                sqlx::query(
                    "update catalog_entries set applied_revision_id = latest_revision_id, \
                     removed_at = null where id = $1",
                )
                .bind(id)
                .execute(&mut *conn)
                .await?;
                sqlx::query(
                    "update catalog_revisions set applied_at = $2 \
                     where id = (select latest_revision_id from catalog_entries where id = $1) \
                     and applied_at is null",
                )
                .bind(id)
                .bind(now)
                .execute(&mut *conn)
                .await?;
                applied.updated.push((kind, entry_name(&name)?));
            }
        } else if removed.is_none()
            && !refused_names.contains(&(kind, name.clone()))
            && !refused_paths.contains(path.as_str())
        {
            sqlx::query("update catalog_entries set removed_at = $2 where id = $1")
                .bind(id)
                .bind(now)
                .execute(&mut *conn)
                .await?;
            applied.removed.push((kind, entry_name(&name)?));
        }
    }
    sqlx::query("update catalog_syncs set applied_at = $2 where id = $1")
        .bind(sync_id)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    sqlx::query("update catalog_sources set applied_commit = $2, updated_at = now() where id = $1")
        .bind(source_id)
        .bind(commit)
        .execute(&mut *conn)
        .await?;
    Ok(applied)
}
