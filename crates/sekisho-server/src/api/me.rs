//! A signed-in person's own routes: who they are, the environments they may
//! use, and their keys, which only they create; and the spend that counts
//! them, which a key may read too.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use sekisho_core::access::can_use;
use sekisho_core::keys::EnvKeyId;
use sekisho_core::names::EnvName;
use sekisho_store::{BudgetScope, FieldError, StoreError};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use super::keys::{CreatedKeyBody, KeyBody};
use super::{ApiError, ApiJson};
use crate::app::AppState;
use crate::auth::{Caller, Principal};

#[derive(Serialize)]
pub struct MeBody {
    id: Uuid,
    email: String,
    name: String,
    local: bool,
    /// By mark or by group.
    admin: bool,
    /// Every group they're in: their identity provider's and Sekisho's.
    groups: Vec<String>,
}

pub async fn me(Extension(caller): Extension<Caller>) -> Json<MeBody> {
    let groups = caller.user.access().groups.into_iter().collect();
    Json(MeBody {
        id: caller.user.id,
        email: caller.user.email,
        name: caller.user.name,
        local: caller.user.local,
        admin: caller.admin,
        groups,
    })
}

#[derive(Serialize)]
pub struct MyEnvironmentBody {
    name: String,
    description: String,
    notice: String,
    models: Vec<String>,
}

/// The environments they may use: one of their groups is among its access
/// groups. Admins get no others.
pub async fn environments(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Vec<MyEnvironmentBody>>, ApiError> {
    let access = caller.user.access();
    let environments = state.store().list_environments().await?;
    Ok(Json(
        environments
            .into_iter()
            .filter(|e| can_use(&access, &e.spec.access_groups))
            .map(|e| MyEnvironmentBody {
                name: e.spec.name.to_string(),
                description: e.spec.description,
                notice: e.spec.notice,
                models: e.spec.models.iter().map(ToString::to_string).collect(),
            })
            .collect(),
    ))
}

pub async fn keys(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
) -> Result<Json<Vec<KeyBody>>, ApiError> {
    let keys = state.store().list_user_keys(&caller.user.id).await?;
    Ok(Json(
        keys.into_iter()
            .map(|k| KeyBody::new(k, Some(caller.user.email.clone())))
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewKeyBody {
    environment: String,
    name: String,
    #[serde(default, with = "time::serde::rfc3339::option")]
    expires_at: Option<OffsetDateTime>,
}

/// One answer for an environment that doesn't exist and one they may not
/// use, so the answer reveals no environment's name.
fn not_usable(environment: &str) -> ApiError {
    ApiError::Validation(vec![FieldError::new(
        "environment",
        format!("you can't use an environment named {environment}"),
    )])
}

/// A key of theirs; its token is shown this once.
pub async fn create_key(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    ApiJson(body): ApiJson<NewKeyBody>,
) -> Result<(StatusCode, Json<CreatedKeyBody>), ApiError> {
    let Ok(environment) = EnvName::parse(&body.environment) else {
        return Err(not_usable(&body.environment));
    };
    let made = state
        .store()
        .create_key(&environment, caller.user.id, &body.name, body.expires_at)
        .await;
    let (key, token) = match made {
        Ok(made) => made,
        Err(StoreError::NotFound) => return Err(not_usable(&body.environment)),
        Err(StoreError::Validation(fields)) if fields.iter().any(|f| f.field == "environment") => {
            return Err(not_usable(&body.environment));
        }
        Err(e) => return Err(e.into()),
    };
    tracing::info!(user = %caller.user.id, key = %key.id, environment = %environment, "made a key");
    Ok((
        StatusCode::CREATED,
        Json(CreatedKeyBody::new(
            KeyBody::new(key, Some(caller.user.email)),
            token,
        )),
    ))
}

/// `suspend`, `resume` or `revoke` one of their keys; anyone else's is not
/// there for them.
pub async fn act(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path((id, action)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let id = EnvKeyId::parse(&id).ok_or(ApiError::NotFound)?;
    let store = state.store();
    match store.find_key(&id).await? {
        Some((key, _)) if key.owner == caller.user.id => {}
        _ => return Err(ApiError::NotFound),
    }
    match action.as_str() {
        "suspend" => store.set_suspended_by_owner(&id, true).await?,
        "resume" => store.set_suspended_by_owner(&id, false).await?,
        "revoke" => store.revoke_key(&id).await?,
        _ => return Err(ApiError::NotFound),
    }
    Ok(StatusCode::NO_CONTENT)
}

/// One budget that counts the caller, in its current window. `environment`
/// for an environment's budget or a person's within one; `key` for a key's.
#[derive(Serialize)]
pub struct SpendBody {
    scope: &'static str,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    environment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    limit_usd: String,
    spent_usd: String,
    #[serde(with = "time::serde::rfc3339")]
    resets_at: OffsetDateTime,
}

/// The budgets that count the caller. A person: their own, those of the
/// environments they may use and those of their keys. A key: its own and
/// its environment's.
pub async fn spend(
    State(state): State<AppState>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<Vec<SpendBody>>, ApiError> {
    let store = state.store();
    let budgets = match &principal {
        Principal::Person(caller) => {
            let access = caller.user.access();
            let environments: Vec<EnvName> = store
                .list_environments()
                .await?
                .into_iter()
                .filter(|e| can_use(&access, &e.spec.access_groups))
                .map(|e| e.spec.name)
                .collect();
            let keys: Vec<EnvKeyId> = store
                .list_user_keys(&caller.user.id)
                .await?
                .into_iter()
                .filter(|k| k.revoked_at.is_none())
                .map(|k| k.id)
                .collect();
            store
                .budgets_counting(Some(caller.user.id), &environments, &keys)
                .await?
        }
        Principal::Key {
            id, environment, ..
        } => {
            store
                .budgets_counting(
                    None,
                    std::slice::from_ref(environment),
                    std::slice::from_ref(id),
                )
                .await?
        }
    };
    let statuses = store.status(&budgets, OffsetDateTime::now_utc()).await?;
    Ok(Json(
        statuses
            .into_iter()
            .map(|s| {
                let (scope, environment, key) = match s.budget.scope {
                    BudgetScope::Person { environment, .. } => {
                        ("person", environment.map(|e| e.to_string()), None)
                    }
                    BudgetScope::Environment(e) => ("environment", Some(e.to_string()), None),
                    BudgetScope::Key(k) => ("key", None, Some(k.to_string())),
                };
                SpendBody {
                    scope,
                    name: s.budget.name,
                    environment,
                    key,
                    limit_usd: s.budget.limit.to_usd_string(),
                    spent_usd: s.spent.to_usd_string(),
                    resets_at: s.resets_at,
                }
            })
            .collect(),
    ))
}
