//! Install-wide package registries (D199), for admins: every machine's
//! local MCP servers fetch their npm and Python packages from them. Each
//! is an `https://` URL without a user name, password, query or fragment,
//! since it is written into every machine's server environment; `null`
//! uses the runner's own default.

use axum::Json;
use axum::extract::State;
use sekisho_store::PackageRegistries;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use url::Url;

use super::{ApiError, ApiJson, Fields};
use crate::app::AppState;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegistriesBody {
    #[serde(default)]
    npm: Option<String>,
    #[serde(default)]
    python: Option<String>,
}

impl From<PackageRegistries> for RegistriesBody {
    fn from(r: PackageRegistries) -> Self {
        Self {
            npm: r.npm,
            python: r.python,
        }
    }
}

/// The URL as Sekisho keeps it. The refusal never quotes the value, which
/// may hold a password.
fn registry_url(value: &str) -> Result<String, &'static str> {
    let url = Url::parse(value.trim()).map_err(|_| "not a URL")?;
    if url.scheme() != "https" || url.host().is_none() {
        return Err("use an https URL");
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("must not carry a user name or password");
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err("must not have a query or fragment");
    }
    Ok(url.into())
}

pub async fn get(State(state): State<AppState>) -> Result<Json<RegistriesBody>, ApiError> {
    Ok(Json(state.store().package_registries().await?.into()))
}

/// Replaces both; a missing field is `null`.
pub async fn put(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<RegistriesBody>,
) -> Result<Json<RegistriesBody>, ApiError> {
    let mut fields = Fields::default();
    let mut check = |field: &str, value: Option<String>| {
        value.and_then(|v| fields.check(field, registry_url(&v)))
    };
    let registries = PackageRegistries {
        npm: check("npm", body.npm),
        python: check("python", body.python),
    };
    fields.finish()?;
    state
        .store()
        .set_package_registries(&registries, OffsetDateTime::now_utc())
        .await?;
    tracing::info!(
        npm = registries.npm.as_deref(),
        python = registries.python.as_deref(),
        "set the package registries"
    );
    Ok(Json(registries.into()))
}
