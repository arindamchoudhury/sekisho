//! Catalog sources, for admins: adding, changing and deleting them, syncing
//! one now, reading the latest sync's report, applying it, and importing
//! Obot's catalogs. A source's token is never answered: only `has_token`.

use std::collections::HashSet;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, response::IntoResponse};
use sekisho_catalog::model::{ObotVmcp, Refusal, Warning};
use sekisho_catalog::names::{EntryKind, EntryName, SourceKind, SourceName};
use sekisho_catalog::source::{Folder, GitRef, ObotRepository, SourceUrl};
use sekisho_store::{Applied, NewSource, Source, SourcePatch, StoreError, SyncReport, SyncSummary};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{ApiError, ApiJson, Fields, present};
use crate::app::AppState;
use crate::auth::Caller;
use crate::catalog::sync::{LEASE, sync_source};

/// The most URLs one Obot import takes.
const MAX_IMPORT: usize = 50;

#[derive(Serialize)]
pub struct SyncSummaryBody {
    id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    finished_at: OffsetDateTime,
    commit: Option<String>,
    /// `ok`, `unchanged` or `failed`.
    outcome: &'static str,
    error: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    applied_at: Option<OffsetDateTime>,
}

impl From<SyncSummary> for SyncSummaryBody {
    fn from(s: SyncSummary) -> Self {
        Self {
            id: s.id,
            started_at: s.started_at,
            finished_at: s.finished_at,
            commit: s.commit,
            outcome: s.outcome.as_str(),
            error: s.error,
            applied_at: s.applied_at,
        }
    }
}

#[derive(Serialize)]
pub struct SourceBody {
    name: String,
    kind: SourceKind,
    url: String,
    #[serde(rename = "ref")]
    git_ref: Option<String>,
    /// Empty is the repository's root.
    folder: String,
    username: Option<String>,
    has_token: bool,
    allow_private_network: bool,
    added_by: Uuid,
    /// The commit environments use.
    applied_commit: Option<String>,
    latest_sync: Option<SyncSummaryBody>,
    /// The latest successful sync waits for an admin's apply.
    pending: bool,
    /// A sync of it is running.
    syncing: bool,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

impl From<Source> for SourceBody {
    fn from(s: Source) -> Self {
        Self {
            name: s.name.to_string(),
            kind: s.kind,
            url: s.url.to_string(),
            git_ref: s.git_ref.map(|r| r.to_string()),
            folder: s.folder.as_str().to_owned(),
            username: s.username,
            has_token: s.has_token,
            allow_private_network: s.allow_private_network,
            added_by: s.added_by,
            applied_commit: s.applied_commit,
            latest_sync: s.latest_sync.map(Into::into),
            pending: s.pending,
            syncing: s.syncing,
            updated_at: s.updated_at,
        }
    }
}

/// An entry named in a report: `{"kind": "skill", "name": "pdf"}`.
#[derive(Serialize)]
pub struct EntryNameBody {
    kind: EntryKind,
    name: EntryName,
}

fn named(list: Vec<(EntryKind, EntryName)>) -> Vec<EntryNameBody> {
    list.into_iter()
        .map(|(kind, name)| EntryNameBody { kind, name })
        .collect()
}

#[derive(Serialize)]
pub struct SyncReportBody {
    id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    finished_at: OffsetDateTime,
    commit: Option<String>,
    outcome: &'static str,
    error: Option<String>,
    /// Environments use what this sync found.
    applied: bool,
    new: Vec<EntryNameBody>,
    updated: Vec<EntryNameBody>,
    unchanged: Vec<EntryNameBody>,
    /// Entries this sync didn't find; apply marks them removed.
    missing: Vec<EntryNameBody>,
    refused: Vec<Refusal>,
    warnings: Vec<Warning>,
    /// Obot virtual MCP servers found, offered as environments.
    vmcps: Vec<ObotVmcp>,
    /// Git mode's report.
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    environments: serde_json::Value,
}

impl From<SyncReport> for SyncReportBody {
    fn from(r: SyncReport) -> Self {
        Self {
            id: r.id,
            started_at: r.started_at,
            finished_at: r.finished_at,
            commit: r.commit,
            outcome: r.outcome.as_str(),
            error: r.error,
            applied: r.applied,
            new: named(r.new),
            updated: named(r.updated),
            unchanged: named(r.unchanged),
            missing: named(r.missing),
            refused: r.refused,
            warnings: r.warnings,
            vmcps: r.vmcps,
            environments: r.environments,
        }
    }
}

#[derive(Serialize)]
pub struct AppliedBody {
    updated: Vec<EntryNameBody>,
    removed: Vec<EntryNameBody>,
    /// Entries left on their earlier version: another source's entry took
    /// their name first (D217).
    refused: Vec<Refusal>,
}

impl From<Applied> for AppliedBody {
    fn from(a: Applied) -> Self {
        Self {
            updated: named(a.updated),
            removed: named(a.removed),
            refused: a.refused,
        }
    }
}

fn path_name(name: &str) -> Result<SourceName, ApiError> {
    SourceName::parse(name).map_err(|_| ApiError::NotFound)
}

/// Claims `name` and syncs it on a task of its own; a replica that holds it
/// already syncs it.
pub(crate) fn sync_in_background(state: &AppState, name: SourceName) {
    let state = state.clone();
    tokio::spawn(async move {
        match state
            .store()
            .claim_source(&name, OffsetDateTime::now_utc(), LEASE)
            .await
        {
            Ok(true) => {
                if let Err(error) = sync_source(&state, &name, false).await {
                    tracing::warn!(source = %name, ?error, "catalog sync failed");
                }
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(source = %name, %error, "can't claim a catalog source"),
        }
    });
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<SourceBody>>, ApiError> {
    let sources = state.store().list_sources().await?;
    Ok(Json(sources.into_iter().map(Into::into).collect()))
}

pub async fn get(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SourceBody>, ApiError> {
    let source = state.store().get_source(&path_name(&name)?).await?;
    Ok(Json(source.into()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewSourceBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default, rename = "ref")]
    git_ref: Option<String>,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    allow_private_network: bool,
}

/// Adds a source and starts its first sync, which applies itself.
pub async fn create(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    ApiJson(body): ApiJson<NewSourceBody>,
) -> Result<impl IntoResponse, ApiError> {
    let mut fields = Fields::default();
    let name = fields
        .required("name", body.name)
        .and_then(|n| fields.check("name", SourceName::parse(&n)));
    let kind = fields
        .required("kind", body.kind)
        .and_then(|k| match SourceKind::parse(&k) {
            Some(SourceKind::Environments) => {
                fields.push("kind", "Git mode's environments source isn't added here");
                None
            }
            Some(kind) => Some(kind),
            None => {
                fields.push("kind", "use catalog, skills, plugins or prompts");
                None
            }
        });
    let url = fields.required("url", body.url).and_then(|u| {
        fields.check(
            "url",
            SourceUrl::parse(&u, state.catalog().allow_plain_http),
        )
    });
    let git_ref = match body.git_ref {
        None => Some(None),
        Some(r) => fields.check("ref", GitRef::parse(&r)).map(Some),
    };
    let folder = fields.check(
        "folder",
        Folder::parse(body.folder.as_deref().unwrap_or("")),
    );
    let (Some(name), Some(kind), Some(url), Some(git_ref), Some(folder)) =
        (name, kind, url, git_ref, folder)
    else {
        return Err(fields.fail());
    };
    let source = state
        .store()
        .add_source(
            state.secret_box(),
            NewSource {
                name: name.clone(),
                kind,
                url,
                git_ref,
                folder,
                username: body.username,
                token: body.token,
                allow_private_network: body.allow_private_network,
            },
            caller.user.id,
        )
        .await?;
    tracing::info!(source = %name, kind = kind.as_str(), by = %caller.user.id, "added a catalog source");
    sync_in_background(&state, name);
    Ok((StatusCode::CREATED, Json(SourceBody::from(source))))
}

/// A change to a source; absent fields stay. `ref`, `username` and `token`
/// may be `null` to clear them. The name and kind never change.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcePatchBody {
    #[serde(default)]
    url: Option<String>,
    #[serde(default, rename = "ref", deserialize_with = "present")]
    git_ref: Option<Option<String>>,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default, deserialize_with = "present")]
    username: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    token: Option<Option<String>>,
    #[serde(default)]
    allow_private_network: Option<bool>,
}

pub async fn update(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<SourcePatchBody>,
) -> Result<Json<SourceBody>, ApiError> {
    let name = path_name(&name)?;
    let mut fields = Fields::default();
    let mut patch = SourcePatch {
        username: body.username,
        token: body.token,
        allow_private_network: body.allow_private_network,
        ..SourcePatch::default()
    };
    if let Some(url) = body.url {
        patch.url = fields.check(
            "url",
            SourceUrl::parse(&url, state.catalog().allow_plain_http),
        );
    }
    match body.git_ref {
        None => {}
        Some(None) => patch.git_ref = Some(None),
        Some(Some(r)) => patch.git_ref = fields.check("ref", GitRef::parse(&r)).map(Some),
    }
    if let Some(folder) = body.folder {
        patch.folder = fields.check("folder", Folder::parse(&folder));
    }
    fields.finish()?;
    let source = state
        .store()
        .update_source(state.secret_box(), &name, patch)
        .await?;
    tracing::info!(source = %name, by = %caller.user.id, "changed a catalog source");
    Ok(Json(source.into()))
}

/// Deletes a source with its syncs and entries.
pub async fn delete(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let name = path_name(&name)?;
    state.store().delete_source(&name).await?;
    tracing::info!(source = %name, by = %caller.user.id, "deleted a catalog source");
    Ok(StatusCode::NO_CONTENT)
}

/// Starts a sync now: 202, or 409 while one runs.
pub async fn sync(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<(StatusCode, Json<SourceBody>), ApiError> {
    let name = path_name(&name)?;
    let store = state.store();
    if !store
        .claim_source(&name, OffsetDateTime::now_utc(), LEASE)
        .await?
    {
        return Err(ApiError::Conflict(format!("a sync of {name} is running")));
    }
    let source = store.get_source(&name).await?;
    let task = state.clone();
    let synced = name.clone();
    tokio::spawn(async move {
        if let Err(error) = sync_source(&task, &synced, false).await {
            tracing::warn!(source = %synced, ?error, "catalog sync failed");
        }
    });
    Ok((StatusCode::ACCEPTED, Json(source.into())))
}

pub async fn latest_sync(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SyncReportBody>, ApiError> {
    let report = state.store().latest_sync_report(&path_name(&name)?).await?;
    Ok(Json(report.into()))
}

/// Applies the latest sync: environments use what it found.
pub async fn apply(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
) -> Result<Json<AppliedBody>, ApiError> {
    let name = path_name(&name)?;
    let applied = state
        .store()
        .apply_source(&name, OffsetDateTime::now_utc())
        .await?;
    tracing::info!(source = %name, by = %caller.user.id, updated = applied.updated.len(), removed = applied.removed.len(), "applied a catalog source");
    Ok(Json(applied.into()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportObotBody {
    urls: Vec<String>,
    /// Sent with `token`, as on a source.
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

#[derive(Serialize)]
pub struct ImportedBody {
    sources: Vec<SourceBody>,
}

/// Adds Obot catalog repositories as `catalog` sources, each named
/// `obot-<repo>` (then `-2`, `-3`… when taken) and synced at once. Every
/// URL is checked before any source is added.
pub async fn import_obot(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    ApiJson(body): ApiJson<ImportObotBody>,
) -> Result<(StatusCode, Json<ImportedBody>), ApiError> {
    let mut fields = Fields::default();
    if body.urls.is_empty() || body.urls.len() > MAX_IMPORT {
        fields.push("urls", format!("give 1-{MAX_IMPORT} URLs"));
    }
    let repos: Vec<ObotRepository> = body
        .urls
        .iter()
        .enumerate()
        .filter_map(|(i, url)| {
            fields.check(
                &format!("urls[{i}]"),
                ObotRepository::parse(url, state.catalog().allow_plain_http),
            )
        })
        .collect();
    fields.finish()?;

    let store = state.store();
    let mut taken: HashSet<String> = store
        .list_sources()
        .await?
        .into_iter()
        .map(|s| s.name.to_string())
        .collect();
    let mut added = Vec::new();
    for repo in repos {
        let mut n = 1;
        let source = loop {
            let name = repo.source_name(n);
            n += 1;
            if !taken.insert(name.to_string()) {
                continue;
            }
            let new = NewSource {
                name,
                kind: SourceKind::Catalog,
                url: repo.url.clone(),
                git_ref: repo.git_ref.clone(),
                folder: Folder::default(),
                username: body.username.clone(),
                token: body.token.clone(),
                allow_private_network: false,
            };
            match store
                .add_source(state.secret_box(), new, caller.user.id)
                .await
            {
                // Added meanwhile by someone else: the next number.
                Err(StoreError::Conflict(_)) => continue,
                other => break other?,
            }
        };
        tracing::info!(source = %source.name, by = %caller.user.id, "imported an Obot catalog");
        sync_in_background(&state, source.name.clone());
        added.push(source.into());
    }
    Ok((StatusCode::CREATED, Json(ImportedBody { sources: added })))
}
