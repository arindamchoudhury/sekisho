use std::fmt;

use sekisho_core::catalog;
use sekisho_core::formats::{
    AzureEndpoint, AzureResourceType, HostApi, HostApiPath, HostApis, ProviderKind, is_claude,
};
use sekisho_core::names::ProviderName;
use sekisho_core::secret_box::{SecretBox, SecretError};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::address::{BaseUrl, check_destination, parse_base_url};
use crate::error::{FieldError, bad_column, check, is_foreign_key_violation, is_unique_violation};
use crate::notify::{ChangeKind, notify};
use crate::{Result, Store, StoreError};

/// How agentgateway authenticates to a provider. Every shape but
/// `CloudIdentity` is a secret, sealed with the install's `SecretBox` and
/// read back only through [`Store::provider_credential`].
#[derive(Clone, PartialEq, Eq)]
pub enum ProviderCredential {
    /// An API key, sent as `Authorization: Bearer`.
    Key(String),
    /// IAM access keys, which agentgateway signs Bedrock requests with.
    AwsAccessKeys {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    },
    /// An Entra service principal, which agentgateway gets Azure tokens for.
    AzureClientSecret {
        tenant_id: String,
        client_id: String,
        client_secret: String,
    },
    /// The gateway's own IAM role, or Azure managed or workload identity.
    /// Sekisho stores no secret.
    CloudIdentity,
}

impl ProviderCredential {
    pub fn kind(&self) -> CredentialKind {
        match self {
            ProviderCredential::Key(_) => CredentialKind::Key,
            ProviderCredential::AwsAccessKeys { .. } => CredentialKind::AwsAccessKeys,
            ProviderCredential::AzureClientSecret { .. } => CredentialKind::AzureClientSecret,
            ProviderCredential::CloudIdentity => CredentialKind::CloudIdentity,
        }
    }
}

/// Shows only the shape: never a key, ID, secret or token.
impl fmt::Debug for ProviderCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderCredential::CloudIdentity => f.write_str("CloudIdentity"),
            secret => write!(f, "{:?}(<hidden>)", secret.kind()),
        }
    }
}

/// A credential's shape, as listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CredentialKind {
    Key,
    AwsAccessKeys,
    AzureClientSecret,
    CloudIdentity,
}

impl CredentialKind {
    pub const ALL: [CredentialKind; 4] = [
        CredentialKind::Key,
        CredentialKind::AwsAccessKeys,
        CredentialKind::AzureClientSecret,
        CredentialKind::CloudIdentity,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            CredentialKind::Key => "key",
            CredentialKind::AwsAccessKeys => "aws_access_keys",
            CredentialKind::AzureClientSecret => "azure_client_secret",
            CredentialKind::CloudIdentity => "cloud_identity",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Whether Sekisho stores a secret for this shape.
    fn is_secret(self) -> bool {
        self != CredentialKind::CloudIdentity
    }

    /// Whether agentgateway would send the gateway's or the provider's cloud
    /// credentials (or AWS signatures) wherever the base URL points.
    fn is_cloud(self) -> bool {
        self != CredentialKind::Key
    }

    fn described(self) -> &'static str {
        match self {
            CredentialKind::Key => "an API key",
            CredentialKind::AwsAccessKeys => "AWS access keys",
            CredentialKind::AzureClientSecret => "an Entra service principal",
            CredentialKind::CloudIdentity => "the gateway's cloud identity",
        }
    }
}

/// A provider kind's own settings. agentgateway builds the upstream host
/// from them when there is no base URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderOptions {
    None,
    Bedrock { region: String },
    Azure { endpoint: AzureEndpoint },
}

#[derive(Debug)]
pub struct NewProvider {
    pub name: ProviderName,
    pub kind: ProviderKind,
    /// Required for `OpenAiCompatible`; otherwise overrides the provider's
    /// default endpoint.
    pub base_url: Option<String>,
    pub options: ProviderOptions,
    /// Sealed when stored and never returned by listings.
    pub credential: ProviderCredential,
    /// Lets the base URL be on a loopback or private network address, or a
    /// host the server can't resolve.
    pub allow_private_network: bool,
    /// The APIs an OpenAI-compatible host serves: required for that kind,
    /// refused for the others, whose APIs agentgateway knows.
    pub apis: Option<HostApis>,
    /// The key agentgateway's model catalog files an OpenAI-compatible
    /// host's prices under (`^[a-z0-9][a-z0-9._-]{0,63}$`); that kind only.
    pub catalog_key: Option<String>,
}

impl NewProvider {
    /// A provider with an API key, no settings, no private network and no
    /// catalog key. An OpenAI-compatible one serves Chat Completions.
    pub fn keyed(
        name: ProviderName,
        kind: ProviderKind,
        base_url: Option<String>,
        key: impl Into<String>,
    ) -> NewProvider {
        NewProvider {
            name,
            kind,
            base_url,
            options: ProviderOptions::None,
            credential: ProviderCredential::Key(key.into()),
            allow_private_network: false,
            apis: (kind == ProviderKind::OpenAiCompatible).then(HostApis::chat_only),
            catalog_key: None,
        }
    }
}

/// Changes to a provider; `None` leaves a field as it is. The name and kind
/// never change.
#[derive(Debug, Default)]
pub struct ProviderUpdate {
    /// A new credential, of any shape the provider takes, replacing the old
    /// one. Required when `base_url` or `options` changes where requests go
    /// while a secret is stored.
    pub credential: Option<ProviderCredential>,
    /// `Some(None)` removes the override (not allowed for `OpenAiCompatible`).
    pub base_url: Option<Option<String>>,
    pub options: Option<ProviderOptions>,
    pub allow_private_network: Option<bool>,
    /// An OpenAI-compatible host's APIs. They don't move the provider to
    /// another host, so they need no credential.
    pub apis: Option<HostApis>,
    /// `Some(None)` removes the catalog key. Refused while it would leave a
    /// model without prices that the catalog doesn't price either.
    pub catalog_key: Option<Option<String>>,
}

/// A provider as listed. Its credential is read only through
/// [`Store::provider_credential`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    pub id: Uuid,
    pub name: ProviderName,
    pub kind: ProviderKind,
    pub base_url: Option<String>,
    pub options: ProviderOptions,
    pub credential: CredentialKind,
    pub allow_private_network: bool,
    /// An OpenAI-compatible host's APIs; `None` for the other kinds.
    pub apis: Option<HostApis>,
    pub catalog_key: Option<String>,
    pub created_at: OffsetDateTime,
}

/// Expands to the provider columns, for building fixed SQL with `concat!`.
macro_rules! columns {
    () => {
        "id, name, kind, base_url, options::text as options, credential_kind, \
         allow_private_network, apis::text as apis, catalog_key, created_at"
    };
}

/// What a provider will be once a create or an update is saved: everything
/// the validation rules look at.
struct Settings<'a> {
    kind: ProviderKind,
    base_url: Option<&'a str>,
    options: &'a ProviderOptions,
    credential: CredentialKind,
    allow_private_network: bool,
    apis: Option<&'a HostApis>,
    catalog_key: Option<&'a str>,
}

const AWS_HOSTS: [&str; 2] = [".amazonaws.com", ".api.aws"];
const AZURE_HOSTS: [&str; 3] = [
    ".openai.azure.com",
    ".services.ai.azure.com",
    ".cognitiveservices.azure.com",
];

/// Claude on Azure: agentgateway sends a deployment whose name starts with
/// `claude` to Foundry's Messages endpoint, which takes Claude API keys only
/// in `x-api-key`, a header agentgateway's Azure provider can't send. So it
/// needs Foundry and an Entra credential, checked per model.
pub(crate) const CLAUDE_ON_AZURE_FOUNDRY: &str = "Claude on Azure is served by Foundry: use \
     https://<resource>.services.ai.azure.com/api/projects/<project>";
pub(crate) const CLAUDE_ON_AZURE_ENTRA: &str = "Claude on Azure (Foundry takes Claude API keys only \
     in a header agentgateway can't send) takes an Entra service principal or the gateway's \
     cloud identity";
/// The credentials Claude on Azure works with.
pub(crate) const CLAUDE_ON_AZURE_CREDENTIALS: [CredentialKind; 2] = [
    CredentialKind::AzureClientSecret,
    CredentialKind::CloudIdentity,
];

/// Whether Claude on Azure is served by this endpoint.
pub(crate) fn is_foundry(options: &ProviderOptions) -> bool {
    matches!(options, ProviderOptions::Azure { endpoint }
        if endpoint.resource_type == AzureResourceType::Foundry)
}

/// The shapes each kind takes.
fn allowed_credentials(kind: ProviderKind) -> &'static [CredentialKind] {
    use CredentialKind::*;
    match kind {
        ProviderKind::Bedrock => &[Key, AwsAccessKeys, CloudIdentity],
        ProviderKind::Azure => &[Key, AzureClientSecret, CloudIdentity],
        ProviderKind::Anthropic
        | ProviderKind::OpenAi
        | ProviderKind::Gemini
        | ProviderKind::OpenAiCompatible => &[Key],
    }
}

/// Checks a provider as it will be saved; `new_credential` is the credential
/// being set, if any. The address check resolves the base URL's host, so this
/// runs before any transaction. Errors come in a fixed order: `credential`,
/// `region`, `endpoint`, `base_url`, `apis`, `catalog_key`, then the
/// credential's own fields.
async fn validate(
    s: &Settings<'_>,
    new_credential: Option<&ProviderCredential>,
) -> Vec<FieldError> {
    let mut errors = Vec::new();
    let allowed = allowed_credentials(s.kind);
    let fits = allowed.contains(&s.credential);
    if !fits {
        let shapes: Vec<_> = allowed.iter().map(|k| k.described()).collect();
        errors.push(FieldError::new(
            "credential",
            format!(
                "a {} provider takes {}",
                s.kind.as_str(),
                shapes.join(" or ")
            ),
        ));
    }
    validate_options(&mut errors, s);
    validate_base_url(&mut errors, s).await;
    let compatible = s.kind == ProviderKind::OpenAiCompatible;
    match (compatible, s.apis) {
        (true, None) => errors.push(FieldError::new(
            "apis",
            "an OpenAI-compatible provider needs its host's APIs: chat_completions, \
             and messages or responses where the host serves them",
        )),
        (false, Some(_)) => errors.push(FieldError::new(
            "apis",
            "only OpenAI-compatible providers declare their APIs; agentgateway knows the others'",
        )),
        _ => {}
    }
    match s.catalog_key {
        Some(_) if !compatible => errors.push(FieldError::new(
            "catalog_key",
            "only OpenAI-compatible providers have a catalog key",
        )),
        Some(key) if !is_catalog_key(key) => errors.push(FieldError::new(
            "catalog_key",
            "use up to 64 lowercase letters, digits, dots, underscores or hyphens, \
             starting with a letter or digit",
        )),
        _ => {}
    }
    if let Some(credential) = new_credential.filter(|_| fits) {
        validate_credential(&mut errors, credential);
    }
    errors
}

fn validate_options(errors: &mut Vec<FieldError>, s: &Settings<'_>) {
    match (s.kind, s.options) {
        (ProviderKind::Bedrock, ProviderOptions::Bedrock { region }) => {
            if !is_aws_region(region) {
                errors.push(FieldError::new(
                    "region",
                    "use an AWS region such as us-east-1",
                ));
            }
        }
        (ProviderKind::Bedrock, _) => errors.push(FieldError::new(
            "region",
            "a Bedrock provider needs a region, such as us-east-1",
        )),
        (ProviderKind::Azure, ProviderOptions::Azure { endpoint }) => {
            // The fields are public, so an endpoint may not have come from
            // parse; it must still be one that parse accepts.
            match AzureEndpoint::parse(&endpoint.url()) {
                Ok(parsed) if parsed == *endpoint => {}
                Ok(_) => errors.push(FieldError::new(
                    "endpoint",
                    "an Azure OpenAI endpoint has no project",
                )),
                Err(message) => errors.push(FieldError::new("endpoint", message)),
            }
        }
        (ProviderKind::Azure, _) => errors.push(FieldError::new(
            "endpoint",
            "an Azure provider needs its endpoint, such as https://<resource>.openai.azure.com",
        )),
        (_, ProviderOptions::Bedrock { .. }) => errors.push(FieldError::new(
            "region",
            "only Bedrock providers have a region",
        )),
        (_, ProviderOptions::Azure { .. }) => errors.push(FieldError::new(
            "endpoint",
            "only Azure providers have an endpoint",
        )),
        (_, ProviderOptions::None) => {}
    }
}

async fn validate_base_url(errors: &mut Vec<FieldError>, s: &Settings<'_>) {
    let Some(url) = s.base_url else {
        if s.kind == ProviderKind::OpenAiCompatible {
            errors.push(FieldError::new(
                "base_url",
                "an OpenAI-compatible provider needs a base URL",
            ));
        }
        return;
    };
    // Read as agentgateway v1.7.0 reads it: one it refuses holds back every
    // environment's config, and the address checks below must see the host
    // it connects to. Never quoted: user info is a credential.
    let url = match parse_base_url(url) {
        Ok(url) => url,
        Err(error) => {
            errors.push(FieldError::new("base_url", error.message()));
            return;
        }
    };
    // Only a build for the end-to-end tests lets cloud credentials reach the
    // mock model.
    if s.credential.is_cloud() && !cfg!(feature = "e2e") {
        let (cloud, hosts): (&str, &[&str]) = match s.kind {
            ProviderKind::Bedrock => ("AWS", &AWS_HOSTS),
            ProviderKind::Azure => ("Azure", &AZURE_HOSTS),
            // Only Bedrock and Azure take cloud credentials; the credential
            // error is already reported.
            _ => ("", &[]),
        };
        if !hosts.is_empty() && !is_cloud_host(&url, hosts) {
            errors.push(FieldError::new(
                "base_url",
                format!(
                    "{} only goes to {cloud} hosts: use https:// on a host ending in {}",
                    s.credential.described(),
                    hosts.join(" or ")
                ),
            ));
            return;
        }
    }
    if let Err(message) = check_destination(&url, s.allow_private_network).await {
        errors.push(FieldError::new("base_url", message));
    }
}

fn validate_credential(errors: &mut Vec<FieldError>, credential: &ProviderCredential) {
    const NOT_EMPTY: &str = "must not be empty";
    match credential {
        ProviderCredential::Key(key) => {
            if key.is_empty() {
                errors.push(FieldError::new("api_key", NOT_EMPTY));
            }
        }
        ProviderCredential::AwsAccessKeys {
            access_key_id,
            secret_access_key,
            session_token,
        } => {
            if !(16..=128).contains(&access_key_id.len())
                || !access_key_id
                    .bytes()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            {
                errors.push(FieldError::new(
                    "aws_access_key_id",
                    "use the access key ID AWS shows, such as AKIAIOSFODNN7EXAMPLE",
                ));
            }
            if secret_access_key.is_empty() {
                errors.push(FieldError::new("aws_secret_access_key", NOT_EMPTY));
            }
            if session_token.as_deref() == Some("") {
                errors.push(FieldError::new("aws_session_token", NOT_EMPTY));
            }
        }
        ProviderCredential::AzureClientSecret {
            tenant_id,
            client_id,
            client_secret,
        } => {
            if !is_guid(tenant_id) && !is_domain(tenant_id) {
                errors.push(FieldError::new(
                    "azure_tenant_id",
                    "use the tenant's ID or a domain such as contoso.onmicrosoft.com",
                ));
            }
            if !is_guid(client_id) {
                errors.push(FieldError::new(
                    "azure_client_id",
                    "use the application (client) ID, a GUID",
                ));
            }
            if client_secret.is_empty() {
                errors.push(FieldError::new("azure_client_secret", NOT_EMPTY));
            }
        }
        ProviderCredential::CloudIdentity => {}
    }
}

/// `^[a-z0-9][a-z0-9._-]{0,63}$`, as agentgateway's catalog names hosts.
fn is_catalog_key(key: &str) -> bool {
    key.len() <= 64
        && key
            .bytes()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && key
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
}

/// `https://` on a host name ending in one of `suffixes`. The host is the
/// one agentgateway connects to, already lowercased by the parser.
fn is_cloud_host(url: &BaseUrl, suffixes: &[&str]) -> bool {
    let url::Host::Domain(host) = &url.host else {
        return false;
    };
    url.tls
        && host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'.')
        && suffixes.iter().any(|suffix| {
            host.strip_suffix(suffix)
                .is_some_and(|label| !label.is_empty() && !label.ends_with('.'))
        })
}

/// `^[a-z]{2}(-[a-z]+)+-[0-9]{1,2}$`, such as `us-east-1` or `us-gov-west-1`.
/// agentgateway builds `bedrock-runtime.<region>.amazonaws.com` from it.
fn is_aws_region(region: &str) -> bool {
    let parts: Vec<&str> = region.split('-').collect();
    let lower = |p: &&str| !p.is_empty() && p.bytes().all(|c| c.is_ascii_lowercase());
    match parts.as_slice() {
        [area, middle @ .., number] if !middle.is_empty() => {
            area.len() == 2
                && lower(area)
                && middle.iter().all(lower)
                && (1..=2).contains(&number.len())
                && number.bytes().all(|c| c.is_ascii_digit())
        }
        _ => false,
    }
}

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` in hex digits.
fn is_guid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, len)| g.len() == len && g.bytes().all(|c| c.is_ascii_hexdigit()))
}

/// Two or more labels of letters, digits and hyphens, such as
/// `contoso.onmicrosoft.com`. agentgateway puts the tenant in Microsoft's
/// sign-in URL path.
fn is_domain(s: &str) -> bool {
    let labels: Vec<&str> = s.split('.').collect();
    s.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|l| {
            (1..=63).contains(&l.len())
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
}

#[derive(Deserialize)]
struct SealedAwsKeys {
    access_key_id: String,
    secret_access_key: String,
    #[serde(default)]
    session_token: Option<String>,
}

#[derive(Deserialize)]
struct SealedAzureClientSecret {
    tenant_id: String,
    client_id: String,
    client_secret: String,
}

/// An API key is sealed as its bytes, as in plan 1, so plan 1's rows stay
/// valid; AWS keys and service principals as JSON; the cloud identity not
/// at all.
fn seal(secret_box: &SecretBox, credential: &ProviderCredential) -> Option<Vec<u8>> {
    let plaintext = match credential {
        ProviderCredential::Key(key) => key.as_bytes().to_vec(),
        ProviderCredential::AwsAccessKeys {
            access_key_id,
            secret_access_key,
            session_token,
        } => json!({
            "access_key_id": access_key_id,
            "secret_access_key": secret_access_key,
            "session_token": session_token,
        })
        .to_string()
        .into_bytes(),
        ProviderCredential::AzureClientSecret {
            tenant_id,
            client_id,
            client_secret,
        } => json!({
            "tenant_id": tenant_id,
            "client_id": client_id,
            "client_secret": client_secret,
        })
        .to_string()
        .into_bytes(),
        ProviderCredential::CloudIdentity => return None,
    };
    Some(secret_box.seal(&plaintext))
}

fn options_json(options: &ProviderOptions) -> String {
    match options {
        ProviderOptions::None => json!({}),
        ProviderOptions::Bedrock { region } => json!({ "region": region }),
        ProviderOptions::Azure { endpoint } => json!({ "endpoint": endpoint.url() }),
    }
    .to_string()
}

pub(crate) fn options_from_json(kind: ProviderKind, text: &str) -> Result<ProviderOptions> {
    let bad = || bad_column("provider options", text);
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| bad())?;
    let field = |name: &str| value.get(name).and_then(|v| v.as_str()).ok_or_else(bad);
    Ok(match kind {
        ProviderKind::Bedrock => ProviderOptions::Bedrock {
            region: field("region")?.to_owned(),
        },
        ProviderKind::Azure => ProviderOptions::Azure {
            endpoint: AzureEndpoint::parse(field("endpoint")?).map_err(|_| bad())?,
        },
        _ => ProviderOptions::None,
    })
}

/// `[{"api": "chat_completions"}, {"api": "messages", "path": "/anthropic/v1/messages"}]`.
fn apis_json(apis: &HostApis) -> String {
    let list: Vec<serde_json::Value> = apis
        .iter()
        .map(|a| match &a.path {
            Some(path) => json!({ "api": a.api.as_str(), "path": path }),
            None => json!({ "api": a.api.as_str() }),
        })
        .collect();
    serde_json::Value::from(list).to_string()
}

fn apis_from_json(text: &str) -> Result<HostApis> {
    let bad = || bad_column("provider APIs", text);
    let list: Vec<serde_json::Value> = serde_json::from_str(text).map_err(|_| bad())?;
    let apis = list
        .iter()
        .map(|entry| {
            let api = entry.get("api").and_then(|a| a.as_str()).ok_or_else(bad)?;
            Ok(HostApiPath {
                api: HostApi::parse(api).ok_or_else(bad)?,
                path: match entry.get("path") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(path) => Some(path.as_str().ok_or_else(bad)?.to_owned()),
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;
    HostApis::new(apis).map_err(|_| bad())
}

fn from_row(row: &PgRow) -> Result<Provider> {
    let name: String = row.try_get("name")?;
    let kind: String = row.try_get("kind")?;
    let kind = ProviderKind::parse(&kind).ok_or_else(|| bad_column("provider kind", &kind))?;
    let credential: String = row.try_get("credential_kind")?;
    let options: String = row.try_get("options")?;
    let apis: Option<String> = row.try_get("apis")?;
    Ok(Provider {
        id: row.try_get("id")?,
        name: ProviderName::parse(&name).map_err(|_| bad_column("provider name", &name))?,
        kind,
        base_url: row.try_get("base_url")?,
        options: options_from_json(kind, &options)?,
        credential: CredentialKind::parse(&credential)
            .ok_or_else(|| bad_column("credential kind", &credential))?,
        allow_private_network: row.try_get("allow_private_network")?,
        apis: apis.as_deref().map(apis_from_json).transpose()?,
        catalog_key: row.try_get("catalog_key")?,
        created_at: row.try_get("created_at")?,
    })
}

impl Store {
    /// Adds a provider, sealing its credential with `secret_box`.
    pub async fn create_provider(
        &self,
        secret_box: &SecretBox,
        new: NewProvider,
    ) -> Result<Provider> {
        let settings = Settings {
            kind: new.kind,
            base_url: new.base_url.as_deref(),
            options: &new.options,
            credential: new.credential.kind(),
            allow_private_network: new.allow_private_network,
            apis: new.apis.as_ref(),
            catalog_key: new.catalog_key.as_deref(),
        };
        check(validate(&settings, Some(&new.credential)).await)?;
        let ciphertext = seal(secret_box, &new.credential);
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(concat!(
            "insert into providers (name, kind, base_url, options, credential_kind, \
             credential_ciphertext, allow_private_network, apis, catalog_key) \
             values ($1, $2, $3, $4::jsonb, $5, $6, $7, $8::jsonb, $9) returning ",
            columns!()
        ))
        .bind(new.name.as_str())
        .bind(new.kind.as_str())
        .bind(&new.base_url)
        .bind(options_json(&new.options))
        .bind(new.credential.kind().as_str())
        .bind(&ciphertext)
        .bind(new.allow_private_network)
        .bind(new.apis.as_ref().map(apis_json))
        .bind(&new.catalog_key)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                StoreError::Conflict(format!("a provider named {} already exists", new.name))
            } else {
                e.into()
            }
        })?;
        notify(&mut tx, ChangeKind::Providers).await?;
        tx.commit().await?;
        from_row(&row)
    }

    /// All providers, sorted by name.
    pub async fn list_providers(&self) -> Result<Vec<Provider>> {
        sqlx::query(concat!(
            "select ",
            columns!(),
            " from providers order by name"
        ))
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(from_row)
        .collect()
    }

    pub async fn get_provider(&self, name: &ProviderName) -> Result<Provider> {
        let row = sqlx::query(concat!(
            "select ",
            columns!(),
            " from providers where name = $1"
        ))
        .bind(name.as_str())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound)?;
        from_row(&row)
    }

    /// Replaces a provider's credential (of any shape it takes) or changes
    /// its base URL, settings, `allow_private_network`, APIs or catalog key.
    /// The result is validated as `create_provider` validates a new
    /// provider. A change to where requests go needs the credential again
    /// while a secret is stored. A change its models can't follow is refused:
    /// Claude on Azure needs Foundry and an Entra credential, and an unpriced
    /// model needs the catalog key that prices it. Its models and
    /// environments keep using it; the gateway config is re-rendered.
    pub async fn update_provider(
        &self,
        secret_box: &SecretBox,
        name: &ProviderName,
        update: ProviderUpdate,
    ) -> Result<Provider> {
        // Validation resolves the base URL's host, so it reads the provider
        // first and runs outside the transaction.
        let current = self.get_provider(name).await?;
        let base_url = match &update.base_url {
            Some(url) => url.clone(),
            None => current.base_url.clone(),
        };
        let options = update
            .options
            .clone()
            .unwrap_or_else(|| current.options.clone());
        let apis = update.apis.clone().or_else(|| current.apis.clone());
        let catalog_key = match &update.catalog_key {
            Some(key) => key.clone(),
            None => current.catalog_key.clone(),
        };
        let settings = Settings {
            kind: current.kind,
            base_url: base_url.as_deref(),
            options: &options,
            credential: update
                .credential
                .as_ref()
                .map_or(current.credential, ProviderCredential::kind),
            allow_private_network: update
                .allow_private_network
                .unwrap_or(current.allow_private_network),
            apis: apis.as_ref(),
            catalog_key: catalog_key.as_deref(),
        };
        let mut errors = validate(&settings, update.credential.as_ref()).await;
        let moves = base_url != current.base_url || options != current.options;
        if moves && update.credential.is_none() && current.credential.is_secret() {
            // As in Obot: a stored secret is never redirected to another
            // server by someone who doesn't have it.
            errors.push(FieldError::new(
                "credential",
                "changing where requests go needs the credential again",
            ));
        }
        check(errors)?;
        let credential = update
            .credential
            .as_ref()
            .map(|c| (c.kind().as_str(), seal(secret_box, c)));

        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query(concat!(
            "select ",
            columns!(),
            " from providers where id = $1 for update"
        ))
        .bind(current.id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(StoreError::NotFound)?;
        if from_row(&locked)? != current {
            return Err(StoreError::Conflict(format!(
                "provider {name} changed while this update was checked; try again"
            )));
        }
        // The lock makes create_model, which share-locks the provider,
        // wait, so no model is added under the old settings meanwhile.
        let models: Vec<(String, bool)> = sqlx::query_as(
            "select name, input_price_micro_usd is null from models \
             where provider_id = $1 order by name",
        )
        .bind(current.id)
        .fetch_all(&mut *tx)
        .await?;
        check(models_follow(
            &current,
            &settings,
            &options,
            catalog_key.as_deref(),
            &models,
        ))?;
        let row = sqlx::query(concat!(
            "update providers set base_url = $2, options = $3::jsonb, \
             allow_private_network = $4, \
             credential_kind = coalesce($5, credential_kind), \
             credential_ciphertext = case when $5::text is null \
             then credential_ciphertext else $6 end, \
             apis = $7::jsonb, catalog_key = $8 \
             where id = $1 returning ",
            columns!()
        ))
        .bind(current.id)
        .bind(&base_url)
        .bind(options_json(&options))
        .bind(settings.allow_private_network)
        .bind(credential.as_ref().map(|(kind, _)| *kind))
        .bind(credential.and_then(|(_, sealed)| sealed))
        .bind(apis.as_ref().map(apis_json))
        .bind(&catalog_key)
        .fetch_one(&mut *tx)
        .await?;
        notify(&mut tx, ChangeKind::Providers).await?;
        tx.commit().await?;
        from_row(&row)
    }

    /// Decrypts a provider's credential, for rendering the gateway config.
    pub async fn provider_credential(
        &self,
        secret_box: &SecretBox,
        id: Uuid,
    ) -> Result<ProviderCredential> {
        Ok(self.provider_with_credential(secret_box, id).await?.1)
    }

    /// A provider and its decrypted credential, read from one row, so the
    /// credential always comes with the base URL and settings it was saved
    /// with. The gateway config is rendered from these: a credential paired
    /// with another version's base URL could reach a host it must not.
    pub async fn provider_with_credential(
        &self,
        secret_box: &SecretBox,
        id: Uuid,
    ) -> Result<(Provider, ProviderCredential)> {
        let row = sqlx::query(concat!(
            "select ",
            columns!(),
            ", credential_ciphertext from providers where id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound)?;
        let provider = from_row(&row)?;
        let sealed: Option<Vec<u8>> = row.try_get("credential_ciphertext")?;
        let credential = open_credential(secret_box, provider.credential, sealed)?;
        Ok((provider, credential))
    }

    /// Deletes a provider that no model uses.
    pub async fn delete_provider(&self, name: &ProviderName) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        // The row lock makes a concurrent create_model for this provider wait,
        // then fail on its foreign key.
        let id: Uuid = sqlx::query_scalar("select id from providers where name = $1 for update")
            .bind(name.as_str())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound)?;
        let models: Vec<String> =
            sqlx::query_scalar("select name from models where provider_id = $1 order by name")
                .bind(id)
                .fetch_all(&mut *tx)
                .await?;
        if !models.is_empty() {
            return Err(StoreError::InUse(format!("models: {}", models.join(", "))));
        }
        sqlx::query("delete from providers where id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|e| {
                if is_foreign_key_violation(&e) {
                    StoreError::InUse("models".into())
                } else {
                    e.into()
                }
            })?;
        notify(&mut tx, ChangeKind::Providers).await?;
        tx.commit().await?;
        Ok(())
    }
}

/// What a provider's models need of an update: Claude on Azure keeps Foundry
/// and an Entra credential, and every unpriced model of an OpenAI-compatible
/// host keeps a catalog key that prices it. `models` are its models' names,
/// each with whether it has no prices of its own.
fn models_follow(
    current: &Provider,
    s: &Settings<'_>,
    options: &ProviderOptions,
    catalog_key: Option<&str>,
    models: &[(String, bool)],
) -> Vec<FieldError> {
    let mut errors = Vec::new();
    if current.kind == ProviderKind::Azure {
        let claude: Vec<&str> = models
            .iter()
            .map(|(name, _)| name.as_str())
            .filter(|name| is_claude(ProviderKind::Azure, name))
            .collect();
        if !claude.is_empty() {
            let serves = format!("; this provider serves {}", claude.join(", "));
            if !CLAUDE_ON_AZURE_CREDENTIALS.contains(&s.credential) {
                errors.push(FieldError::new(
                    "credential",
                    format!("{CLAUDE_ON_AZURE_ENTRA}{serves}"),
                ));
            }
            if !is_foundry(options) {
                errors.push(FieldError::new(
                    "endpoint",
                    format!("{CLAUDE_ON_AZURE_FOUNDRY}{serves}"),
                ));
            }
        }
    }
    if current.kind == ProviderKind::OpenAiCompatible
        && catalog_key != current.catalog_key.as_deref()
    {
        let unpriced: Vec<&str> = models
            .iter()
            .filter(|(name, unpriced)| {
                *unpriced && !catalog_key.is_some_and(|key| catalog::knows(key, name))
            })
            .map(|(name, _)| name.as_str())
            .collect();
        if !unpriced.is_empty() {
            let under = match catalog_key {
                Some(key) => format!("under {key}"),
                None => "without a catalog key".to_owned(),
            };
            errors.push(FieldError::new(
                "catalog_key",
                format!(
                    "{} would have no prices: agentgateway's catalog has none for them {under}; \
                     keep the catalog key, or remove them from their environments, delete \
                     them and add them again with prices",
                    unpriced.join(", ")
                ),
            ));
        }
    }
    errors
}

/// Opens a sealed credential of shape `kind`; the cloud identity has none.
fn open_credential(
    secret_box: &SecretBox,
    kind: CredentialKind,
    sealed: Option<Vec<u8>>,
) -> Result<ProviderCredential> {
    if kind == CredentialKind::CloudIdentity {
        return Ok(ProviderCredential::CloudIdentity);
    }
    let sealed = sealed.ok_or_else(|| bad_column("credential", "NULL"))?;
    let plaintext = secret_box.open(&sealed)?;
    // Decoding errors could quote the plaintext, so none is passed on.
    let undecodable = || StoreError::Secret(SecretError::Decrypt);
    Ok(match kind {
        CredentialKind::Key => {
            ProviderCredential::Key(String::from_utf8(plaintext).map_err(|_| undecodable())?)
        }
        CredentialKind::AwsAccessKeys => {
            let keys: SealedAwsKeys =
                serde_json::from_slice(&plaintext).map_err(|_| undecodable())?;
            ProviderCredential::AwsAccessKeys {
                access_key_id: keys.access_key_id,
                secret_access_key: keys.secret_access_key,
                session_token: keys.session_token,
            }
        }
        CredentialKind::AzureClientSecret => {
            let sp: SealedAzureClientSecret =
                serde_json::from_slice(&plaintext).map_err(|_| undecodable())?;
            ProviderCredential::AzureClientSecret {
                tenant_id: sp.tenant_id,
                client_id: sp.client_id,
                client_secret: sp.client_secret,
            }
        }
        CredentialKind::CloudIdentity => ProviderCredential::CloudIdentity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_hosts_are_matched_by_suffix() {
        let is_cloud_host = |url: &str, hosts: &[&str]| {
            parse_base_url(url).is_ok_and(|url| super::is_cloud_host(&url, hosts))
        };
        assert!(is_cloud_host(
            "https://bedrock-runtime.us-east-1.amazonaws.com",
            &AWS_HOSTS
        ));
        assert!(is_cloud_host("https://X.API.AWS:8443/p", &AWS_HOSTS));
        for refused in [
            "http://bedrock-runtime.us-east-1.amazonaws.com",
            "https://amazonaws.com",
            "https://.amazonaws.com",
            "https://x..amazonaws.com",
            "https://evil_x.amazonaws.com",
            "https://evil.com/.amazonaws.com",
            "https://x.amazonaws.com@evil.com",
            "https://x.amazonaws.com.",
        ] {
            assert!(!is_cloud_host(refused, &AWS_HOSTS), "{refused}");
        }
    }

    #[test]
    fn guids_and_domains() {
        assert!(is_guid("11111111-2222-3333-4444-55555555555a"));
        assert!(!is_guid("11111111-2222-3333-4444-5555555555"));
        assert!(!is_guid("11111111-2222-3333-4444-55555555555g"));
        assert!(is_domain("contoso.onmicrosoft.com"));
        assert!(!is_domain("contoso"));
        assert!(!is_domain("-a.com"));
        assert!(!is_domain("a..com"));
    }
}
