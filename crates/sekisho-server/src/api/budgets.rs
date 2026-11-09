use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use sekisho_core::keys::EnvKeyId;
use sekisho_core::money::MicroUsd;
use sekisho_core::names::EnvName;
use sekisho_core::window::{BudgetWindow, WindowKind};
use sekisho_store::{Budget, BudgetScope, BudgetStatus, NewBudget};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{ApiError, ApiJson, Fields};
use crate::app::AppState;

/// `scope` is `person` (with `person`, and `environment` to limit it to one),
/// `environment` (with `environment`) or `key` (with `key`). `limit` is USD.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewBudgetBody {
    name: String,
    scope: String,
    #[serde(default)]
    person: Option<String>,
    #[serde(default)]
    environment: Option<String>,
    #[serde(default)]
    key: Option<String>,
    limit: String,
    window: String,
    #[serde(default = "calendar_by_default")]
    calendar: bool,
}

fn calendar_by_default() -> bool {
    true
}

#[derive(Serialize)]
pub struct BudgetBody {
    id: Uuid,
    name: String,
    scope: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    person: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    environment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    limit: String,
    window: &'static str,
    calendar: bool,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

impl From<Budget> for BudgetBody {
    fn from(b: Budget) -> Self {
        let (scope, person, environment, key) = match b.scope {
            BudgetScope::Person {
                person,
                environment,
            } => (
                "person",
                Some(person),
                environment.map(|e| e.to_string()),
                None,
            ),
            BudgetScope::Environment(env) => ("environment", None, Some(env.to_string()), None),
            BudgetScope::Key(id) => ("key", None, None, Some(id.to_string())),
        };
        Self {
            id: b.id,
            name: b.name,
            scope,
            person,
            environment,
            key,
            limit: b.limit.to_usd_string(),
            window: b.window.kind.as_str(),
            calendar: b.window.calendar,
            created_at: b.created_at,
        }
    }
}

#[derive(Serialize)]
pub struct StatusBody {
    budget: BudgetBody,
    /// Spend in the current window, USD.
    spent: String,
    #[serde(with = "time::serde::rfc3339")]
    resets_at: OffsetDateTime,
    exhausted: bool,
}

impl From<BudgetStatus> for StatusBody {
    fn from(s: BudgetStatus) -> Self {
        Self {
            exhausted: s.exhausted(),
            spent: s.spent.to_usd_string(),
            resets_at: s.resets_at,
            budget: s.budget.into(),
        }
    }
}

fn scope(fields: &mut Fields, body: &NewBudgetBody) -> Option<BudgetScope> {
    let not_for = |fields: &mut Fields, field: &str, value: &Option<String>| {
        if value.is_some() {
            fields.push(field, format!("not used by scope {}", body.scope));
        }
    };
    let environment =
        |fields: &mut Fields, value: &str| fields.check("environment", EnvName::parse(value));
    match body.scope.as_str() {
        "person" => {
            not_for(fields, "key", &body.key);
            let person = fields.required("person", body.person.clone());
            let env = match &body.environment {
                Some(e) => Some(environment(fields, e)?),
                None => None,
            };
            Some(BudgetScope::Person {
                person: person?,
                environment: env,
            })
        }
        "environment" => {
            not_for(fields, "person", &body.person);
            not_for(fields, "key", &body.key);
            let env = fields.required("environment", body.environment.clone())?;
            Some(BudgetScope::Environment(environment(fields, &env)?))
        }
        "key" => {
            not_for(fields, "person", &body.person);
            not_for(fields, "environment", &body.environment);
            let key = fields.required("key", body.key.clone())?;
            let id = fields.check(
                "key",
                EnvKeyId::parse(&key).ok_or("not an environment key id"),
            )?;
            Some(BudgetScope::Key(id))
        }
        _ => {
            fields.push("scope", "use person, environment or key");
            None
        }
    }
}

pub async fn create(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<NewBudgetBody>,
) -> Result<(StatusCode, Json<BudgetBody>), ApiError> {
    let mut fields = Fields::default();
    let limit = fields.check("limit", MicroUsd::from_usd_str(&body.limit));
    let kind = fields.check(
        "window",
        WindowKind::parse(&body.window).ok_or("use day, week or month"),
    );
    let scope = scope(&mut fields, &body);
    let (Some(limit), Some(kind), Some(scope)) = (limit, kind, scope) else {
        return Err(fields.fail());
    };
    fields.finish()?;
    let budget = state
        .store()
        .create_budget(NewBudget {
            name: body.name,
            scope,
            limit,
            window: BudgetWindow {
                kind,
                calendar: body.calendar,
            },
        })
        .await?;
    Ok((StatusCode::CREATED, Json(budget.into())))
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<BudgetBody>>, ApiError> {
    let budgets = state.store().list_budgets().await?;
    Ok(Json(budgets.into_iter().map(Into::into).collect()))
}

fn path_id(id: &str) -> Result<Uuid, ApiError> {
    id.parse().map_err(|_| ApiError::NotFound)
}

pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.store().delete_budget(path_id(&id)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<StatusBody>, ApiError> {
    let store = state.store();
    let budget = store.get_budget(path_id(&id)?).await?;
    let status = store
        .status(&[budget], OffsetDateTime::now_utc())
        .await?
        .pop()
        .ok_or(ApiError::NotFound)?;
    Ok(Json(status.into()))
}
