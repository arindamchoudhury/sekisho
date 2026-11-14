//! Local accounts, for admins: invitations, password resets and deletion.
//! The setup links in the answers are shown to the admin once and never
//! logged.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use sekisho_auth::local::{self, LocalError, SetupLink};
use sekisho_store::StoreError;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use super::users::UserBody;
use super::{ApiError, ApiJson};
use crate::app::AppState;
use crate::auth::Caller;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewLocalAccountBody {
    email: String,
    #[serde(default)]
    name: String,
}

#[derive(Serialize)]
pub struct LinkBody {
    setup_url: String,
    #[serde(with = "time::serde::rfc3339")]
    expires_at: OffsetDateTime,
}

impl From<SetupLink> for LinkBody {
    fn from(link: SetupLink) -> Self {
        Self {
            setup_url: link.url.to_string(),
            expires_at: link.expires_at,
        }
    }
}

#[derive(Serialize)]
pub struct InvitedBody {
    user: UserBody,
    #[serde(flatten)]
    link: LinkBody,
}

impl From<LocalError> for ApiError {
    fn from(e: LocalError) -> Self {
        match e {
            LocalError::NotLocal | LocalError::Store(StoreError::NotFound) => ApiError::NotFound,
            LocalError::Store(e) => e.into(),
            LocalError::Dex(error) => {
                tracing::warn!(%error, "Dex couldn't change a local account");
                ApiError::Unavailable(
                    "the sign-in service can't be reached; nothing was changed, try again".into(),
                )
            }
            LocalError::Interrupted => ApiError::Internal,
        }
    }
}

fn account(id: &str) -> Result<Uuid, ApiError> {
    id.parse().map_err(|_| ApiError::NotFound)
}

/// Makes a local account and the link where its person sets their password.
pub async fn create(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<NewLocalAccountBody>,
) -> Result<(StatusCode, Json<InvitedBody>), ApiError> {
    let (user, link) = local::invite(state.auth(), &body.email, &body.name).await?;
    Ok((
        StatusCode::CREATED,
        Json(InvitedBody {
            user: UserBody::new(user, state.admin_groups()),
            link: link.into(),
        }),
    ))
}

/// Removes the password at once, ends the person's sessions, and makes a
/// new link.
pub async fn reset(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<LinkBody>, ApiError> {
    let link = local::reset(state.auth(), &account(&id)?).await?;
    Ok(Json(link.into()))
}

/// Deletes the password at Dex and disables the user.
pub async fn delete(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = account(&id)?;
    if id == caller.user.id {
        return Err(ApiError::Conflict(
            "you can't delete your own account; another admin can".into(),
        ));
    }
    local::delete(state.auth(), &id).await?;
    Ok(StatusCode::NO_CONTENT)
}
