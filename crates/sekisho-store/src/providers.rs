use std::fmt;

use sekisho_core::formats::{ApiFormat, AzureEndpoint, AzureResourceType, ProviderKind};
use sekisho_core::names::ProviderName;
use sekisho_core::secret_box::{SecretBox, SecretError};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;
use sqlx::postgres::PgRow;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::address::{check_destination, host_and_port};
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
    /// One of `kind.formats()`; fixed once the provider is added.
    pub format: ApiFormat,
    /// Required for `OpenAiCompatible`; otherwise overrides the provider's
    /// default endpoint.
    pub base_url: Option<String>,
    pub options: ProviderOptions,
    /// Sealed when stored and never returned by listings.
    pub credential: ProviderCredential,
    /// Lets the base URL be on a loopback or private network address, or a
    /// host the server can't resolve.
    pub allow_private_network: bool,
}

impl NewProvider {
    /// A provider of a single-format kind with an API key, no settings and
    /// no private network.
    pub fn keyed(
        name: ProviderName,
        kind: ProviderKind,
        base_url: Option<String>,
        key: impl Into<String>,
    ) -> NewProvider {
        NewProvider {
            name,
            kind,
            format: kind.formats()[0],
            base_url,
            options: ProviderOptions::None,
            credential: ProviderCredential::Key(key.into()),
            allow_private_network: false,
        }
    }
}

/// Changes to a provider; `None` leaves a field as it is. The name, kind and
/// format never change.
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
}

/// A provider as listed. Its credential is read only through
/// [`Store::provider_credential`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    pub id: Uuid,
    pub name: ProviderName,
    pub kind: ProviderKind,
    pub format: ApiFormat,
    pub base_url: Option<String>,
    pub options: ProviderOptions,
    pub credential: CredentialKind,
    pub allow_private_network: bool,
    pub created_at: OffsetDateTime,
}

/// Expands to the provider columns, for building fixed SQL with `concat!`.
macro_rules! columns {
    () => {
        "id, name, kind, api_format, base_url, options::text as options, credential_kind, \
         allow_private_network, created_at"
    };
}

/// What a provider will be once a create or an update is saved: everything
/// the validation rules look at.
struct Settings<'a> {
    kind: ProviderKind,
    format: ApiFormat,
    base_url: Option<&'a str>,
    options: &'a ProviderOptions,
    credential: CredentialKind,
    allow_private_network: bool,
}

const AWS_HOSTS: [&str; 2] = [".amazonaws.com", ".api.aws"];
const AZURE_HOSTS: [&str; 3] = [
    ".openai.azure.com",
    ".services.ai.azure.com",
    ".cognitiveservices.azure.com",
];

/// The shapes each kind takes. Foundry takes Claude API keys only in
/// `x-api-key`, which agentgateway's Azure provider can't send, so Claude
/// on Azure needs an Entra credential.
fn allowed_credentials(kind: ProviderKind, format: ApiFormat) -> &'static [CredentialKind] {
    use CredentialKind::*;
    match kind {
        ProviderKind::Bedrock => &[Key, AwsAccessKeys, CloudIdentity],
        ProviderKind::Azure if format == ApiFormat::Anthropic => {
            &[AzureClientSecret, CloudIdentity]
        }
        ProviderKind::Azure => &[Key, AzureClientSecret, CloudIdentity],
        ProviderKind::Anthropic
        | ProviderKind::OpenAi
        | ProviderKind::Gemini
        | ProviderKind::OpenAiCompatible => &[Key],
    }
}

/// Checks a provider as it will be saved; `new_credential` is the credential
/// being set, if any. The address check resolves the base URL's host, so this
/// runs before any transaction. Errors come in a fixed order: `format`,
/// `credential`, `region`, `endpoint`, `base_url`, then the credential's own
/// fields.
async fn validate(
    s: &Settings<'_>,
    new_credential: Option<&ProviderCredential>,
) -> Vec<FieldError> {
    let mut errors = Vec::new();
    if !s.kind.formats().contains(&s.format) {
        let formats: Vec<_> = s.kind.formats().iter().map(|f| f.as_str()).collect();
        errors.push(FieldError::new(
            "format",
            format!(
                "a {} provider serves {}",
                s.kind.as_str(),
                formats.join(" or ")
            ),
        ));
    }
    let allowed = allowed_credentials(s.kind, s.format);
    let fits = allowed.contains(&s.credential);
    if !fits {
        let shapes: Vec<_> = allowed.iter().map(|k| k.described()).collect();
        let what = match (s.kind, s.format) {
            (ProviderKind::Azure, ApiFormat::Anthropic) => {
                "Claude on Azure (Foundry takes Claude API keys only in a header agentgateway can't send)".to_owned()
            }
            (kind, _) => format!("a {} provider", kind.as_str()),
        };
        errors.push(FieldError::new(
            "credential",
            format!("{what} takes {}", shapes.join(" or ")),
        ));
    }
    validate_options(&mut errors, s);
    validate_base_url(&mut errors, s).await;
    if let Some(credential) = new_credential.filter(|_| fits) {
        validate_credential(&mut errors, credential);
    }
    errors
}

fn validate_options(errors: &mut Vec<FieldError>, s: &Settings<'_>) {
    const FOUNDRY: &str = "Claude on Azure is served by Foundry: use \
         https://<resource>.services.ai.azure.com/api/projects/<project>";
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
                Ok(parsed) if parsed == *endpoint => {
                    if s.format == ApiFormat::Anthropic
                        && endpoint.resource_type != AzureResourceType::Foundry
                    {
                        errors.push(FieldError::new("endpoint", FOUNDRY));
                    }
                }
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
    if !is_http_url(url) {
        errors.push(FieldError::new(
            "base_url",
            "use an http:// or https:// URL with a host",
        ));
        return;
    }
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
        if !hosts.is_empty() && !is_cloud_host(url, hosts) {
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
    if let Err(message) = check_destination(url, s.allow_private_network).await {
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

fn is_http_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    rest.is_some_and(|r| !r.is_empty() && !r.starts_with('/'))
        && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// `https://` on a host ending in one of `suffixes`, ignoring case. The host
/// is split as the gateway renderer splits it.
fn is_cloud_host(url: &str, suffixes: &[&str]) -> bool {
    let Some((host, _)) = url
        .starts_with("https://")
        .then(|| host_and_port(url))
        .flatten()
    else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    host.bytes()
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

fn options_from_json(kind: ProviderKind, text: &str) -> Result<ProviderOptions> {
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

fn from_row(row: &PgRow) -> Result<Provider> {
    let name: String = row.try_get("name")?;
    let kind: String = row.try_get("kind")?;
    let kind = ProviderKind::parse(&kind).ok_or_else(|| bad_column("provider kind", &kind))?;
    let format: String = row.try_get("api_format")?;
    let credential: String = row.try_get("credential_kind")?;
    let options: String = row.try_get("options")?;
    Ok(Provider {
        id: row.try_get("id")?,
        name: ProviderName::parse(&name).map_err(|_| bad_column("provider name", &name))?,
        kind,
        format: ApiFormat::parse(&format).ok_or_else(|| bad_column("API format", &format))?,
        base_url: row.try_get("base_url")?,
        options: options_from_json(kind, &options)?,
        credential: CredentialKind::parse(&credential)
            .ok_or_else(|| bad_column("credential kind", &credential))?,
        allow_private_network: row.try_get("allow_private_network")?,
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
            format: new.format,
            base_url: new.base_url.as_deref(),
            options: &new.options,
            credential: new.credential.kind(),
            allow_private_network: new.allow_private_network,
        };
        check(validate(&settings, Some(&new.credential)).await)?;
        let ciphertext = seal(secret_box, &new.credential);
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(concat!(
            "insert into providers (name, kind, api_format, base_url, options, \
             credential_kind, credential_ciphertext, allow_private_network) \
             values ($1, $2, $3, $4, $5::jsonb, $6, $7, $8) returning ",
            columns!()
        ))
        .bind(new.name.as_str())
        .bind(new.kind.as_str())
        .bind(new.format.as_str())
        .bind(&new.base_url)
        .bind(options_json(&new.options))
        .bind(new.credential.kind().as_str())
        .bind(&ciphertext)
        .bind(new.allow_private_network)
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
    /// its base URL, settings or `allow_private_network`. The result is
    /// validated as `create_provider` validates a new provider. A change to
    /// where requests go needs the credential again while a secret is
    /// stored. Its models and environments keep using it; the gateway config
    /// is re-rendered.
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
        let settings = Settings {
            kind: current.kind,
            format: current.format,
            base_url: base_url.as_deref(),
            options: &options,
            credential: update
                .credential
                .as_ref()
                .map_or(current.credential, ProviderCredential::kind),
            allow_private_network: update
                .allow_private_network
                .unwrap_or(current.allow_private_network),
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
        let row = sqlx::query(concat!(
            "update providers set base_url = $2, options = $3::jsonb, \
             allow_private_network = $4, \
             credential_kind = coalesce($5, credential_kind), \
             credential_ciphertext = case when $5::text is null \
             then credential_ciphertext else $6 end \
             where id = $1 returning ",
            columns!()
        ))
        .bind(current.id)
        .bind(&base_url)
        .bind(options_json(&options))
        .bind(settings.allow_private_network)
        .bind(credential.as_ref().map(|(kind, _)| *kind))
        .bind(credential.and_then(|(_, sealed)| sealed))
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
