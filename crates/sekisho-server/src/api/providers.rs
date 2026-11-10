use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use sekisho_core::formats::{ApiFormat, AzureEndpoint, ProviderKind};
use sekisho_core::names::ProviderName;
use sekisho_store::{NewProvider, Provider, ProviderCredential, ProviderOptions, ProviderUpdate};
use serde::{Deserialize, Deserializer, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{ApiError, ApiJson, Fields};
use crate::app::AppState;

/// Adds a provider. `format` is required for Bedrock and Azure, which serve
/// either route; other kinds have only one. Exactly one credential shape:
/// `api_key`; `aws_access_key_id` with `aws_secret_access_key` and an
/// optional `aws_session_token`; `azure_tenant_id`, `azure_client_id` and
/// `azure_client_secret`; or `"cloud_identity": true`. Credential fields are
/// write-only.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewProviderBody {
    name: String,
    kind: String,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    /// Bedrock's AWS region.
    #[serde(default)]
    region: Option<String>,
    /// Azure's endpoint, as the Azure portal shows it.
    #[serde(default)]
    endpoint: Option<String>,
    /// Lets `base_url` be on a loopback or private network address, or a
    /// host only the gateway's network resolves.
    #[serde(default)]
    allow_private_network: bool,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    aws_access_key_id: Option<String>,
    #[serde(default)]
    aws_secret_access_key: Option<String>,
    #[serde(default)]
    aws_session_token: Option<String>,
    #[serde(default)]
    azure_tenant_id: Option<String>,
    #[serde(default)]
    azure_client_id: Option<String>,
    #[serde(default)]
    azure_client_secret: Option<String>,
    #[serde(default)]
    cloud_identity: Option<bool>,
}

/// Changes a provider. Absent fields stay as they are; `"base_url": null`
/// removes the override. At most one credential shape, as on create,
/// replaces the stored credential; changing where requests go needs it. The
/// name, kind and format never change, so they are unknown fields here.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderUpdateBody {
    #[serde(default, deserialize_with = "present")]
    base_url: Option<Option<String>>,
    #[serde(default)]
    region: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    allow_private_network: Option<bool>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    aws_access_key_id: Option<String>,
    #[serde(default)]
    aws_secret_access_key: Option<String>,
    #[serde(default)]
    aws_session_token: Option<String>,
    #[serde(default)]
    azure_tenant_id: Option<String>,
    #[serde(default)]
    azure_client_id: Option<String>,
    #[serde(default)]
    azure_client_secret: Option<String>,
    #[serde(default)]
    cloud_identity: Option<bool>,
}

/// Tells a present `null` (`Some(None)`) from an absent field (`None`).
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

/// A provider as answered. No credential value appears, not even an AWS
/// access key ID or an Entra client ID: only the credential's shape.
#[derive(Serialize)]
pub struct ProviderBody {
    id: Uuid,
    name: String,
    kind: &'static str,
    format: &'static str,
    base_url: Option<String>,
    /// Bedrock providers only.
    #[serde(skip_serializing_if = "Option::is_none")]
    region: Option<String>,
    /// Azure providers only, in its canonical form.
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    /// `key`, `aws_access_keys`, `azure_client_secret` or `cloud_identity`.
    credential: &'static str,
    allow_private_network: bool,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

impl From<Provider> for ProviderBody {
    fn from(p: Provider) -> Self {
        let (region, endpoint) = match p.options {
            ProviderOptions::None => (None, None),
            ProviderOptions::Bedrock { region } => (Some(region), None),
            ProviderOptions::Azure { endpoint } => (None, Some(endpoint.url())),
        };
        Self {
            id: p.id,
            name: p.name.to_string(),
            kind: p.kind.as_str(),
            format: p.format.as_str(),
            base_url: p.base_url,
            region,
            endpoint,
            credential: p.credential.as_str(),
            allow_private_network: p.allow_private_network,
            created_at: p.created_at,
        }
    }
}

/// A request's credential fields, before they are checked.
struct CredentialFields {
    api_key: Option<String>,
    aws_access_key_id: Option<String>,
    aws_secret_access_key: Option<String>,
    aws_session_token: Option<String>,
    azure_tenant_id: Option<String>,
    azure_client_id: Option<String>,
    azure_client_secret: Option<String>,
    cloud_identity: Option<bool>,
}

const SHAPES: &str = "api_key; aws_access_key_id and aws_secret_access_key (with an optional \
     aws_session_token); azure_tenant_id, azure_client_id and azure_client_secret; or \
     \"cloud_identity\": true";

impl CredentialFields {
    /// The one credential given, if any. `required` makes none an error, as
    /// on create; two are always one.
    fn check(self, fields: &mut Fields, required: bool) -> Option<ProviderCredential> {
        let aws = self.aws_access_key_id.is_some()
            || self.aws_secret_access_key.is_some()
            || self.aws_session_token.is_some();
        let azure = self.azure_tenant_id.is_some()
            || self.azure_client_id.is_some()
            || self.azure_client_secret.is_some();
        let cloud = self.cloud_identity == Some(true);
        match [self.api_key.is_some(), aws, azure, cloud]
            .into_iter()
            .filter(|given| *given)
            .count()
        {
            0 => {
                if required {
                    fields.push("credential", format!("give a credential: {SHAPES}"));
                }
                return None;
            }
            1 => {}
            _ => {
                fields.push("credential", format!("give only one credential: {SHAPES}"));
                return None;
            }
        }
        if let Some(key) = self.api_key {
            return Some(ProviderCredential::Key(key));
        }
        if cloud {
            return Some(ProviderCredential::CloudIdentity);
        }
        let mut needed = |field: &str, value: Option<String>, with: &str| {
            if value.is_none() {
                fields.push(field, format!("required with {with}"));
            }
            value
        };
        if aws {
            const WITH: &str = "the other AWS access key fields";
            let access_key_id = needed("aws_access_key_id", self.aws_access_key_id, WITH);
            let secret_access_key =
                needed("aws_secret_access_key", self.aws_secret_access_key, WITH);
            return Some(ProviderCredential::AwsAccessKeys {
                access_key_id: access_key_id?,
                secret_access_key: secret_access_key?,
                session_token: self.aws_session_token,
            });
        }
        const WITH: &str = "the other Entra service principal fields";
        let tenant_id = needed("azure_tenant_id", self.azure_tenant_id, WITH);
        let client_id = needed("azure_client_id", self.azure_client_id, WITH);
        let client_secret = needed("azure_client_secret", self.azure_client_secret, WITH);
        Some(ProviderCredential::AzureClientSecret {
            tenant_id: tenant_id?,
            client_id: client_id?,
            client_secret: client_secret?,
        })
    }
}

/// The format named, or the kind's only one. Whether it fits the kind is
/// the store's rule.
fn format(
    fields: &mut Fields,
    kind: Option<ProviderKind>,
    given: Option<&str>,
) -> Option<ApiFormat> {
    match (given, kind) {
        (Some(given), _) => fields.check(
            "format",
            ApiFormat::parse(given).ok_or("use anthropic, openai or gemini"),
        ),
        (None, Some(kind)) if kind.formats().len() == 1 => Some(kind.formats()[0]),
        (None, Some(kind)) => {
            let formats: Vec<_> = kind.formats().iter().map(|f| f.as_str()).collect();
            fields.push(
                "format",
                format!(
                    "a {} provider serves {}: choose one",
                    kind.as_str(),
                    formats.join(" or ")
                ),
            );
            None
        }
        (None, None) => None,
    }
}

/// The kind's settings given, if any. Whether they fit the kind is mostly
/// the store's rule; only both at once is settled here, by the kind.
fn settings(
    fields: &mut Fields,
    kind: Option<ProviderKind>,
    region: Option<String>,
    endpoint: Option<String>,
) -> Option<ProviderOptions> {
    const ONLY_BEDROCK: &str = "only Bedrock providers have a region";
    const ONLY_AZURE: &str = "only Azure providers have an endpoint";
    let azure = |fields: &mut Fields, endpoint: &str| {
        fields
            .check("endpoint", AzureEndpoint::parse(endpoint))
            .map(|endpoint| ProviderOptions::Azure { endpoint })
    };
    match (region, endpoint) {
        (None, None) => None,
        (Some(region), None) => Some(ProviderOptions::Bedrock { region }),
        (None, Some(endpoint)) => azure(fields, &endpoint),
        (Some(region), Some(endpoint)) => match kind {
            Some(ProviderKind::Bedrock) => {
                fields.push("endpoint", ONLY_AZURE);
                Some(ProviderOptions::Bedrock { region })
            }
            Some(ProviderKind::Azure) => {
                fields.push("region", ONLY_BEDROCK);
                azure(fields, &endpoint)
            }
            _ => {
                fields.push("region", ONLY_BEDROCK);
                fields.push("endpoint", ONLY_AZURE);
                None
            }
        },
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
    let format = format(&mut fields, kind, body.format.as_deref());
    let options = settings(&mut fields, kind, body.region, body.endpoint);
    let credential = CredentialFields {
        api_key: body.api_key,
        aws_access_key_id: body.aws_access_key_id,
        aws_secret_access_key: body.aws_secret_access_key,
        aws_session_token: body.aws_session_token,
        azure_tenant_id: body.azure_tenant_id,
        azure_client_id: body.azure_client_id,
        azure_client_secret: body.azure_client_secret,
        cloud_identity: body.cloud_identity,
    }
    .check(&mut fields, true);
    let (Some(name), Some(kind), Some(format), Some(credential)) = (name, kind, format, credential)
    else {
        return Err(fields.fail());
    };
    fields.finish()?;
    let provider = state
        .store()
        .create_provider(
            state.secret_box(),
            NewProvider {
                name,
                kind,
                format,
                base_url: body.base_url,
                options: options.unwrap_or(ProviderOptions::None),
                credential,
                allow_private_network: body.allow_private_network,
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
    // The stored kind decides which of the settings given belong to it; the
    // store then merges them with the rest and validates the result.
    let current = state.store().get_provider(&name).await?;
    let mut fields = Fields::default();
    let options = settings(&mut fields, Some(current.kind), body.region, body.endpoint);
    let credential = CredentialFields {
        api_key: body.api_key,
        aws_access_key_id: body.aws_access_key_id,
        aws_secret_access_key: body.aws_secret_access_key,
        aws_session_token: body.aws_session_token,
        azure_tenant_id: body.azure_tenant_id,
        azure_client_id: body.azure_client_id,
        azure_client_secret: body.azure_client_secret,
        cloud_identity: body.cloud_identity,
    }
    .check(&mut fields, false);
    fields.finish()?;
    let provider = state
        .store()
        .update_provider(
            state.secret_box(),
            &name,
            ProviderUpdate {
                credential,
                base_url: body.base_url,
                options,
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
