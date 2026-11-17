//! People, for admins: listing, admin marks, disabling, and offboarding.

use axum::extract::{Path, State};
use axum::{Extension, Json};
use sekisho_core::access::is_admin;
use sekisho_store::User;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::time::timeout;
use uuid::Uuid;

use super::{ApiError, ApiJson};
use crate::app::AppState;
use crate::auth::Caller;

#[derive(Serialize)]
pub struct UserBody {
    id: Uuid,
    email: String,
    name: String,
    local: bool,
    /// The admin mark, which `PATCH` sets.
    admin: bool,
    /// An admin through `SEKISHO_ADMIN_GROUPS`, mark or not.
    admin_by_group: bool,
    /// The Sekisho groups they're in.
    groups: Vec<String>,
    /// Their identity provider's groups at the latest sign-in.
    idp_groups: Vec<String>,
    /// The connector of their latest sign-in.
    connector: Option<String>,
    disabled: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    disabled_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    idp_refused_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    last_sign_in_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

impl UserBody {
    pub fn new(user: User, admin_groups: &[String]) -> Self {
        let mut access = user.access();
        // Whether the groups alone make them an admin.
        access.disabled = false;
        Self {
            admin_by_group: is_admin(false, &access, admin_groups),
            id: user.id,
            email: user.email,
            name: user.name,
            local: user.local,
            admin: user.admin,
            groups: user.groups.iter().map(ToString::to_string).collect(),
            idp_groups: user.idp_groups,
            connector: user.dex_connector,
            disabled: user.disabled_at.is_some(),
            disabled_at: user.disabled_at,
            idp_refused_at: user.idp_refused_at,
            last_sign_in_at: user.last_sign_in_at,
            created_at: user.created_at,
        }
    }
}

fn user_id(id: &str) -> Result<Uuid, ApiError> {
    id.parse().map_err(|_| ApiError::NotFound)
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<UserBody>>, ApiError> {
    let users = state.store().list_users().await?;
    Ok(Json(
        users
            .into_iter()
            .map(|u| UserBody::new(u, state.admin_groups()))
            .collect(),
    ))
}

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<UserBody>, ApiError> {
    let user = state.store().user(&user_id(&id)?).await?;
    Ok(Json(UserBody::new(user, state.admin_groups())))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserPatch {
    #[serde(default)]
    admin: Option<bool>,
    #[serde(default)]
    disabled: Option<bool>,
}

/// Marks or un-marks an admin, disables (ending their sessions) or enables.
/// An admin can't un-mark or disable themselves, so whoever acts stays an
/// admin and there is always one left.
pub async fn patch(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<UserPatch>,
) -> Result<Json<UserBody>, ApiError> {
    let id = user_id(&id)?;
    if id == caller.user.id && (body.admin == Some(false) || body.disabled == Some(true)) {
        return Err(ApiError::Conflict(
            "you can't un-mark or disable yourself; another admin can".into(),
        ));
    }
    let store = state.store();
    store.user(&id).await?;
    if let Some(admin) = body.admin {
        store.set_admin(&id, admin).await?;
    }
    if let Some(disabled) = body.disabled {
        store
            .set_disabled(&id, disabled, OffsetDateTime::now_utc())
            .await?;
    }
    tracing::info!(user = %id, by = %caller.user.id, admin = ?body.admin, disabled = ?body.disabled, "changed a user");
    Ok(Json(UserBody::new(
        store.user(&id).await?,
        state.admin_groups(),
    )))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OffboardBody {
    /// Whoever gets what they own.
    #[serde(default)]
    new_owner: Option<Uuid>,
}

#[derive(Serialize)]
pub struct OffboardedBody {
    ended_sessions: u64,
    revoked_keys: Vec<String>,
    handed_over: Vec<String>,
    /// The catalog entries handed over, as `<kind>/<source>/<name>` with
    /// the kind's path segment, such as `skills/team/pdf`.
    handed_over_entries: Vec<String>,
    /// Whether Dex holds no sign-in of theirs any more. `false` when Dex
    /// couldn't be reached: Sekisho's part is done, and their refresh token
    /// at Dex dies at their next sign-in or by Dex's expiry.
    dex_refresh_revoked: bool,
}

/// Offboards a person in one step: disables them, ends their sessions,
/// revokes their keys and hands what they own (environments and catalog
/// entries) to `new_owner`, all or
/// nothing; then revokes their sign-in at Dex.
pub async fn offboard(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<OffboardBody>,
) -> Result<Json<OffboardedBody>, ApiError> {
    let id = user_id(&id)?;
    if id == caller.user.id {
        return Err(ApiError::Conflict(
            "you can't offboard yourself; another admin can".into(),
        ));
    }
    let done = state
        .store()
        .offboard(&id, body.new_owner, OffsetDateTime::now_utc())
        .await?;
    // After the commit, on a task the caller can't cancel: Sekisho's part
    // stands whatever Dex does.
    let dex_refresh_revoked = match done.dex_subject.clone() {
        None => true,
        Some(subject) => {
            let state = state.clone();
            tokio::spawn(async move {
                match timeout(
                    sekisho_auth::dex::DEX_TIMEOUT,
                    state.dex().revoke_refresh(&subject),
                )
                .await
                {
                    Ok(Ok(())) => true,
                    Ok(Err(error)) => {
                        tracing::warn!(user = %id, %error, "offboarded, but Dex didn't revoke their sign-in");
                        false
                    }
                    Err(_) => {
                        tracing::warn!(user = %id, "offboarded, but Dex didn't answer the revocation");
                        false
                    }
                }
            })
            .await
            .unwrap_or(false)
        }
    };
    tracing::info!(user = %id, by = %caller.user.id, sessions = done.ended_sessions, keys = done.revoked_keys.len(), dex_refresh_revoked, "offboarded a person");
    Ok(Json(OffboardedBody {
        ended_sessions: done.ended_sessions,
        revoked_keys: done.revoked_keys.iter().map(ToString::to_string).collect(),
        handed_over: done.handed_over.iter().map(ToString::to_string).collect(),
        handed_over_entries: done
            .handed_over_entries
            .iter()
            .map(|k| format!("{}/{}/{}", k.kind.path_segment(), k.source, k.name))
            .collect(),
        dex_refresh_revoked,
    }))
}
