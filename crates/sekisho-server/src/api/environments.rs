use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use sekisho_core::names::{EnvName, ModelName};
use sekisho_store::{Environment, EnvironmentSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{ApiError, ApiJson, Fields};
use crate::app::AppState;

/// The version of the environment format (design 9.2).
pub const API_VERSION: &str = "sekisho/v1alpha1";

/// The PUT body. The read-only fields of a GET answer (`id`, `created_at`,
/// `updated_at`) are accepted and ignored, so a fetched environment can be
/// sent back as is.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentPut {
    #[serde(default)]
    api_version: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: String,
    #[serde(default)]
    notice: String,
    #[serde(default)]
    access_groups: Vec<String>,
    #[serde(default)]
    models: Vec<String>,
    #[serde(default, rename = "id")]
    _id: Option<Value>,
    #[serde(default, rename = "created_at")]
    _created_at: Option<Value>,
    #[serde(default, rename = "updated_at")]
    _updated_at: Option<Value>,
}

#[derive(Serialize)]
pub struct EnvironmentBody {
    api_version: &'static str,
    id: Uuid,
    name: String,
    description: String,
    notice: String,
    access_groups: Vec<String>,
    models: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

impl From<Environment> for EnvironmentBody {
    fn from(e: Environment) -> Self {
        Self {
            api_version: API_VERSION,
            id: e.id,
            name: e.spec.name.to_string(),
            description: e.spec.description,
            notice: e.spec.notice,
            access_groups: e.spec.access_groups,
            models: e.spec.models.iter().map(ToString::to_string).collect(),
            created_at: e.created_at,
            updated_at: e.updated_at,
        }
    }
}

fn path_name(name: &str) -> Result<EnvName, ApiError> {
    EnvName::parse(name).map_err(|_| ApiError::NotFound)
}

/// Creates the environment or replaces its spec.
pub async fn put(
    State(state): State<AppState>,
    Path(path): Path<String>,
    ApiJson(body): ApiJson<EnvironmentPut>,
) -> Result<Json<EnvironmentBody>, ApiError> {
    let mut fields = Fields::default();
    if body
        .api_version
        .as_deref()
        .is_some_and(|v| v != API_VERSION)
    {
        fields.push("api_version", format!("use {API_VERSION}"));
    }
    if body.name.as_deref().is_some_and(|n| n != path) {
        fields.push("name", "must match the name in the path");
    }
    let name = fields.check("name", EnvName::parse(&path));
    let models: Vec<Option<ModelName>> = body
        .models
        .iter()
        .enumerate()
        .map(|(i, m)| fields.check(&format!("models[{i}]"), ModelName::parse(m)))
        .collect();
    let (Some(name), Some(models)) = (name, models.into_iter().collect::<Option<Vec<_>>>()) else {
        return Err(fields.fail());
    };
    fields.finish()?;
    let environment = state
        .store()
        .upsert_environment(EnvironmentSpec {
            name,
            description: body.description,
            notice: body.notice,
            access_groups: body.access_groups,
            models,
        })
        .await?;
    Ok(Json(environment.into()))
}

pub async fn get(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<EnvironmentBody>, ApiError> {
    let environment = state.store().get_environment(&path_name(&name)?).await?;
    Ok(Json(environment.into()))
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<EnvironmentBody>>, ApiError> {
    let environments = state.store().list_environments().await?;
    Ok(Json(environments.into_iter().map(Into::into).collect()))
}

pub async fn delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.store().delete_environment(&path_name(&name)?).await?;
    Ok(StatusCode::NO_CONTENT)
}
