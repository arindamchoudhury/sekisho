//! Sekisho groups, for admins: they hold any user, and add to the groups
//! their identity provider gives them.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use sekisho_core::access::is_admin;
use sekisho_core::names::GroupName;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ApiError, ApiJson, Fields};
use crate::app::AppState;
use crate::auth::Caller;

#[derive(Serialize)]
pub struct GroupBody {
    name: String,
    members: Vec<Uuid>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewGroupBody {
    name: String,
}

fn group(name: &str) -> Result<GroupName, ApiError> {
    GroupName::parse(name).map_err(|_| ApiError::NotFound)
}

/// Whether leaving `group` would end the caller's admin: they're in it as a
/// Sekisho member, it is one of `SEKISHO_ADMIN_GROUPS`, and nothing else
/// (their mark, another admin group, their identity provider's groups) makes
/// them an admin. An identity provider's group is named by its connector, so
/// it is never the Sekisho group itself.
fn ends_own_admin(caller: &Caller, group: &GroupName, admin_groups: &[String]) -> bool {
    let user = &caller.user;
    if !caller.admin || !user.groups.contains(group) {
        return false;
    }
    let mut access = user.access();
    access.groups.remove(group.as_str());
    !is_admin(user.admin, &access, admin_groups)
}

fn keeps_own_admin(state: &AppState, caller: &Caller, group: &GroupName) -> Result<(), ApiError> {
    if ends_own_admin(caller, group, state.admin_groups()) {
        return Err(ApiError::Conflict(format!(
            "{group} is what makes you an admin; another admin can change it"
        )));
    }
    Ok(())
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<GroupBody>>, ApiError> {
    let groups = state.store().list_groups().await?;
    Ok(Json(
        groups
            .into_iter()
            .map(|(name, members)| GroupBody {
                name: name.to_string(),
                members,
            })
            .collect(),
    ))
}

pub async fn create(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<NewGroupBody>,
) -> Result<(StatusCode, Json<GroupBody>), ApiError> {
    let mut fields = Fields::default();
    let Some(name) = fields.check("name", GroupName::parse(&body.name)) else {
        return Err(fields.fail());
    };
    state.store().create_group(&name).await?;
    Ok((
        StatusCode::CREATED,
        Json(GroupBody {
            name: name.to_string(),
            members: Vec::new(),
        }),
    ))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let name = group(&name)?;
    keeps_own_admin(&state, &caller, &name)?;
    state.store().delete_group(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn add_member(
    State(state): State<AppState>,
    Path((name, user)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let name = group(&name)?;
    let user: Uuid = user.parse().map_err(|_| ApiError::NotFound)?;
    state.store().add_member(&name, &user).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn remove_member(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path((name, user)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let name = group(&name)?;
    let user: Uuid = user.parse().map_err(|_| ApiError::NotFound)?;
    if user == caller.user.id {
        keeps_own_admin(&state, &caller, &name)?;
    }
    state.store().remove_member(&name, &user).await?;
    Ok(StatusCode::NO_CONTENT)
}
