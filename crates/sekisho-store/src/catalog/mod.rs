//! Catalogs (plan 3): Git sources, their syncs, the entries they hold with
//! revisions, quarantine, and registry credentials.
//!
//! A sync records what it found; environments use each entry's applied
//! revision. A source's first sync is applied at once; later ones wait for
//! an admin (D164). Locks are taken as everywhere: users, then sources,
//! then entries.

mod entries;
mod registries;
mod sources;
mod syncs;
mod toolkits;

pub use toolkits::ToolkitMember;

use sekisho_catalog::model::{CheckedEntry, EntrySpec, ObotVmcp, Refusal, Warning};
use sekisho_catalog::names::{EntryKind, EntryName, SourceKind, SourceName};
use sekisho_catalog::source::{Folder, GitRef, SourceUrl};
use sekisho_catalog::tree::TreeFile;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::StoreError;
use crate::error::bad_column;

/// A source to add.
#[derive(Clone, Debug)]
pub struct NewSource {
    pub name: SourceName,
    pub kind: SourceKind,
    pub url: SourceUrl,
    pub git_ref: Option<GitRef>,
    pub folder: Folder,
    /// Sent with the token; Git hosts each want their own (D165).
    pub username: Option<String>,
    pub token: Option<String>,
    pub allow_private_network: bool,
}

/// Changes to a source; `None` keeps a setting. `Some(None)` clears an
/// optional one.
#[derive(Clone, Debug, Default)]
pub struct SourcePatch {
    pub url: Option<SourceUrl>,
    pub git_ref: Option<Option<GitRef>>,
    pub folder: Option<Folder>,
    pub username: Option<Option<String>>,
    pub token: Option<Option<String>>,
    pub allow_private_network: Option<bool>,
}

/// A source as stored. Its token is never in it: only `has_token`.
#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub id: Uuid,
    pub name: SourceName,
    pub kind: SourceKind,
    pub url: SourceUrl,
    pub git_ref: Option<GitRef>,
    pub folder: Folder,
    pub username: Option<String>,
    pub has_token: bool,
    pub allow_private_network: bool,
    pub added_by: Uuid,
    pub applied_commit: Option<String>,
    pub latest_sync: Option<SyncSummary>,
    /// The latest successful sync waits for an admin's apply.
    pub pending: bool,
    /// A replica holds the source's claim.
    pub syncing: bool,
    /// When its settings last changed: a sync before that read something
    /// else.
    pub updated_at: OffsetDateTime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncOutcomeKind {
    Ok,
    Unchanged,
    Failed,
}

impl SyncOutcomeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SyncOutcomeKind::Ok => "ok",
            SyncOutcomeKind::Unchanged => "unchanged",
            SyncOutcomeKind::Failed => "failed",
        }
    }

    pub(crate) fn parse(s: &str) -> Result<Self, StoreError> {
        match s {
            "ok" => Ok(SyncOutcomeKind::Ok),
            "unchanged" => Ok(SyncOutcomeKind::Unchanged),
            "failed" => Ok(SyncOutcomeKind::Failed),
            other => Err(bad_column("outcome", other)),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SyncSummary {
    pub id: Uuid,
    pub started_at: OffsetDateTime,
    pub finished_at: OffsetDateTime,
    pub commit: Option<String>,
    pub outcome: SyncOutcomeKind,
    pub error: Option<String>,
    pub applied_at: Option<OffsetDateTime>,
}

/// An entry a sync found, ready to store.
#[derive(Clone, Debug, PartialEq)]
pub struct FoundEntry {
    pub kind: EntryKind,
    pub name: EntryName,
    pub path: String,
    pub spec: EntrySpec,
    pub digest: String,
    pub files: Vec<TreeFile>,
}

impl FoundEntry {
    /// Takes a checked entry with its revision digest.
    pub fn from_checked(entry: CheckedEntry) -> Self {
        let digest = entry.digest();
        Self {
            kind: entry.kind,
            name: entry.name,
            path: entry.path,
            spec: entry.spec,
            digest,
            files: entry.files,
        }
    }
}

/// Everything a successful sync found.
#[derive(Clone, Debug, Default)]
pub struct SyncFound {
    pub entries: Vec<FoundEntry>,
    pub refused: Vec<Refusal>,
    pub warnings: Vec<Warning>,
    pub vmcps: Vec<ObotVmcp>,
    /// Git mode's report (Task 10).
    pub environments: serde_json::Value,
}

#[derive(Clone, Debug)]
pub enum SyncOutcome {
    Ok(SyncFound),
    /// The ref hasn't moved since the latest successful sync.
    Unchanged,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct SyncRecord {
    pub commit: Option<String>,
    pub started_at: OffsetDateTime,
    pub finished_at: OffsetDateTime,
    pub outcome: SyncOutcome,
}

/// What a recorded sync found, as stored.
#[derive(Clone, Debug, PartialEq)]
pub struct SyncReport {
    pub id: Uuid,
    pub started_at: OffsetDateTime,
    pub finished_at: OffsetDateTime,
    pub commit: Option<String>,
    pub outcome: SyncOutcomeKind,
    pub error: Option<String>,
    pub new: Vec<(EntryKind, EntryName)>,
    pub updated: Vec<(EntryKind, EntryName)>,
    pub unchanged: Vec<(EntryKind, EntryName)>,
    /// Entries of the source that this sync didn't find; apply marks them
    /// removed.
    pub missing: Vec<(EntryKind, EntryName)>,
    pub refused: Vec<Refusal>,
    pub warnings: Vec<Warning>,
    pub vmcps: Vec<ObotVmcp>,
    pub environments: serde_json::Value,
    /// The sync is applied: a source's first as it is recorded, later ones
    /// by an admin.
    pub applied: bool,
}

/// What an apply changed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Applied {
    pub updated: Vec<(EntryKind, EntryName)>,
    pub removed: Vec<(EntryKind, EntryName)>,
}

/// An entry's address: its kind, source and name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EntryKey {
    pub kind: EntryKind,
    pub source: SourceName,
    pub name: EntryName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMeta {
    pub path: String,
    /// Lowercase hex.
    pub sha256: String,
    pub size: u64,
    pub executable: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Revision {
    pub id: Uuid,
    pub commit: String,
    pub digest: String,
    pub spec: EntrySpec,
    pub applied_at: Option<OffsetDateTime>,
    pub files: Vec<FileMeta>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quarantine {
    pub reason: String,
    pub by: Uuid,
    pub at: OffsetDateTime,
}

/// One period in quarantine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantineRecord {
    pub reason: String,
    pub quarantined_by: Uuid,
    pub quarantined_at: OffsetDateTime,
    pub lifted_by: Option<Uuid>,
    pub lifted_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: Uuid,
    pub key: EntryKey,
    pub owner: Uuid,
    /// Where in its source the latest sync found it.
    pub path: String,
    /// What environments use; `None` until its source's sync is applied.
    pub applied: Option<Revision>,
    /// A newer revision waiting for an admin's apply.
    pub pending: Option<Revision>,
    pub removed_at: Option<OffsetDateTime>,
    pub quarantine: Option<Quarantine>,
    /// The toolkits whose applied revision lists it.
    pub toolkits: Vec<EntryKey>,
}

/// Why a reference names no single entry.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no such entry")]
    NotFound,
    /// The bare name is in several sources; write `name@source`.
    #[error("in sources {}", .0.iter().map(SourceName::as_str).collect::<Vec<_>>().join(", "))]
    Ambiguous(Vec<SourceName>),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<sqlx::Error> for ResolveError {
    fn from(e: sqlx::Error) -> Self {
        ResolveError::Store(e.into())
    }
}

pub(crate) fn entry_kind(s: &str) -> Result<EntryKind, StoreError> {
    EntryKind::parse(s).ok_or_else(|| bad_column("kind", s))
}

pub(crate) fn source_kind(s: &str) -> Result<SourceKind, StoreError> {
    SourceKind::parse(s).ok_or_else(|| bad_column("kind", s))
}

pub(crate) fn source_name(s: &str) -> Result<SourceName, StoreError> {
    if s == SourceName::git_mode().as_str() {
        return Ok(SourceName::git_mode());
    }
    SourceName::parse(s).map_err(|_| bad_column("name", s))
}

pub(crate) fn entry_name(s: &str) -> Result<EntryName, StoreError> {
    EntryName::parse(s).map_err(|_| bad_column("name", s))
}
