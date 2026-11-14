//! Identity providers' connectors at Dex, for admins. A connector's config
//! holds its secrets, so it goes to Dex and is never returned or logged.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use sekisho_auth::dex::{DEX_TIMEOUT, DexConnector, DexError};
use sekisho_core::names::ConnectorId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use tokio::time::timeout;

use super::{ApiError, ApiJson, Fields};
use crate::app::AppState;
use crate::auth::Caller;

/// The connector types Sekisho supports (design 9.1). No SAML in release 1
/// (D123): Dex v2.46.1 gives a SAML sign-in no refresh token, so it couldn't
/// be checked again; identity providers connect through OIDC instead.
pub const TYPES: [&str; 6] = ["github", "gitlab", "google", "microsoft", "ldap", "oidc"];
const NAME_MAX_CHARS: usize = 100;

#[derive(Serialize)]
pub struct ConnectorBody {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    name: String,
}

impl From<DexConnector> for ConnectorBody {
    fn from(c: DexConnector) -> Self {
        Self {
            id: c.id.to_string(),
            kind: c.kind,
            name: c.name,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorPut {
    #[serde(rename = "type")]
    kind: String,
    name: String,
    config: Value,
}

/// A Dex call with Dex's timeout; its errors never quote what was sent.
async fn dex<T>(call: impl Future<Output = Result<T, DexError>>) -> Result<T, ApiError> {
    match timeout(DEX_TIMEOUT, call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(DexError::Invalid(_))) => {
            // Dex's message may quote the config, so it stays out of the log.
            tracing::warn!("Dex refused a connector change");
            let mut fields = Fields::default();
            fields.push("config", "Dex refused this connector's config");
            Err(fields.fail())
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, "Dex couldn't be asked about connectors");
            Err(ApiError::Unavailable(
                "the sign-in service can't be reached; try again".into(),
            ))
        }
        Err(_) => Err(ApiError::Unavailable(
            "the sign-in service didn't answer; try again".into(),
        )),
    }
}

fn connector_id(id: &str) -> Result<ConnectorId, ApiError> {
    ConnectorId::parse(id).map_err(|_| ApiError::NotFound)
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<ConnectorBody>>, ApiError> {
    let connectors = dex(state.dex().connectors()).await?;
    Ok(Json(connectors.into_iter().map(Into::into).collect()))
}

/// Creates the connector or replaces it.
pub async fn put(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<ConnectorPut>,
) -> Result<Json<ConnectorBody>, ApiError> {
    let mut fields = Fields::default();
    let id = fields.check("id", ConnectorId::parse(&id));
    if !TYPES.contains(&body.kind.as_str()) {
        fields.push("type", format!("use one of {}", TYPES.join(", ")));
    }
    let name = body.name.trim();
    if name.is_empty()
        || name.chars().count() > NAME_MAX_CHARS
        || name.chars().any(char::is_control)
    {
        fields.push("name", "use 1 to 100 characters, no control characters");
    }
    if !body.config.is_object() {
        fields.push("config", "must be an object, as Dex's connector config");
    }
    let Some(id) = id else {
        return Err(fields.fail());
    };
    fields.finish()?;
    let connector = DexConnector {
        id,
        kind: body.kind,
        name: name.to_owned(),
    };
    dex(state.dex().put_connector(&connector, &body.config)).await?;
    tracing::info!(connector = %connector.id, by = %caller.user.id, "saved a connector");
    Ok(Json(connector.into()))
}

/// Deletes the connector, and ends the sessions of the people whose latest
/// sign-in came through it.
pub async fn delete(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id = connector_id(&id)?;
    // It would end the admin's own session, and could leave them no way to
    // sign in.
    if caller.user.dex_connector.as_deref() == Some(id.as_str()) {
        return Err(ApiError::Conflict(format!(
            "you signed in through {id}; another admin can delete it, or sign in another way \
             first"
        )));
    }
    if !dex(state.dex().delete_connector(&id)).await? {
        return Err(ApiError::NotFound);
    }
    let ended = state
        .store()
        .end_connector_sessions(id.as_str(), "connector_deleted", OffsetDateTime::now_utc())
        .await?;
    tracing::info!(connector = %id, by = %caller.user.id, ended, "deleted a connector");
    Ok(StatusCode::NO_CONTENT)
}
