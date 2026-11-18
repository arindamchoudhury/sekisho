//! Obot's virtual MCP servers (D176): offered from a source's applied sync,
//! each as an environment an admin may create. What can't carry over is
//! listed, never guessed.

use axum::Extension;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use sekisho_catalog::environment::McpServerUse;
use sekisho_catalog::model::ObotVmcp;
use sekisho_catalog::names::{EntryKind, EntryName, EntryRef, SourceName};
use sekisho_core::names::{EnvName, ModelName};
use sekisho_store::{EntryKey, EnvironmentSpec, StoreError};
use serde::{Deserialize, Serialize};

use super::environments::{EnvironmentBody, json_body};
use super::{ApiError, ApiJson, Fields};
use crate::app::AppState;
use crate::auth::Caller;

/// One component's server and the tools it enables; `null` is all.
#[derive(Clone, Serialize)]
pub struct ServerBody {
    server: EntryName,
    tools: Option<Vec<String>>,
}

/// A component the environment can't have, and why.
#[derive(Clone, Serialize)]
pub struct MissingBody {
    server: EntryName,
    reason: String,
}

#[derive(Serialize)]
pub struct OfferBody {
    key: EntryName,
    display_name: String,
    description: String,
    /// Every component's server, in Obot's order; one named twice once,
    /// with its tools unioned.
    servers: Vec<ServerBody>,
    /// What Sekisho can't carry over.
    differences: Vec<String>,
    /// Servers left out of the environment.
    missing: Vec<MissingBody>,
}

fn source_path(name: &str) -> Result<SourceName, ApiError> {
    SourceName::parse(name).map_err(|_| ApiError::NotFound)
}

/// The offer's servers, each once: a server named by two components gets
/// the union of their tools, or all of them if either allows all.
fn servers(vmcp: &ObotVmcp) -> Vec<ServerBody> {
    let mut out: Vec<ServerBody> = Vec::new();
    for c in &vmcp.components {
        match out.iter_mut().find(|s| s.server == c.server) {
            Some(s) => {
                s.tools = match (s.tools.take(), &c.tools) {
                    (Some(mut a), Some(b)) => {
                        for t in b {
                            if !a.contains(t) {
                                a.push(t.clone());
                            }
                        }
                        Some(a)
                    }
                    _ => None,
                };
            }
            None => out.push(ServerBody {
                server: c.server.clone(),
                tools: c.tools.clone(),
            }),
        }
    }
    out
}

/// The servers an environment can list: applied MCP server entries of the
/// source that are neither quarantined nor removed, with at least one tool
/// enabled. The rest are missing, with why.
async fn usable(
    state: &AppState,
    source: &SourceName,
    servers: &[ServerBody],
) -> Result<(Vec<ServerBody>, Vec<MissingBody>), ApiError> {
    let (mut ok, mut missing) = (Vec::new(), Vec::new());
    for s in servers {
        let key = EntryKey {
            kind: EntryKind::McpServer,
            source: source.clone(),
            name: s.server.clone(),
        };
        let reason = match state.store().get_entry(&key).await {
            Err(StoreError::NotFound) => Some(format!(
                "{source} has no MCP server {}: it was refused or isn't in the catalog",
                s.server
            )),
            Err(e) => return Err(e.into()),
            Ok(e) if e.applied.is_none() => Some("its source's sync isn't applied yet".to_owned()),
            Ok(e) if e.quarantine.is_some() => Some("it is quarantined".to_owned()),
            Ok(e) if e.removed_at.is_some() => Some("it was removed from its source".to_owned()),
            Ok(_) if s.tools.as_ref().is_some_and(Vec::is_empty) => {
                Some("Obot enables no tool of it".to_owned())
            }
            Ok(_) => None,
        };
        match reason {
            Some(reason) => missing.push(MissingBody {
                server: s.server.clone(),
                reason,
            }),
            None => ok.push(s.clone()),
        }
    }
    Ok((ok, missing))
}

async fn offer(
    state: &AppState,
    source: &SourceName,
    vmcp: ObotVmcp,
) -> Result<OfferBody, ApiError> {
    let all = servers(&vmcp);
    let (_, missing) = usable(state, source, &all).await?;
    Ok(OfferBody {
        key: vmcp.key,
        display_name: vmcp.display_name,
        description: vmcp.description,
        servers: all,
        differences: vmcp.differences,
        missing,
    })
}

/// The virtual servers of the source's applied sync.
pub async fn list(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<Vec<OfferBody>>, ApiError> {
    let source = source_path(&name)?;
    let mut out = Vec::new();
    for vmcp in state.store().applied_vmcps(&source).await? {
        out.push(offer(&state, &source, vmcp).await?);
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    access_groups: Vec<String>,
    #[serde(default)]
    models: Vec<String>,
}

#[derive(Serialize)]
pub struct CreatedBody {
    environment: EnvironmentBody,
    differences: Vec<String>,
    missing: Vec<MissingBody>,
}

/// Creates an environment from a virtual server: its description, and its
/// usable servers as `server@source` with their tools. Never replaces an
/// environment (409).
pub async fn create(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path((name, key)): Path<(String, String)>,
    ApiJson(body): ApiJson<CreateBody>,
) -> Result<(StatusCode, Json<CreatedBody>), ApiError> {
    let source = source_path(&name)?;
    super::git_mode::refuse_in_git_mode(&state, &key).await?;
    let key = EntryName::parse(&key).map_err(|_| ApiError::NotFound)?;
    let vmcp = state
        .store()
        .applied_vmcps(&source)
        .await?
        .into_iter()
        .find(|v| v.key == key)
        .ok_or(ApiError::NotFound)?;

    let mut fields = Fields::default();
    let env_name = fields
        .required("name", body.name)
        .and_then(|n| fields.check("name", EnvName::parse(&n)));
    let models: Vec<Option<ModelName>> = body
        .models
        .iter()
        .enumerate()
        .map(|(i, m)| fields.check(&format!("models[{i}]"), ModelName::parse(m)))
        .collect();
    let (Some(env_name), Some(models)) = (env_name, models.into_iter().collect::<Option<Vec<_>>>())
    else {
        return Err(fields.fail());
    };

    let (usable, missing) = usable(&state, &source, &servers(&vmcp)).await?;
    let mut spec = EnvironmentSpec::new(env_name);
    spec.description = vmcp.description.clone();
    spec.access_groups = body.access_groups;
    spec.models = models;
    spec.mcp_servers = usable
        .into_iter()
        .map(|s| McpServerUse {
            server: EntryRef {
                name: s.server,
                source: Some(source.clone()),
            },
            tools: s.tools,
        })
        .collect();
    let environment = state
        .store()
        .create_environment(spec, caller.user.id)
        .await?;
    tracing::info!(environment = %environment.spec.name, source = %source, vmcp = %key, by = %caller.user.id, "made an environment from an Obot virtual server");
    Ok((
        StatusCode::CREATED,
        Json(CreatedBody {
            environment: json_body(&state, environment).await?,
            differences: vmcp.differences,
            missing,
        }),
    ))
}
