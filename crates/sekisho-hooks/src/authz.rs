//! The `extAuthz` hook (HTTP): agentgateway forwards each request's method,
//! path and `Authorization` header; a 2xx answer lets the request through and
//! any other answer goes back to the client as is.
//!
//! Checks, in order: a well-formed `Bearer` credential (else 401). For an
//! environment key: known, with the right secret (else 403 `unknown_key`),
//! for the calling route's environment (`x-sekisho-env`), active, and owned
//! by someone who may use the environment (else 403 with a reason). For a
//! session token: valid and unexpired, its session not ended and its user
//! enabled (else 401 with a reason), and its user may use the environment
//! (else 403 `no_access`). Then no used-up budget (else 429).

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use sekisho_core::access::can_use;
use sekisho_core::names::EnvName;
use sekisho_store::{BudgetStatus, Store, StoreError};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::cache::AuthCache;
use crate::credential::{Authenticated, Refusal, authenticate, refusal};
use crate::ratelimit::Payer;

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
    Allow {
        payer: Payer,
        env: EnvName,
    },
    /// With a reason for a session token.
    Unauthorized(Option<&'static str>),
    Forbidden(&'static str),
    BudgetExhausted(BudgetStatus),
    Unavailable,
}

async fn check(State(state): State<HooksState>, req: Request) -> Response {
    let decision = decide(&state, req.headers()).await;
    if !matches!(decision, Decision::Allow { .. }) {
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
    let now = (state.clock)();
    let route = route_environment(headers);
    let authenticated = authenticate(
        &state.cache,
        &state.store,
        single_authorization(headers),
        now,
    )
    .await;
    let (payer, env) = match authenticated {
        Authenticated::Refused(refused) => {
            return match refused {
                Refusal::Malformed => Decision::Unauthorized(None),
                Refusal::UnknownKey => Decision::Forbidden("unknown_key"),
                Refusal::InvalidToken | Refusal::TokenExpired | Refusal::SessionEnded => {
                    Decision::Unauthorized(Some(refused.reason()))
                }
                Refusal::Unavailable => Decision::Unavailable,
            };
        }
        Authenticated::Key { key, owner } => {
            let Some(env) = route
                .filter(|env| *env == key.environment)
                .and_then(|env| state.cache.environment(&env))
            else {
                return Decision::Forbidden("wrong_environment");
            };
            if let Some(reason) = refusal(&key, now) {
                return Decision::Forbidden(reason);
            }
            if !can_use(&owner.access, &env.access_groups) {
                return Decision::Forbidden("invalid");
            }
            let payer = Payer {
                person: owner.id,
                key: Some(key.id),
            };
            (payer, env.name)
        }
        Authenticated::Session { user, .. } => {
            let Some(env) = route.and_then(|env| state.cache.environment(&env)) else {
                return Decision::Forbidden("wrong_environment");
            };
            if !can_use(&user.access, &env.access_groups) {
                return Decision::Forbidden("no_access");
            }
            let payer = Payer {
                person: user.id,
                key: None,
            };
            (payer, env.name)
        }
    };
    match exhausted_budget(&state.store, &env, &payer, now).await {
        Ok(None) => Decision::Allow { payer, env },
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
    env: &EnvName,
    payer: &Payer,
    now: OffsetDateTime,
) -> Result<Option<BudgetStatus>, StoreError> {
    let budgets = store
        .applicable_budgets(env, payer.key.as_ref(), payer.person)
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
        "invalid" => "this key's owner may not use this environment",
        "no_access" => "you may not use this environment",
        _ => "unknown key",
    }
}

impl Decision {
    fn status(&self) -> StatusCode {
        match self {
            Decision::Allow { .. } => StatusCode::OK,
            Decision::Unauthorized(_) => StatusCode::UNAUTHORIZED,
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
            Decision::Allow { payer, env } => {
                let mut headers = HeaderMap::new();
                let mut set = |name: &'static str, value: &str| match HeaderValue::from_str(value) {
                    Ok(value) => {
                        headers.insert(HeaderName::from_static(name), value);
                    }
                    Err(_) => {
                        tracing::debug!(header = name, "value is not a valid header; left out")
                    }
                };
                // `none` for a session: the charge then names no key.
                set(
                    "x-sekisho-key",
                    payer.key.as_ref().map_or("none", |k| k.as_str()),
                );
                set("x-sekisho-person", &payer.person.to_string());
                set("x-sekisho-env", env.as_str());
                return (status, headers).into_response();
            }
            Decision::Unauthorized(None) => json!({
                "error": "unauthorized",
                "message": "send Authorization: Bearer <a sek_ environment key or a session token>",
            }),
            Decision::Unauthorized(Some(reason)) => json!({
                "error": "unauthorized",
                "reason": reason,
                "message": match reason {
                    "token_expired" => "the session token has expired; get a new one",
                    "session_ended" => "the session has ended; sign in again",
                    _ => "the session token is not valid; sign in again",
                },
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
                "message": "Sekisho couldn't check this request; try again",
            }),
        };
        (status, Json(body)).into_response()
    }
}
