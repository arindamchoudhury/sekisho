//! `GET /internal/gateway/config?after=<version>`, long-polled by gateway agents.

use std::time::Duration;

use axum::Json;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tokio::time::Instant;

use super::ApiError;
use crate::app::AppState;

/// How often a waiting poll re-reads the database, in case another replica
/// saved a version this one has not seen.
const RECHECK: Duration = Duration::from_secs(5);

fn after(query: Option<&str>) -> Result<i64, ApiError> {
    let value = query
        .unwrap_or_default()
        .split('&')
        .find_map(|pair| pair.strip_prefix("after="));
    match value {
        None => Ok(0),
        Some(v) => v.parse().map_err(|_| {
            ApiError::BadRequest(format!("after must be a version number, not {v:?}"))
        }),
    }
}

/// `200 {version, yaml}` with the newest config once it is not `after`, the
/// version the agent has (it is older after the database was re-created);
/// `204` when none appears within the long-poll time.
pub async fn config(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
) -> Result<Response, ApiError> {
    let after = after(query.as_deref())?;
    let deadline = Instant::now() + state.long_poll();
    let mut published = state.published();
    loop {
        published.borrow_and_update();
        if let Some((version, yaml)) = state
            .store()
            .gateway_config_after(state.secret_box(), after)
            .await?
        {
            return Ok(Json(json!({ "version": version, "yaml": yaml })).into_response());
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        // Wakes on this replica's next publish, or re-checks after a while.
        let _ = tokio::time::timeout((deadline - now).min(RECHECK), published.changed()).await;
    }
}
