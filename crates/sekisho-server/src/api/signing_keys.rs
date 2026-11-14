//! The signing keys of session tokens, for admins.

use axum::Extension;
use axum::extract::State;
use axum::http::StatusCode;
use time::OffsetDateTime;

use super::ApiError;
use crate::app::AppState;
use crate::auth::Caller;

/// Rotates now: the new key is published at once and signs a minute later,
/// so every replica knows it first; the old key's tokens keep working until
/// they expire.
pub async fn rotate(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> Result<StatusCode, ApiError> {
    state
        .store()
        .rotate_signing_key(state.secret_box(), OffsetDateTime::now_utc(), true)
        .await?;
    tracing::info!(by = %caller.user.id, "rotated the signing key");
    Ok(StatusCode::NO_CONTENT)
}
