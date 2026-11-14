//! A signed-in person's own routes: who they are, the environments they may
//! use, and their keys, which only they create.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use sekisho_core::access::can_use;
use sekisho_core::keys::EnvKeyId;
use sekisho_core::names::EnvName;
use sekisho_store::{FieldError, StoreError};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use super::keys::{CreatedKeyBody, KeyBody};
use super::{ApiError, ApiJson};
use crate::app::AppState;
use crate::auth::Caller;

#[derive(Serialize)]
pub struct MeBody {
    id: Uuid,
    email: String,
    name: String,
    local: bool,
    /// By mark or by group.
    admin: bool,
    /// Every group they're in: their identity provider's and Sekisho's.
    groups: Vec<String>,
}

pub async fn me(Extension(caller): Extension<Caller>) -> Json<MeBody> {
    let groups = caller.user.access().groups.into_iter().collect();
    Json(MeBody {
        id: caller.user.id,
        email: caller.user.email,
        name: caller.user.name,
        local: caller.user.local,
        admin: caller.admin,
        groups,
    })
}

#[derive(Serialize)]
pub struct MyEnvironmentBody {
    name: String,
    description: String,
    notice: String,
    models: Vec<String>,
}

/// The environments they may use: one of their groups is among its access
/// groups. Admins get no others.
pub async fn environments(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Vec<MyEnvironmentBody>>, ApiError> {
    let access = caller.user.access();
    let environments = state.store().list_environments().await?;
    Ok(Json(
        environments
            .into_iter()
            .filter(|e| can_use(&access, &e.spec.access_groups))
            .map(|e| MyEnvironmentBody {
                name: e.spec.name.to_string(),
                description: e.spec.description,
                notice: e.spec.notice,
                models: e.spec.models.iter().map(ToString::to_string).collect(),
            })
            .collect(),
    ))
}

pub async fn keys(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Vec<KeyBody>>, ApiError> {
    let keys = state.store().list_user_keys(&caller.user.id).await?;
    Ok(Json(
        keys.into_iter()
            .map(|k| KeyBody::new(k, Some(caller.user.email.clone())))
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewKeyBody {
    environment: String,
    name: String,
    #[serde(default, with = "time::serde::rfc3339::option")]
    expires_at: Option<OffsetDateTime>,
}

/// One answer for an environment that doesn't exist and one they may not
/// use, so the answer reveals no environment's name.
fn not_usable(environment: &str) -> ApiError {
    ApiError::Validation(vec![FieldError::new(
        "environment",
        format!("you can't use an environment named {environment}"),
    )])
}

/// A key of theirs; its token is shown this once.
pub async fn create_key(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    ApiJson(body): ApiJson<NewKeyBody>,
) -> Result<(StatusCode, Json<CreatedKeyBody>), ApiError> {
    let Ok(environment) = EnvName::parse(&body.environment) else {
        return Err(not_usable(&body.environment));
    };
    let made = state
        .store()
        .create_key(&environment, caller.user.id, &body.name, body.expires_at)
        .await;
    let (key, token) = match made {
        Ok(made) => made,
        Err(StoreError::NotFound) => return Err(not_usable(&body.environment)),
        Err(StoreError::Validation(fields)) if fields.iter().any(|f| f.field == "environment") => {
            return Err(not_usable(&body.environment));
        }
        Err(e) => return Err(e.into()),
    };
    tracing::info!(user = %caller.user.id, key = %key.id, environment = %environment, "made a key");
    Ok((
        StatusCode::CREATED,
        Json(CreatedKeyBody::new(
            KeyBody::new(key, Some(caller.user.email)),
            token,
        )),
    ))
}

/// `suspend`, `resume` or `revoke` one of their keys; anyone else's is not
/// there for them.
pub async fn act(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path((id, action)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let id = EnvKeyId::parse(&id).ok_or(ApiError::NotFound)?;
    let store = state.store();
    match store.find_key(&id).await? {
        Some((key, _)) if key.owner == caller.user.id => {}
        _ => return Err(ApiError::NotFound),
    }
    match action.as_str() {
        "suspend" => store.set_suspended_by_owner(&id, true).await?,
        "resume" => store.set_suspended_by_owner(&id, false).await?,
        "revoke" => store.revoke_key(&id).await?,
        _ => return Err(ApiError::NotFound),
    }
    Ok(StatusCode::NO_CONTENT)
}
