//! The API (`/api/v1`) and the gateway agents' endpoint (`/internal`).
//!
//! `/api/v1/me` routes are for any signed-in person; every other `/api/v1`
//! route is for admins. JSON fields are snake_case and named like the
//! store's validation fields. Money is an exact USD decimal string; times
//! are RFC 3339.

mod budgets;
mod connectors;
mod environments;
mod gateway;
mod groups;
mod keys;
mod local_accounts;
mod me;
mod models;
mod providers;
mod signing_keys;
mod users;

use std::fmt::Display;

use axum::extract::{FromRequest, Request};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router, middleware};
use sekisho_store::{FieldError, StoreError};
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::app::AppState;
use crate::auth::{require_admin, require_gateway, require_user};

pub(crate) fn router(state: AppState) -> Router {
    let me = Router::new()
        .route("/api/v1/me", get(me::me))
        .route("/api/v1/me/environments", get(me::environments))
        .route("/api/v1/me/keys", get(me::keys).post(me::create_key))
        .route("/api/v1/me/keys/{id}/{action}", post(me::act))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_user));
    let admin = Router::new()
        .route(
            "/api/v1/providers",
            get(providers::list).post(providers::create),
        )
        .route(
            "/api/v1/providers/{name}",
            patch(providers::update).delete(providers::delete),
        )
        .route("/api/v1/provider-presets", get(providers::list_presets))
        .route("/api/v1/models", get(models::list).post(models::create))
        .route("/api/v1/models/{name}", delete(models::delete))
        .route("/api/v1/environments", get(environments::list))
        .route(
            "/api/v1/environments/{name}",
            get(environments::get)
                .put(environments::put)
                .delete(environments::delete),
        )
        // Keys are made by their owners, on /api/v1/me/keys.
        .route("/api/v1/environments/{name}/keys", get(keys::list))
        .route("/api/v1/keys/{id}/{action}", post(keys::act))
        .route("/api/v1/budgets", get(budgets::list).post(budgets::create))
        .route("/api/v1/budgets/{id}", delete(budgets::delete))
        .route("/api/v1/budgets/{id}/status", get(budgets::status))
        .route("/api/v1/users", get(users::list))
        .route("/api/v1/users/{id}", get(users::get).patch(users::patch))
        .route("/api/v1/users/{id}/offboard", post(users::offboard))
        .route("/api/v1/groups", get(groups::list).post(groups::create))
        .route("/api/v1/groups/{name}", delete(groups::delete))
        .route(
            "/api/v1/groups/{name}/members/{user}",
            put(groups::add_member).delete(groups::remove_member),
        )
        .route("/api/v1/local-accounts", post(local_accounts::create))
        .route(
            "/api/v1/local-accounts/{id}",
            delete(local_accounts::delete),
        )
        .route(
            "/api/v1/local-accounts/{id}/reset",
            post(local_accounts::reset),
        )
        .route("/api/v1/connectors", get(connectors::list))
        .route(
            "/api/v1/connectors/{id}",
            put(connectors::put).delete(connectors::delete),
        )
        .route("/api/v1/signing-keys/rotate", post(signing_keys::rotate))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin));
    let internal = Router::new()
        .route("/internal/gateway/config", get(gateway::config))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_gateway,
        ));
    me.merge(admin).merge(internal).with_state(state)
}

/// Every error answer: `{"error": <code>, "message": ..., "fields"?: [...]}`.
#[derive(Debug)]
pub enum ApiError {
    /// No usable credential; the message says what to send.
    Unauthorized(String),
    /// Signed in, but not an admin.
    Forbidden,
    NotFound,
    BadRequest(String),
    Validation(Vec<FieldError>),
    Conflict(String),
    InUse(String),
    /// Something Sekisho depends on, such as Dex, can't be reached.
    Unavailable(String),
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
            ApiError::Unauthorized(m) => (StatusCode::UNAUTHORIZED, "unauthorized", m.clone()),
            ApiError::Forbidden => (
                StatusCode::FORBIDDEN,
                "forbidden",
                "only admins may do this".to_owned(),
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
            ApiError::Unavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, "unavailable", m.clone()),
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
        let mut response = (status, Json(body)).into_response();
        if status == StatusCode::UNAUTHORIZED {
            // RFC 6750 section 3.
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"sekisho\""),
            );
        }
        response
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
