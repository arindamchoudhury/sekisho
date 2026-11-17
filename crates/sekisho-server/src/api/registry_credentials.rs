//! Container registry credentials, for admins: the sync uses them to
//! resolve image tags to digests. Passwords are sealed and never answered.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::{ApiError, ApiJson};
use crate::app::AppState;

#[derive(Serialize)]
pub struct CredentialBody {
    host: String,
    username: String,
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<CredentialBody>>, ApiError> {
    let hosts = state.store().registry_hosts().await?;
    Ok(Json(
        hosts
            .into_iter()
            .map(|(host, username)| CredentialBody { host, username })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialRequest {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

/// Sets the credential for `host` (`host[:port]`), replacing any.
pub async fn put(
    State(state): State<AppState>,
    Path(host): Path<String>,
    ApiJson(body): ApiJson<CredentialRequest>,
) -> Result<Json<CredentialBody>, ApiError> {
    let host = state
        .store()
        .set_registry_credential(state.secret_box(), &host, &body.username, &body.password)
        .await?;
    tracing::info!(%host, "set a registry credential");
    Ok(Json(CredentialBody {
        host,
        username: body.username,
    }))
}

pub async fn delete(
    State(state): State<AppState>,
    Path(host): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.store().delete_registry_credential(&host).await?;
    tracing::info!(%host, "deleted a registry credential");
    Ok(StatusCode::NO_CONTENT)
}
