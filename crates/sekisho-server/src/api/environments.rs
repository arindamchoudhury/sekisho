//! Environments, for admins: read and saved as the API's JSON or as the
//! environment format's YAML (`sekisho/v1alpha1`, design 9.2), with what
//! each lists from the catalog and delivers; owners change with `PATCH`.

use std::collections::{BTreeMap, HashMap};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use sekisho_catalog::environment::{self, Problem};
use sekisho_catalog::names::EntryKind;
use sekisho_core::names::EnvName;
use sekisho_store::{Delivered, Environment, EnvironmentSpec, FieldError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use super::keys::emails;
use super::{ApiError, ApiJson, Fields};
use crate::app::AppState;
use crate::auth::Caller;

/// The YAML media type, for `Content-Type` and `Accept`.
const YAML: &str = "application/yaml";

fn is_yaml(value: Option<&HeaderValue>) -> bool {
    value.and_then(|v| v.to_str().ok()).is_some_and(|v| {
        v.split(',').any(|part| {
            let media = part.split(';').next().unwrap_or("").trim();
            media.eq_ignore_ascii_case(YAML)
                || media.eq_ignore_ascii_case("application/x-yaml")
                || media.eq_ignore_ascii_case("text/yaml")
        })
    })
}

#[derive(Serialize)]
pub struct DefaultsBody {
    anthropic: Option<String>,
    openai: Option<String>,
    gemini: Option<String>,
}

#[derive(Serialize)]
pub struct EffectiveDefaultsBody {
    anthropic: Option<String>,
    openai: Option<String>,
    gemini: Option<String>,
    /// Only as named: without one, tools use their default model.
    small: Option<String>,
}

/// One reference the environment lists, and the entry it is bound to.
#[derive(Serialize)]
pub struct CatalogStatusBody {
    kind: EntryKind,
    reference: String,
    source: String,
    name: String,
    quarantined: bool,
    removed: bool,
}

/// One entry the environment delivers.
#[derive(Serialize)]
pub struct DeliveredBody {
    kind: EntryKind,
    source: String,
    name: String,
    /// An MCP server's allowed tools; `null` allows them all.
    tools: Option<Vec<String>>,
    /// The environment lists it itself.
    listed: bool,
    /// The toolkits that bring it, as `<source>/<name>`.
    via: Vec<String>,
    quarantined: bool,
    removed: bool,
}

impl From<Delivered> for DeliveredBody {
    fn from(d: Delivered) -> Self {
        Self {
            kind: d.key.kind,
            source: d.key.source.to_string(),
            name: d.key.name.to_string(),
            tools: d.tools,
            listed: d.listed,
            via: d
                .via
                .iter()
                .map(|k| format!("{}/{}", k.source, k.name))
                .collect(),
            quarantined: d.quarantined,
            removed: d.removed,
        }
    }
}

/// The JSON answer: the format's fields, then what Sekisho adds. Every
/// field may be sent back in a `PUT`.
#[derive(Serialize)]
pub struct EnvironmentBody {
    api_version: &'static str,
    kind: &'static str,
    id: Uuid,
    name: String,
    description: String,
    notice: String,
    access_groups: Vec<String>,
    models: Vec<String>,
    default_models: DefaultsBody,
    small_model: Option<String>,
    labels: BTreeMap<String, String>,
    /// A server's name, or `{server, tools}` when it allows only some tools.
    mcp_servers: Vec<Value>,
    a2a_agents: Vec<String>,
    skills: Vec<String>,
    plugins: Vec<String>,
    prompts: Vec<String>,
    toolkits: Vec<String>,
    /// `null` only for an environment saved before owners existed.
    owner: Option<Uuid>,
    owner_email: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    updated_at: OffsetDateTime,
    effective_default_models: EffectiveDefaultsBody,
    catalog_status: Vec<CatalogStatusBody>,
    delivered: Vec<DeliveredBody>,
}

impl EnvironmentBody {
    fn new(e: Environment, owner_email: Option<String>, delivered: Vec<Delivered>) -> Self {
        let text = |list: &[sekisho_catalog::names::EntryRef]| {
            list.iter().map(ToString::to_string).collect()
        };
        let name = |m: &Option<sekisho_core::names::ModelName>| m.as_ref().map(ToString::to_string);
        let spec = &e.spec;
        Self {
            api_version: environment::API_VERSION,
            kind: environment::KIND,
            id: e.id,
            name: spec.name.to_string(),
            description: spec.description.clone(),
            notice: spec.notice.clone(),
            access_groups: spec.access_groups.clone(),
            models: spec.models.iter().map(ToString::to_string).collect(),
            default_models: DefaultsBody {
                anthropic: name(&spec.default_models.anthropic),
                openai: name(&spec.default_models.openai),
                gemini: name(&spec.default_models.gemini),
            },
            small_model: name(&spec.small_model),
            labels: spec.labels.as_map().clone(),
            mcp_servers: spec
                .mcp_servers
                .iter()
                .map(|u| match &u.tools {
                    None => json!(u.server.to_string()),
                    Some(tools) => json!({"server": u.server.to_string(), "tools": tools}),
                })
                .collect(),
            a2a_agents: text(&spec.a2a_agents),
            skills: text(&spec.skills),
            plugins: text(&spec.plugins),
            prompts: text(&spec.prompts),
            toolkits: text(&spec.toolkits),
            owner: e.owner,
            owner_email,
            created_at: e.created_at,
            updated_at: e.updated_at,
            effective_default_models: EffectiveDefaultsBody {
                anthropic: name(&e.effective_defaults.anthropic),
                openai: name(&e.effective_defaults.openai),
                gemini: name(&e.effective_defaults.gemini),
                small: name(&e.effective_defaults.small),
            },
            catalog_status: e
                .entries
                .iter()
                .map(|l| CatalogStatusBody {
                    kind: l.kind,
                    reference: l.written.clone(),
                    source: l.key.source.to_string(),
                    name: l.key.name.to_string(),
                    quarantined: l.quarantined,
                    removed: l.removed,
                })
                .collect(),
            delivered: delivered.into_iter().map(Into::into).collect(),
        }
    }
}

/// The answer for one environment, as JSON or, when asked for, as YAML.
async fn answer(state: &AppState, e: Environment, yaml: bool) -> Result<Response, ApiError> {
    if yaml {
        let text = e.spec.to_doc().to_yaml();
        return Ok(([(header::CONTENT_TYPE, YAML)], text).into_response());
    }
    let email = match e.owner {
        Some(owner) => Some(state.store().user(&owner).await?.email),
        None => None,
    };
    let delivered = state.store().environment_catalog(e.id).await?;
    Ok(Json(EnvironmentBody::new(e, email, delivered)).into_response())
}

fn path_name(name: &str) -> Result<EnvName, ApiError> {
    EnvName::parse(name).map_err(|_| ApiError::NotFound)
}

fn invalid(problems: Vec<Problem>) -> ApiError {
    ApiError::Validation(
        problems
            .into_iter()
            .map(|p| FieldError::new(p.field, p.message))
            .collect(),
    )
}

/// Creates the environment or replaces its spec. The body is the API's
/// JSON, or the format's YAML with `Content-Type: application/yaml`. JSON
/// may name the `owner` (a user ID); without one the stored owner stays,
/// or the admin who creates it owns it. YAML never holds an owner: change
/// it with `PATCH`.
pub async fn put(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let yaml = is_yaml(headers.get(header::CONTENT_TYPE));
    let (doc, owner) = if yaml {
        let text = std::str::from_utf8(&body)
            .map_err(|_| ApiError::BadRequest("the body isn't UTF-8 text".into()))?;
        (environment::parse_yaml(text).map_err(invalid)?, None)
    } else {
        let mut value: Value = serde_json::from_slice(&body)
            .map_err(|e| ApiError::BadRequest(format!("the body isn't JSON: {e}")))?;
        let Some(map) = value.as_object_mut() else {
            return Err(ApiError::BadRequest(
                "the body must be a JSON object".into(),
            ));
        };
        let owner = match map.get("owner") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(
                s.parse::<Uuid>()
                    .map_err(|_| ApiError::BadRequest("owner must be a user ID".into()))?,
            ),
            Some(_) => return Err(ApiError::BadRequest("owner must be a user ID".into())),
        };
        // The name comes from the path when the body leaves it out.
        map.entry("name").or_insert_with(|| json!(path));
        (
            environment::from_json_value(value, false).map_err(invalid)?,
            owner,
        )
    };
    let mut fields = Fields::default();
    if doc.name.as_str() != path {
        fields.push("name", "must match the name in the path");
    }
    fields.finish()?;
    let environment = state
        .store()
        .upsert_environment(EnvironmentSpec::from_doc(doc, owner), caller.user.id)
        .await?;
    answer(&state, environment, is_yaml(headers.get(header::ACCEPT))).await
}

/// One environment, as JSON or (with `Accept: application/yaml`) YAML.
pub async fn get(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let environment = state.store().get_environment(&path_name(&name)?).await?;
    answer(&state, environment, is_yaml(headers.get(header::ACCEPT))).await
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<EnvironmentBody>>, ApiError> {
    let store = state.store();
    let environments = store.list_environments().await?;
    let emails = emails(&state).await?;
    let ids: Vec<Uuid> = environments.iter().map(|e| e.id).collect();
    let mut delivered: HashMap<Uuid, Vec<Delivered>> = store.environment_catalogs(&ids).await?;
    Ok(Json(
        environments
            .into_iter()
            .map(|e| {
                let email = e.owner.and_then(|o| emails.get(&o).cloned());
                let d = delivered.remove(&e.id).unwrap_or_default();
                EnvironmentBody::new(e, email, d)
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerPatch {
    #[serde(default)]
    owner: Option<Uuid>,
}

/// Hands the environment to another owner, an enabled user. Nothing else
/// changes here: the rest is the environment's spec.
pub async fn patch(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<OwnerPatch>,
) -> Result<Response, ApiError> {
    let name = path_name(&name)?;
    let Some(owner) = body.owner else {
        let mut fields = Fields::default();
        fields.push("owner", "required");
        return Err(fields.fail());
    };
    let environment = state.store().set_environment_owner(&name, owner).await?;
    tracing::info!(environment = %name, %owner, by = %caller.user.id, "changed an environment's owner");
    answer(&state, environment, false).await
}

pub async fn delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    state.store().delete_environment(&path_name(&name)?).await?;
    Ok(StatusCode::NO_CONTENT)
}
