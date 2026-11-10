//! The admin API (`/api/v1`) and the gateway agents' endpoint (`/internal`).
//!
//! JSON fields are snake_case and named like the store's validation fields.
//! Money is an exact USD decimal string; times are RFC 3339.

mod budgets;
mod environments;
mod gateway;
mod keys;
mod models;
mod providers;

use std::fmt::Display;

use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router, middleware};
use sekisho_store::{FieldError, StoreError};
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::admin_auth::{require_bootstrap, require_gateway};
use crate::app::AppState;

pub(crate) fn router(state: AppState) -> Router {
    let admin = Router::new()
        .route(
            "/api/v1/providers",
            get(providers::list).post(providers::create),
        )
        .route(
            "/api/v1/providers/{name}",
            patch(providers::update).delete(providers::delete),
        )
        .route("/api/v1/models", get(models::list).post(models::create))
        .route("/api/v1/models/{name}", delete(models::delete))
        .route("/api/v1/environments", get(environments::list))
        .route(
            "/api/v1/environments/{name}",
            get(environments::get)
                .put(environments::put)
                .delete(environments::delete),
        )
        .route(
            "/api/v1/environments/{name}/keys",
            get(keys::list).post(keys::create),
        )
        .route("/api/v1/keys/{id}/{action}", post(keys::act))
        .route("/api/v1/budgets", get(budgets::list).post(budgets::create))
        .route("/api/v1/budgets/{id}", delete(budgets::delete))
        .route("/api/v1/budgets/{id}/status", get(budgets::status))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_bootstrap,
        ));
    let internal = Router::new()
        .route("/internal/gateway/config", get(gateway::config))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_gateway,
        ));
    admin.merge(internal).with_state(state)
}

/// Every error answer: `{"error": <code>, "message": ..., "fields"?: [...]}`.
#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    NotFound,
    BadRequest(String),
    Validation(Vec<FieldError>),
    Conflict(String),
    InUse(String),
    Internal,
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound => ApiError::NotFound,
            StoreError::Conflict(m) => ApiError::Conflict(m),
            StoreError::InUse(m) => ApiError::InUse(m),
            StoreError::Validation(f) => ApiError::Validation(f),
            // Details go to the log, not to the client.
            StoreError::Db(_) | StoreError::Secret(_) => {
                tracing::error!(error = %e, "request failed");
                ApiError::Internal
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "missing or wrong bearer token".to_owned(),
            ),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not_found", "not found".to_owned()),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, "bad_request", m.clone()),
            ApiError::Validation(_) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation",
                "some fields are invalid".to_owned(),
            ),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, "conflict", m.clone()),
            ApiError::InUse(m) => (
                StatusCode::CONFLICT,
                "in_use",
                format!("still in use by {m}"),
            ),
            ApiError::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "internal error; see the server log".to_owned(),
            ),
        };
        let mut body = json!({ "error": code, "message": message });
        if let ApiError::Validation(fields) = &self {
            body["fields"] = fields
                .iter()
                .map(|f| json!({ "field": f.field, "message": f.message }))
                .collect();
        }
        (status, Json(body)).into_response()
    }
}

/// `Json<T>` whose rejections are JSON `bad_request` answers.
pub(crate) struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        Json::<T>::from_request(req, state)
            .await
            .map(|Json(value)| ApiJson(value))
            .map_err(|e| ApiError::BadRequest(e.body_text()))
    }
}

/// Collects field errors while converting a request body.
#[derive(Default)]
pub(crate) struct Fields(Vec<FieldError>);

impl Fields {
    pub(crate) fn push(&mut self, field: impl Into<String>, message: impl Into<String>) {
        self.0.push(FieldError::new(field, message));
    }

    /// The value, or `None` after recording why it is invalid.
    pub(crate) fn check<T, E: Display>(&mut self, field: &str, result: Result<T, E>) -> Option<T> {
        result.map_err(|e| self.push(field, e.to_string())).ok()
    }

    /// A value that must be present.
    pub(crate) fn required(&mut self, field: &str, value: Option<String>) -> Option<String> {
        if value.as_deref().is_none_or(|v| v.trim().is_empty()) {
            self.push(field, "required");
            return None;
        }
        value
    }

    /// `Validation` with everything collected (call when a value is missing).
    pub(crate) fn fail(self) -> ApiError {
        ApiError::Validation(self.0)
    }

    pub(crate) fn finish(self) -> Result<(), ApiError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(self.fail())
        }
    }
}
