use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use sekisho_core::names::{EnvName, ModelName};
use sekisho_store::{Environment, EnvironmentSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use uuid::Uuid;

use super::keys::emails;
use super::{ApiError, ApiJson, Fields};
use crate::app::AppState;
use crate::auth::Caller;

/// The version of the environment format (design 9.2).
pub const API_VERSION: &str = "sekisho/v1alpha1";

/// The PUT body. The read-only fields of a GET answer (`id`,
/// `owner_email`, `created_at`, `updated_at`) are accepted and ignored, so a
/// fetched environment can be sent back as is.
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
    /// The owner's user ID. Without one, the stored owner stays, or a new
    /// environment's creator owns it.
    #[serde(default)]
    owner: Option<Uuid>,
    #[serde(default, rename = "owner_email")]
    _owner_email: Option<Value>,
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
    /// `null` only for an environment saved before owners existed.
    owner: Option<Uuid>,
    owner_email: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
}

impl EnvironmentBody {
    fn new(e: Environment, owner_email: Option<String>) -> Self {
        Self {
            api_version: API_VERSION,
            id: e.id,
            name: e.spec.name.to_string(),
            description: e.spec.description,
            notice: e.spec.notice,
            access_groups: e.spec.access_groups,
            models: e.spec.models.iter().map(ToString::to_string).collect(),
            owner: e.owner,
            owner_email,
            created_at: e.created_at,
            updated_at: e.updated_at,
        }
    }
}

/// The answer for one environment, with its owner's email.
async fn answer(state: &AppState, e: Environment) -> Result<EnvironmentBody, ApiError> {
    let email = match e.owner {
        Some(owner) => Some(state.store().user(&owner).await?.email),
        None => None,
    };
    Ok(EnvironmentBody::new(e, email))
}

fn path_name(name: &str) -> Result<EnvName, ApiError> {
    EnvName::parse(name).map_err(|_| ApiError::NotFound)
}

/// Creates the environment or replaces its spec.
pub async fn put(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
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
    // Without an owner in the body, the stored one stays, or the admin who
    // creates the environment owns it.
    let environment = state
        .store()
        .upsert_environment(
            EnvironmentSpec {
                name,
                description: body.description,
                notice: body.notice,
                access_groups: body.access_groups,
                models,
                owner: body.owner,
            },
            caller.user.id,
        )
        .await?;
    Ok(Json(answer(&state, environment).await?))
}

pub async fn get(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<EnvironmentBody>, ApiError> {
    let environment = state.store().get_environment(&path_name(&name)?).await?;
    Ok(Json(answer(&state, environment).await?))
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<EnvironmentBody>>, ApiError> {
    let environments = state.store().list_environments().await?;
    let emails = emails(&state).await?;
    Ok(Json(
        environments
            .into_iter()
            .map(|e| {
                let email = e.owner.and_then(|o| emails.get(&o).cloned());
                EnvironmentBody::new(e, email)
            })
            .collect(),
    ))
}

pub async fn delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.store().delete_environment(&path_name(&name)?).await?;
    Ok(StatusCode::NO_CONTENT)
}
