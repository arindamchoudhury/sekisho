//! The `extAuthz` hook (HTTP): agentgateway forwards each request's method,
//! path and `Authorization` header; a 2xx answer lets the request through and
//! any other answer goes back to the client as is.
//!
//! Checks, in order: a well-formed `Bearer sek_…` token (else 401); a known
//! key with the right secret, for the calling route's environment
//! (`x-sekisho-env`), that is active (else 403 with a reason); no used-up
//! budget (else 429).

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use sekisho_core::names::EnvName;
use sekisho_store::{BudgetStatus, Store, StoreError};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::cache::{AuthCache, CachedKey};
use crate::credential::{Credential, refusal, resolve};

/// The header each route adds to the hook request, naming its environment.
pub const ROUTE_ENV_HEADER: &str = "x-sekisho-env";

/// What every hook needs.
#[derive(Clone)]
pub struct HooksState {
    pub cache: Arc<AuthCache>,
    pub store: Store,
    /// The current time; tests fix it.
    pub clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
}

/// Answers any method on any path.
pub fn router(state: HooksState) -> Router {
    Router::new().fallback(check).with_state(state)
}

enum Decision {
    Allow(CachedKey),
    Unauthorized,
    Forbidden(&'static str),
    BudgetExhausted(BudgetStatus),
    Unavailable,
}

async fn check(State(state): State<HooksState>, req: Request) -> Response {
    let decision = decide(&state, req.headers()).await;
    if !matches!(decision, Decision::Allow(_)) {
        tracing::debug!(path = req.uri().path(), status = ?decision.status(), "request refused");
    }
    decision.into_response()
}

/// The value of exactly one `Authorization` header.
fn single_authorization(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    value.to_str().ok()
}

/// The environment of the route that called the hook. Each route adds
/// `x-sekisho-env` to the hook request (`addRequestHeaders`), so it comes
/// from the config, not the client. The path is not used: agentgateway may
/// match routes on a normalised path but forward the original one.
fn route_environment(headers: &HeaderMap) -> Option<EnvName> {
    let mut values = headers.get_all(ROUTE_ENV_HEADER).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    EnvName::parse(value.to_str().ok()?).ok()
}

async fn decide(state: &HooksState, headers: &HeaderMap) -> Decision {
    let key = match resolve(&state.cache, single_authorization(headers)) {
        Credential::Malformed => return Decision::Unauthorized,
        Credential::Unknown => return Decision::Forbidden("unknown_key"),
        Credential::Known(key) => key,
    };
    match route_environment(headers) {
        Some(env) if env == key.environment && state.cache.environment_exists(&env) => {}
        _ => return Decision::Forbidden("wrong_environment"),
    }
    let now = (state.clock)();
    if let Some(reason) = refusal(&key, now) {
        return Decision::Forbidden(reason);
    }
    match exhausted_budget(&state.store, &key, now).await {
        Ok(None) => Decision::Allow(key),
        Ok(Some(status)) => Decision::BudgetExhausted(status),
        Err(error) => {
            tracing::warn!(%error, "budget check failed; refusing the request");
            Decision::Unavailable
        }
    }
}

/// The used-up budget among those the request counts against, if any: the
/// one that resets last, since the request stays refused until then.
///
/// Reads PostgreSQL on every request (two indexed queries). Plan 6 can cache
/// this if the latency target needs it.
pub async fn exhausted_budget(
    store: &Store,
    key: &CachedKey,
    now: OffsetDateTime,
) -> Result<Option<BudgetStatus>, StoreError> {
    let budgets = store
        .applicable_budgets(&key.environment, Some(&key.id), key.owner)
        .await?;
    if budgets.is_empty() {
        return Ok(None);
    }
    Ok(store
        .status(&budgets, now)
        .await?
        .into_iter()
        .filter(BudgetStatus::exhausted)
        .max_by(|a, b| {
            a.resets_at
                .cmp(&b.resets_at)
                .then_with(|| b.budget.name.cmp(&a.budget.name))
        }))
}

fn forbidden_message(reason: &str) -> &'static str {
    match reason {
        "revoked" => "this key was revoked",
        "expired" => "this key has expired",
        "suspended" => "this key is suspended",
        "wrong_environment" => "this key is not for this environment",
        _ => "unknown key",
    }
}

impl Decision {
    fn status(&self) -> StatusCode {
        match self {
            Decision::Allow(_) => StatusCode::OK,
            Decision::Unauthorized => StatusCode::UNAUTHORIZED,
            Decision::Forbidden(_) => StatusCode::FORBIDDEN,
            Decision::BudgetExhausted(_) => StatusCode::TOO_MANY_REQUESTS,
            Decision::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

impl IntoResponse for Decision {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = match self {
            Decision::Allow(key) => {
                let mut headers = HeaderMap::new();
                let mut set = |name: &'static str, value: &str| match HeaderValue::from_str(value) {
                    Ok(value) => {
                        headers.insert(HeaderName::from_static(name), value);
                    }
                    Err(_) => {
                        tracing::debug!(header = name, "value is not a valid header; left out")
                    }
                };
                set("x-sekisho-key", key.id.as_str());
                set("x-sekisho-person", &key.owner.to_string());
                set("x-sekisho-env", key.environment.as_str());
                return (status, headers).into_response();
            }
            Decision::Unauthorized => json!({
                "error": "unauthorized",
                "message": "send Authorization: Bearer <a sek_ environment key>",
            }),
            Decision::Forbidden(reason) => json!({
                "error": "forbidden",
                "reason": reason,
                "message": forbidden_message(reason),
            }),
            Decision::BudgetExhausted(s) => {
                let resets_at = s
                    .resets_at
                    .format(&Rfc3339)
                    .expect("a UTC time always formats as RFC 3339");
                json!({
                    "error": "budget_exhausted",
                    "budget": s.budget.name,
                    "resets_at": resets_at,
                    "message": format!("Budget '{}' is used up until {resets_at}.", s.budget.name),
                })
            }
            Decision::Unavailable => json!({
                "error": "unavailable",
                "message": "the budget check failed; try again",
            }),
        };
        (status, Json(body)).into_response()
    }
}
