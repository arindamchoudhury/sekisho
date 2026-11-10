use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use sekisho_core::formats::ProviderKind;
use sekisho_core::names::ProviderName;
use sekisho_store::{NewProvider, Provider, ProviderCredential, ProviderUpdate};
use serde::{Deserialize, Deserializer, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{ApiError, ApiJson, Fields};
use crate::app::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewProviderBody {
    name: String,
    kind: String,
    #[serde(default)]
    base_url: Option<String>,
    /// Write-only: never returned.
    api_key: String,
    /// Lets `base_url` be on a loopback or private network address, or a
    /// host only the gateway's network resolves.
    #[serde(default)]
    allow_private_network: bool,
}

/// Rotates the key or changes the base URL. Absent fields stay as they are;
/// `"base_url": null` removes the override. The name and kind never change.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderUpdateBody {
    /// Write-only: never returned.
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default, deserialize_with = "present")]
    base_url: Option<Option<String>>,
    #[serde(default)]
    allow_private_network: Option<bool>,
}

/// Tells a present `null` (`Some(None)`) from an absent field (`None`).
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Serialize)]
pub struct ProviderBody {
    id: Uuid,
    name: String,
    kind: &'static str,
    base_url: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

impl From<Provider> for ProviderBody {
    fn from(p: Provider) -> Self {
        Self {
            id: p.id,
            name: p.name.to_string(),
            kind: p.kind.as_str(),
            base_url: p.base_url,
            created_at: p.created_at,
        }
    }
}

pub async fn create(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<NewProviderBody>,
) -> Result<(StatusCode, Json<ProviderBody>), ApiError> {
    let mut fields = Fields::default();
    let name = fields.check("name", ProviderName::parse(&body.name));
    let kind = fields.check(
        "kind",
        ProviderKind::parse(&body.kind)
            .ok_or("use anthropic, openai, gemini, openai_compatible, bedrock or azure"),
    );
    let (Some(name), Some(kind)) = (name, kind) else {
        return Err(fields.fail());
    };
    let provider = state
        .store()
        .create_provider(
            state.secret_box(),
            NewProvider {
                allow_private_network: body.allow_private_network,
                ..NewProvider::keyed(name, kind, body.base_url, body.api_key)
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(provider.into())))
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<ProviderBody>>, ApiError> {
    let providers = state.store().list_providers().await?;
    Ok(Json(providers.into_iter().map(Into::into).collect()))
}

pub async fn update(
    State(state): State<AppState>,
    Path(name): Path<String>,
    ApiJson(body): ApiJson<ProviderUpdateBody>,
) -> Result<Json<ProviderBody>, ApiError> {
    let name = ProviderName::parse(&name).map_err(|_| ApiError::NotFound)?;
    let provider = state
        .store()
        .update_provider(
            state.secret_box(),
            &name,
            ProviderUpdate {
                credential: body.api_key.map(ProviderCredential::Key),
                base_url: body.base_url,
                options: None,
                allow_private_network: body.allow_private_network,
            },
        )
        .await?;
    Ok(Json(provider.into()))
}

pub async fn delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let name = ProviderName::parse(&name).map_err(|_| ApiError::NotFound)?;
    state.store().delete_provider(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}
