//! What machines read for an environment (D198): the delivery document
//! `sekisho use` installs from, and its files by SHA-256. For a person who
//! may use the environment, or that environment's key; every other caller,
//! an admin outside its access groups included, gets the same 404 as for an
//! environment that doesn't exist.

use axum::extract::{Path, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use sekisho_catalog::delivery::{
    Defaults, DeliveredEntry, DeliveredFile, DeliveredModel, Delivery, EntryAddress, Registries,
};
use sekisho_core::access::can_use;
use sekisho_core::formats::{ClientApi, reaches};
use sekisho_core::names::EnvName;
use sekisho_store::{Delivered, Environment, StoreError};
use uuid::Uuid;

use super::ApiError;
use crate::app::AppState;
use crate::auth::Principal;

/// The environment, when the caller may read it; `NotFound` otherwise.
async fn readable(
    state: &AppState,
    principal: &Principal,
    name: &str,
) -> Result<Environment, ApiError> {
    let name = EnvName::parse(name).map_err(|_| ApiError::NotFound)?;
    if let Principal::Key { environment, .. } = principal
        && *environment != name
    {
        return Err(ApiError::NotFound);
    }
    let environment = state.store().get_environment(&name).await?;
    if let Principal::Person(caller) = principal
        && !can_use(&caller.user.access(), &environment.spec.access_groups)
    {
        return Err(ApiError::NotFound);
    }
    Ok(environment)
}

/// The entries the environment delivers to machines: applied, not
/// quarantined. An entry gone from its source keeps its last revision.
fn installable(catalog: &[Delivered]) -> impl Iterator<Item = (&Delivered, Uuid)> {
    catalog
        .iter()
        .filter(|d| !d.quarantined)
        .filter_map(|d| d.revision.map(|r| (d, r)))
}

pub async fn delivery(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path(name): Path<String>,
) -> Result<Json<Delivery>, ApiError> {
    let environment = readable(&state, &principal, &name).await?;
    let store = state.store();
    let catalog = store.environment_catalog(environment.id).await?;
    let ids: Vec<Uuid> = installable(&catalog).map(|(_, r)| r).collect();
    let mut revisions = store.revisions(&ids).await?;
    let mut entries = Vec::with_capacity(ids.len());
    for (d, id) in installable(&catalog) {
        let revision = revisions.remove(&id).ok_or_else(|| {
            tracing::error!(revision = %id, "an applied revision is missing");
            ApiError::Internal
        })?;
        let Some(applied_at) = revision.applied_at else {
            tracing::error!(revision = %id, "an applied revision has no applied time");
            return Err(ApiError::Internal);
        };
        entries.push(DeliveredEntry {
            kind: d.key.kind,
            source: d.key.source.clone(),
            name: d.key.name.clone(),
            commit: revision.commit,
            digest: revision.digest,
            applied_at,
            tools: d.tools.clone(),
            spec: revision.spec,
            files: revision
                .files
                .into_iter()
                .map(|f| DeliveredFile {
                    path: f.path,
                    sha256: f.sha256,
                    size: f.size,
                    executable: f.executable,
                })
                .collect(),
        });
    }
    let quarantined = catalog
        .iter()
        .filter(|d| d.quarantined)
        .map(|d| EntryAddress {
            kind: d.key.kind,
            source: d.key.source.clone(),
            name: d.key.name.clone(),
        })
        .collect();
    let models = store
        .environment_models(environment.id)
        .await?
        .into_iter()
        .map(|m| DeliveredModel {
            apis: ClientApi::ALL
                .into_iter()
                .filter(|api| reaches(*api, m.kind, m.name.as_str()))
                .collect(),
            name: m.name,
        })
        .collect();
    let registries = store.package_registries().await?;
    let defaults = environment.effective_defaults;
    let name = environment.spec.name;
    Ok(Json(
        Delivery {
            gateway_url: format!(
                "{}/env/{name}",
                state.gateway_url().as_str().trim_end_matches('/')
            ),
            environment: name,
            notice: environment.spec.notice,
            revision: String::new(),
            models,
            defaults: Defaults {
                anthropic: defaults.anthropic,
                openai: defaults.openai,
                gemini: defaults.gemini,
                small: defaults.small,
            },
            mcp: None,
            entries,
            quarantined,
            registries: Registries {
                npm: registries.npm,
                python: registries.python,
            },
        }
        .seal(),
    ))
}

/// A file of an entry the environment delivers, by its SHA-256 in
/// lowercase hex. The same 404 for a file it doesn't deliver and one
/// Sekisho doesn't have.
pub async fn blob(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
    Path((name, sha256)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let environment = readable(&state, &principal, &name).await?;
    let store = state.store();
    let catalog = store.environment_catalog(environment.id).await?;
    let revisions: Vec<Uuid> = installable(&catalog).map(|(_, r)| r).collect();
    let bytes = match store.blob_in(&revisions, &sha256).await {
        Ok(bytes) => bytes,
        Err(StoreError::NotFound) => return Err(ApiError::NotFound),
        Err(e) => return Err(e.into()),
    };
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    // The content of a hash never changes; only this caller may see it.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    Ok(response)
}
