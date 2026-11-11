use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use sekisho_core::money::MicroUsd;
use sekisho_core::names::{ModelName, ProviderName};
use sekisho_store::{Model, NewModel};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ApiError, ApiJson, Fields};
use crate::app::AppState;

/// Prices are USD per 1,000,000 tokens, as decimal strings. Leave input and
/// output out to use agentgateway's catalog prices. Cache prices are optional
/// and only accepted with input and output prices; when unset, the
/// provider's usual ratios to the input price apply.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewModelBody {
    name: String,
    provider: String,
    #[serde(default)]
    input_price: Option<String>,
    #[serde(default)]
    output_price: Option<String>,
    #[serde(default)]
    cache_read_price: Option<String>,
    #[serde(default)]
    cache_write_price: Option<String>,
}

#[derive(Serialize)]
pub struct ModelBody {
    id: Uuid,
    name: String,
    provider: String,
    provider_kind: &'static str,
    input_price: Option<String>,
    output_price: Option<String>,
    cache_read_price: Option<String>,
    cache_write_price: Option<String>,
}

impl From<Model> for ModelBody {
    fn from(m: Model) -> Self {
        Self {
            id: m.id,
            name: m.name.to_string(),
            provider: m.provider.to_string(),
            provider_kind: m.provider_kind.as_str(),
            input_price: m.input_price.map(MicroUsd::to_usd_string),
            output_price: m.output_price.map(MicroUsd::to_usd_string),
            cache_read_price: m.cache_read_price.map(MicroUsd::to_usd_string),
            cache_write_price: m.cache_write_price.map(MicroUsd::to_usd_string),
        }
    }
}

pub async fn create(
    State(state): State<AppState>,
    ApiJson(body): ApiJson<NewModelBody>,
) -> Result<(StatusCode, Json<ModelBody>), ApiError> {
    let mut fields = Fields::default();
    let name = fields.check("name", ModelName::parse(&body.name));
    let provider = fields.check("provider", ProviderName::parse(&body.provider));
    let mut price = |field: &str, value: &Option<String>| match value {
        Some(usd) => fields.check(field, MicroUsd::from_usd_str(usd)).map(Some),
        None => Some(None),
    };
    let input_price = price("input_price", &body.input_price);
    let output_price = price("output_price", &body.output_price);
    let cache_read_price = price("cache_read_price", &body.cache_read_price);
    let cache_write_price = price("cache_write_price", &body.cache_write_price);
    let (
        Some(name),
        Some(provider),
        Some(input_price),
        Some(output_price),
        Some(cache_read_price),
        Some(cache_write_price),
    ) = (
        name,
        provider,
        input_price,
        output_price,
        cache_read_price,
        cache_write_price,
    )
    else {
        return Err(fields.fail());
    };
    let model = state
        .store()
        .create_model(NewModel {
            name,
            provider,
            input_price,
            output_price,
            cache_read_price,
            cache_write_price,
        })
        .await?;
    Ok((StatusCode::CREATED, Json(model.into())))
}

pub async fn list(State(state): State<AppState>) -> Result<Json<Vec<ModelBody>>, ApiError> {
    let models = state.store().list_models().await?;
    Ok(Json(models.into_iter().map(Into::into).collect()))
}

pub async fn delete(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let name = ModelName::parse(&name).map_err(|_| ApiError::NotFound)?;
    state.store().delete_model(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}
