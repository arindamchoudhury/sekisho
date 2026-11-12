use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use sekisho_core::keys::{EnvKeyId, KeyState};
use sekisho_core::names::EnvName;
use sekisho_store::EnvKey;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use super::{ApiError, ApiJson};
use crate::app::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewKeyBody {
    owner: String,
    name: String,
    #[serde(default, with = "time::serde::rfc3339::option")]
    expires_at: Option<OffsetDateTime>,
}

#[derive(Serialize)]
pub struct KeyBody {
    id: String,
    environment: String,
    owner: String,
    name: String,
    /// active, suspended, expired or revoked, now.
    state: &'static str,
    suspended: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    expires_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    revoked_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

/// A new key with its token, which is never shown again.
#[derive(Serialize)]
pub struct CreatedKeyBody {
    #[serde(flatten)]
    key: KeyBody,
    token: String,
}

fn state_name(state: KeyState) -> &'static str {
    match state {
        KeyState::Active => "active",
        KeyState::Invalid => "invalid",
        KeyState::Suspended => "suspended",
        KeyState::Expired => "expired",
        KeyState::Revoked => "revoked",
    }
}

impl From<EnvKey> for KeyBody {
    fn from(k: EnvKey) -> Self {
        Self {
            state: state_name(k.state(OffsetDateTime::now_utc())),
            id: k.id.to_string(),
            environment: k.environment.to_string(),
            owner: k.owner,
            name: k.name,
            suspended: k.suspended,
            expires_at: k.expires_at,
            revoked_at: k.revoked_at,
            created_at: k.created_at,
        }
    }
}

pub async fn create(
    State(state): State<AppState>,
    Path(environment): Path<String>,
    ApiJson(body): ApiJson<NewKeyBody>,
) -> Result<(StatusCode, Json<CreatedKeyBody>), ApiError> {
    let environment = EnvName::parse(&environment).map_err(|_| ApiError::NotFound)?;
    let (key, token) = state
        .store()
        .create_key(&environment, &body.owner, &body.name, body.expires_at)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedKeyBody {
            key: key.into(),
            token,
        }),
    ))
}

pub async fn list(
    State(state): State<AppState>,
    Path(environment): Path<String>,
) -> Result<Json<Vec<KeyBody>>, ApiError> {
    let environment = EnvName::parse(&environment).map_err(|_| ApiError::NotFound)?;
    let keys = state.store().list_keys(&environment).await?;
    Ok(Json(keys.into_iter().map(Into::into).collect()))
}

/// `suspend`, `resume` or `revoke`.
pub async fn act(
    State(state): State<AppState>,
    Path((id, action)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let id = EnvKeyId::parse(&id).ok_or(ApiError::NotFound)?;
    let store = state.store();
    match action.as_str() {
        "suspend" => store.set_suspended(&id, true).await?,
        "resume" => store.set_suspended(&id, false).await?,
        "revoke" => store.revoke_key(&id).await?,
        _ => return Err(ApiError::NotFound),
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_state_has_its_api_name() {
        assert_eq!(state_name(KeyState::Active), "active");
        assert_eq!(state_name(KeyState::Suspended), "suspended");
        assert_eq!(state_name(KeyState::Expired), "expired");
        assert_eq!(state_name(KeyState::Revoked), "revoked");
        assert_eq!(state_name(KeyState::Invalid), "invalid");
    }
}
