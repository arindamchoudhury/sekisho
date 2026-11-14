//! Environment keys, for admins: listing each environment's, and
//! suspending, resuming or revoking anyone's. Keys are created by their
//! owners (`/api/v1/me/keys`).

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use sekisho_core::keys::{EnvKeyId, KeyState};
use sekisho_core::names::EnvName;
use sekisho_store::EnvKey;
use serde::Serialize;
use time::OffsetDateTime;
use uuid::Uuid;

use super::ApiError;
use crate::app::AppState;

#[derive(Serialize)]
pub struct KeyBody {
    id: String,
    environment: String,
    /// The owner's user ID.
    owner: Uuid,
    owner_email: Option<String>,
    name: String,
    /// active, invalid, suspended, expired or revoked, now.
    state: &'static str,
    suspended: bool,
    /// An admin suspended it: only an admin resumes it.
    suspended_by_admin: bool,
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

impl CreatedKeyBody {
    pub fn new(key: KeyBody, token: String) -> Self {
        Self { key, token }
    }
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

impl KeyBody {
    pub fn new(k: EnvKey, owner_email: Option<String>) -> Self {
        Self {
            state: state_name(k.state(OffsetDateTime::now_utc())),
            id: k.id.to_string(),
            environment: k.environment.to_string(),
            owner: k.owner,
            owner_email,
            name: k.name,
            suspended: k.suspended,
            suspended_by_admin: k.suspended_by_admin,
            expires_at: k.expires_at,
            revoked_at: k.revoked_at,
            created_at: k.created_at,
        }
    }
}

pub async fn list(
    State(state): State<AppState>,
    Path(environment): Path<String>,
) -> Result<Json<Vec<KeyBody>>, ApiError> {
    let environment = EnvName::parse(&environment).map_err(|_| ApiError::NotFound)?;
    let keys = state.store().list_keys(&environment).await?;
    let emails = emails(&state).await?;
    Ok(Json(
        keys.into_iter()
            .map(|k| {
                let email = emails.get(&k.owner).cloned();
                KeyBody::new(k, email)
            })
            .collect(),
    ))
}

/// Every user's email, by ID.
pub(crate) async fn emails(state: &AppState) -> Result<HashMap<Uuid, String>, ApiError> {
    Ok(state
        .store()
        .list_users()
        .await?
        .into_iter()
        .map(|u| (u.id, u.email))
        .collect())
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
