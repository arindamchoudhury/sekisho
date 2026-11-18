//! Catalog entries, for admins: listing them by kind, reading one with its
//! revisions and files (a toolkit's with its members), changing its owner,
//! and quarantining it.

use axum::Extension;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use sekisho_catalog::model::EntrySpec;
use sekisho_catalog::names::{EntryKind, EntryName, SourceName};
use sekisho_store::{
    Entry, EntryKey, FileMeta, Quarantine, QuarantineRecord, Revision, ToolkitMember,
};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{ApiError, ApiJson, Fields};
use crate::app::AppState;
use crate::auth::Caller;

#[derive(Serialize)]
pub struct QuarantineBody {
    reason: String,
    by: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
}

impl From<Quarantine> for QuarantineBody {
    fn from(q: Quarantine) -> Self {
        Self {
            reason: q.reason,
            by: q.by,
            at: q.at,
        }
    }
}

#[derive(Serialize)]
pub struct QuarantineRecordBody {
    reason: String,
    quarantined_by: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    quarantined_at: OffsetDateTime,
    lifted_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339::option")]
    lifted_at: Option<OffsetDateTime>,
}

impl From<QuarantineRecord> for QuarantineRecordBody {
    fn from(q: QuarantineRecord) -> Self {
        Self {
            reason: q.reason,
            quarantined_by: q.quarantined_by,
            quarantined_at: q.quarantined_at,
            lifted_by: q.lifted_by,
            lifted_at: q.lifted_at,
        }
    }
}

/// A revision in a listing: which commit, and its digest.
#[derive(Serialize)]
pub struct RevisionSummaryBody {
    id: Uuid,
    commit: String,
    digest: String,
    #[serde(with = "time::serde::rfc3339::option")]
    applied_at: Option<OffsetDateTime>,
}

impl From<&Revision> for RevisionSummaryBody {
    fn from(r: &Revision) -> Self {
        Self {
            id: r.id,
            commit: r.commit.clone(),
            digest: r.digest.clone(),
            applied_at: r.applied_at,
        }
    }
}

#[derive(Serialize)]
pub struct FileBody {
    path: String,
    size: u64,
    /// Lowercase hex.
    sha256: String,
    executable: bool,
}

impl From<FileMeta> for FileBody {
    fn from(f: FileMeta) -> Self {
        Self {
            path: f.path,
            size: f.size,
            sha256: f.sha256,
            executable: f.executable,
        }
    }
}

/// An entry a toolkit revision lists, as bound at sync.
#[derive(Serialize)]
pub struct MemberBody {
    kind: EntryKind,
    /// As the toolkit writes it: `name` or `name@source`.
    reference: String,
    source: SourceName,
    name: EntryName,
    /// An MCP server's allowed tools; `null` allows them all.
    tools: Option<Vec<String>>,
    quarantined: bool,
    removed: bool,
}

impl From<ToolkitMember> for MemberBody {
    fn from(m: ToolkitMember) -> Self {
        Self {
            kind: m.kind,
            reference: m.written,
            source: m.key.source,
            name: m.key.name,
            tools: m.tools,
            quarantined: m.quarantined,
            removed: m.removed,
        }
    }
}

/// A revision with what it holds.
#[derive(Serialize)]
pub struct RevisionBody {
    #[serde(flatten)]
    summary: RevisionSummaryBody,
    spec: EntrySpec,
    files: Vec<FileBody>,
    /// A toolkit's entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    members: Option<Vec<MemberBody>>,
}

impl RevisionBody {
    async fn new(state: &AppState, r: Revision) -> Result<Self, ApiError> {
        let members = match r.spec {
            EntrySpec::Toolkit(_) => Some(
                state
                    .store()
                    .toolkit_members(r.id)
                    .await?
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            ),
            _ => None,
        };
        Ok(Self {
            summary: (&r).into(),
            spec: r.spec,
            files: r.files.into_iter().map(Into::into).collect(),
            members,
        })
    }
}

/// The toolkits whose applied revision lists an entry, as
/// `<source>/<name>`.
fn toolkit_names(toolkits: &[EntryKey]) -> Vec<String> {
    toolkits
        .iter()
        .map(|k| format!("{}/{}", k.source, k.name))
        .collect()
}

#[derive(Serialize)]
pub struct EntrySummaryBody {
    kind: EntryKind,
    source: SourceName,
    name: EntryName,
    /// Where in its source the latest sync found it.
    path: String,
    owner: Uuid,
    /// What environments use; `null` until its source's sync is applied.
    applied: Option<RevisionSummaryBody>,
    /// A newer revision waiting for an admin's apply.
    pending: Option<RevisionSummaryBody>,
    #[serde(with = "time::serde::rfc3339::option")]
    removed_at: Option<OffsetDateTime>,
    quarantine: Option<QuarantineBody>,
    /// The toolkits whose applied revision lists it, as `<source>/<name>`.
    toolkits: Vec<String>,
    /// The environments that list it themselves.
    environments: Vec<String>,
}

impl From<Entry> for EntrySummaryBody {
    fn from(e: Entry) -> Self {
        Self {
            kind: e.key.kind,
            source: e.key.source,
            name: e.key.name,
            path: e.path,
            owner: e.owner,
            applied: e.applied.as_ref().map(Into::into),
            pending: e.pending.as_ref().map(Into::into),
            removed_at: e.removed_at,
            quarantine: e.quarantine.map(Into::into),
            toolkits: toolkit_names(&e.toolkits),
            environments: e.environments.iter().map(ToString::to_string).collect(),
        }
    }
}

#[derive(Serialize)]
pub struct EntryBody {
    kind: EntryKind,
    source: SourceName,
    name: EntryName,
    path: String,
    owner: Uuid,
    applied: Option<RevisionBody>,
    pending: Option<RevisionBody>,
    #[serde(with = "time::serde::rfc3339::option")]
    removed_at: Option<OffsetDateTime>,
    quarantine: Option<QuarantineBody>,
    /// Every period in quarantine, oldest first.
    quarantine_history: Vec<QuarantineRecordBody>,
    toolkits: Vec<String>,
    environments: Vec<String>,
}

fn kind_segment(kind: &str) -> Result<EntryKind, ApiError> {
    EntryKind::from_path_segment(kind).ok_or(ApiError::NotFound)
}

fn entry_key(kind: &str, source: &str, name: &str) -> Result<EntryKey, ApiError> {
    Ok(EntryKey {
        kind: kind_segment(kind)?,
        source: SourceName::parse(source).map_err(|_| ApiError::NotFound)?,
        name: EntryName::parse(name).map_err(|_| ApiError::NotFound)?,
    })
}

async fn entry_body(state: &AppState, key: &EntryKey) -> Result<EntryBody, ApiError> {
    let store = state.store();
    let e = store.get_entry(key).await?;
    let history = store.quarantine_history(key).await?;
    Ok(EntryBody {
        kind: e.key.kind,
        source: e.key.source,
        name: e.key.name,
        path: e.path,
        owner: e.owner,
        applied: match e.applied {
            Some(r) => Some(RevisionBody::new(state, r).await?),
            None => None,
        },
        pending: match e.pending {
            Some(r) => Some(RevisionBody::new(state, r).await?),
            None => None,
        },
        removed_at: e.removed_at,
        quarantine: e.quarantine.map(Into::into),
        quarantine_history: history.into_iter().map(Into::into).collect(),
        toolkits: toolkit_names(&e.toolkits),
        environments: e.environments.iter().map(ToString::to_string).collect(),
    })
}

/// The entries of one kind, by source and name.
pub async fn list(
    State(state): State<AppState>,
    Path(kind): Path<String>,
) -> Result<Json<Vec<EntrySummaryBody>>, ApiError> {
    let entries = state
        .store()
        .list_entries(Some(kind_segment(&kind)?))
        .await?;
    Ok(Json(entries.into_iter().map(Into::into).collect()))
}

pub async fn get(
    State(state): State<AppState>,
    Path((kind, source, name)): Path<(String, String, String)>,
) -> Result<Json<EntryBody>, ApiError> {
    let key = entry_key(&kind, &source, &name)?;
    Ok(Json(entry_body(&state, &key).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryPatchBody {
    #[serde(default)]
    owner: Option<Uuid>,
}

/// Hands the entry to another owner, an enabled user.
pub async fn patch(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path((kind, source, name)): Path<(String, String, String)>,
    ApiJson(body): ApiJson<EntryPatchBody>,
) -> Result<Json<EntryBody>, ApiError> {
    let key = entry_key(&kind, &source, &name)?;
    let Some(owner) = body.owner else {
        let mut fields = Fields::default();
        fields.push("owner", "required");
        return Err(fields.fail());
    };
    state.store().set_entry_owner(&key, owner).await?;
    tracing::info!(entry = %format_args!("{kind}/{source}/{name}"), %owner, by = %caller.user.id, "changed a catalog entry's owner");
    Ok(Json(entry_body(&state, &key).await?))
}

/// A file of the entry's applied revision, as bytes. Never rendered: it
/// came from a Git repository.
pub async fn file(
    State(state): State<AppState>,
    Path((kind, source, name, path)): Path<(String, String, String, String)>,
) -> Result<Response, ApiError> {
    let key = entry_key(&kind, &source, &name)?;
    let bytes = state.store().entry_file(&key, &path).await?;
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment"),
    );
    Ok(response)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantineRequest {
    #[serde(default)]
    reason: String,
}

/// Quarantines the entry in every environment at once; 409 if it is
/// already.
pub async fn quarantine(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path((kind, source, name)): Path<(String, String, String)>,
    ApiJson(body): ApiJson<QuarantineRequest>,
) -> Result<Json<EntryBody>, ApiError> {
    let key = entry_key(&kind, &source, &name)?;
    state
        .store()
        .quarantine(
            &key,
            &body.reason,
            caller.user.id,
            OffsetDateTime::now_utc(),
        )
        .await?;
    tracing::warn!(entry = %format_args!("{kind}/{source}/{name}"), by = %caller.user.id, "quarantined a catalog entry");
    Ok(Json(entry_body(&state, &key).await?))
}

/// Lifts the entry's quarantine; 409 if it isn't quarantined.
pub async fn lift(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path((kind, source, name)): Path<(String, String, String)>,
) -> Result<Json<EntryBody>, ApiError> {
    let key = entry_key(&kind, &source, &name)?;
    state
        .store()
        .lift_quarantine(&key, caller.user.id, OffsetDateTime::now_utc())
        .await?;
    tracing::info!(entry = %format_args!("{kind}/{source}/{name}"), by = %caller.user.id, "lifted a catalog entry's quarantine");
    Ok(Json(entry_body(&state, &key).await?))
}
