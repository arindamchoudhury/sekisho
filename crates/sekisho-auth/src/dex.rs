//! Sekisho's client for Dex: sign-in and refresh over Dex's OAuth endpoints,
//! and passwords, connectors, refresh tokens and Sekisho's own client
//! registration over Dex's gRPC API.
//!
//! Every call has a [`DEX_TIMEOUT`]. [`DexError::Refused`] means Dex refused
//! the person (its token endpoint answered 400). Dex not answering is
//! [`DexError::Unreachable`]; an answer that is Dex's or Sekisho's own problem
//! is [`DexError::Unavailable`]. Neither ever ends a session.

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use sekisho_core::names::ConnectorId;
use sekisho_proto::dex as api;
use sekisho_proto::dex::dex_client::DexClient as Grpc;
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::sync::RwLock;
use tonic::transport::{Channel, Endpoint};
use url::Url;

/// How long Sekisho waits for any one answer from Dex.
pub const DEX_TIMEOUT: Duration = Duration::from_secs(10);
/// The scopes Sekisho asks Dex for: `offline_access` for a refresh token,
/// `federated:id` for the connector and the user's ID there.
pub const SCOPES: &str = "openid email profile groups offline_access federated:id";

/// How Sekisho reaches Dex, and its client registration there.
#[derive(Clone)]
pub struct DexConfig {
    /// Dex's issuer, as browsers reach it (`<issuer>/auth`).
    pub issuer: Url,
    /// Where Sekisho reaches Dex's HTTP: `<http_url><issuer path>/{token,keys}`.
    pub http_url: Url,
    /// Dex's gRPC API, `host:port`, plain HTTP/2 on the private network.
    pub grpc: String,
    pub client_id: String,
    pub client_secret: String,
    /// Sekisho's `/oauth/callback`.
    pub redirect_uri: Url,
}

impl fmt::Debug for DexConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DexConfig")
            .field("issuer", &self.issuer.as_str())
            .field("http_url", &self.http_url.as_str())
            .field("grpc", &self.grpc)
            .field("client_id", &self.client_id)
            .field("redirect_uri", &self.redirect_uri.as_str())
            .finish_non_exhaustive()
    }
}

/// Who Dex says signed in.
#[derive(Clone, PartialEq, Eq)]
pub struct DexIdentity {
    /// Dex's `sub`: the connector and the user's ID there, encoded.
    pub subject: String,
    pub email: String,
    pub email_verified: bool,
    pub name: String,
    /// Dex's `groups` claim, from the connector.
    pub groups: Vec<String>,
    pub connector_id: String,
    /// The user's ID at the connector; for a local account, the user ID
    /// Sekisho gave its password entry.
    pub connector_user_id: String,
    /// Dex's refresh token, rotated by every refresh.
    pub refresh_token: Option<String>,
}

/// Shows everything but the refresh token.
impl fmt::Debug for DexIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DexIdentity")
            .field("subject", &self.subject)
            .field("email", &self.email)
            .field("email_verified", &self.email_verified)
            .field("name", &self.name)
            .field("groups", &self.groups)
            .field("connector_id", &self.connector_id)
            .field("connector_user_id", &self.connector_user_id)
            .field("has_refresh_token", &self.refresh_token.is_some())
            .finish()
    }
}

/// A local account's entry in Dex's password database.
#[derive(Clone)]
pub struct DexPassword {
    pub email: String,
    /// bcrypt, cost 12. Sekisho never stores it.
    pub bcrypt_hash: String,
    pub username: String,
    pub user_id: String,
}

/// Shows everything but the hash.
impl fmt::Debug for DexPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DexPassword")
            .field("email", &self.email)
            .field("username", &self.username)
            .field("user_id", &self.user_id)
            .finish_non_exhaustive()
    }
}

/// An identity provider's connector at Dex. Its config holds secrets, so it
/// is written but never read back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DexConnector {
    pub id: ConnectorId,
    /// Dex's connector type, such as `github` or `oidc`.
    pub kind: String,
    pub name: String,
}

/// Why a call to Dex failed. The messages never carry tokens or secrets.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DexError {
    /// Dex refused the person: its token endpoint answered 400
    /// (`invalid_grant`, `invalid_request`).
    #[error("Dex refused: {0}")]
    Refused(String),
    /// Dex answered, but failed on its side (5xx, which in v2.46.1 includes
    /// a connector's refusal, or a 4xx that is about Sekisho's client rather
    /// than the person).
    #[error("Dex is unavailable: {0}")]
    Unavailable(String),
    /// Dex couldn't be reached or didn't answer within [`DEX_TIMEOUT`].
    #[error("Dex is unreachable: {0}")]
    Unreachable(String),
    /// Dex's answer failed Sekisho's checks, or Dex refused a management call.
    #[error("Dex's answer is invalid: {0}")]
    Invalid(String),
}

/// What Sekisho asks of Dex.
#[async_trait]
pub trait Dex: Send + Sync {
    /// Where to send a browser to sign in, with PKCE `S256`.
    fn authorize_url(&self, state: &str, nonce: &str, code_challenge: &str) -> Url;
    /// Trades an authorisation code for the identity and a refresh token.
    async fn sign_in(
        &self,
        code: &str,
        code_verifier: &str,
        nonce: &str,
    ) -> Result<DexIdentity, DexError>;
    /// Replays a refresh token: Dex asks the connector again and rotates it.
    async fn refresh(&self, refresh_token: &str) -> Result<DexIdentity, DexError>;
    /// Creates a local account's password entry, or updates it.
    async fn put_password(&self, p: &DexPassword) -> Result<(), DexError>;
    /// Deletes a local account's password entry; an absent one is fine.
    async fn delete_password(&self, email: &str) -> Result<(), DexError>;
    /// Revokes the refresh token Dex holds for this subject and Sekisho's
    /// client; an absent one is fine.
    async fn revoke_refresh(&self, subject: &str) -> Result<(), DexError>;
    /// The connectors at Dex, without Dex's own `local`.
    async fn connectors(&self) -> Result<Vec<DexConnector>, DexError>;
    /// Creates a connector, or updates it.
    async fn put_connector(
        &self,
        c: &DexConnector,
        config: &serde_json::Value,
    ) -> Result<(), DexError>;
    /// Deletes a connector; `true` if it existed.
    async fn delete_connector(&self, id: &ConnectorId) -> Result<bool, DexError>;
    /// Registers Sekisho's client with its secret and redirect URI.
    async fn ensure_client(&self) -> Result<(), DexError>;
}

/// What an ID token must say.
#[derive(Clone, Debug)]
pub struct IdTokenCheck {
    pub issuer: String,
    pub client_id: String,
    /// The sign-in's nonce; `None` for a refresh's token.
    pub nonce: Option<String>,
}

/// The claims Sekisho reads from Dex's ID tokens.
#[derive(Clone, Debug, Deserialize)]
pub struct IdTokenClaims {
    pub sub: String,
    pub exp: i64,
    #[serde(default)]
    pub nonce: Option<String>,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub email_verified: bool,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub groups: Vec<String>,
    /// Present when the `federated:id` scope was asked for.
    #[serde(default)]
    pub federated_claims: Option<FederatedClaims>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct FederatedClaims {
    pub connector_id: String,
    pub user_id: String,
}

/// Checks a Dex ID token: signed by the key whose `kid` it names, with the
/// algorithm that key names; `iss` exact; `aud` containing the client; the
/// nonce, if one is expected; `exp` after `now`.
pub fn verify_id_token(
    token: &str,
    keys: &JwkSet,
    expect: &IdTokenCheck,
    now: OffsetDateTime,
) -> Result<IdTokenClaims, DexError> {
    let invalid = |why: &str| DexError::Invalid(format!("ID token {why}"));
    let header = jsonwebtoken::decode_header(token).map_err(|_| invalid("is malformed"))?;
    let kid = header.kid.ok_or_else(|| invalid("names no key"))?;
    let jwk = keys
        .find(&kid)
        .ok_or_else(|| invalid("is signed by an unknown key"))?;
    let algorithm = jwk
        .common
        .key_algorithm
        .and_then(|a| Algorithm::try_from(a).ok())
        .ok_or_else(|| invalid("is signed by a key that names no algorithm"))?;
    if header.alg != algorithm {
        return Err(invalid("uses another algorithm than its key"));
    }
    let key = DecodingKey::from_jwk(jwk).map_err(|_| invalid("is signed by an unusable key"))?;
    let mut validation = Validation::new(algorithm);
    validation.leeway = 0;
    // Checked below against `now`, not the system clock.
    validation.validate_exp = false;
    validation.set_issuer(&[&expect.issuer]);
    validation.set_audience(&[&expect.client_id]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    let claims = jsonwebtoken::decode::<IdTokenClaims>(token, &key, &validation)
        .map_err(|_| invalid("failed its signature, issuer or audience check"))?
        .claims;
    if let Some(nonce) = &expect.nonce
        && claims.nonce.as_deref() != Some(nonce.as_str())
    {
        return Err(invalid("has another nonce"));
    }
    if claims.exp <= now.unix_timestamp() {
        return Err(invalid("has expired"));
    }
    Ok(claims)
}

/// Dex's endpoint `name` under the issuer's path, on `http_url`.
fn endpoint(config: &DexConfig, name: &str) -> Url {
    let mut url = config.http_url.clone();
    url.set_path(&format!(
        "{}/{name}",
        config.issuer.path().trim_end_matches('/')
    ));
    url.set_query(None);
    url
}

/// What an error answer from Dex's token endpoint means. Dex answers 400 for
/// a code or refresh token it refuses; 401 is a wrong client secret, which is
/// Sekisho's problem; 5xx is Dex's (in v2.46.1 that includes a connector
/// failing to refresh, which Dex doesn't tell apart from its own failures).
fn token_error(status: u16, body: &[u8]) -> DexError {
    #[derive(Deserialize)]
    struct OAuthError {
        error: String,
        #[serde(default)]
        error_description: String,
    }
    let said = serde_json::from_slice::<OAuthError>(body)
        .map(|e| {
            if e.error_description.is_empty() {
                e.error
            } else {
                format!("{}: {}", e.error, e.error_description)
            }
        })
        .unwrap_or_else(|_| "no OAuth error in the answer".to_owned());
    let said: String = said.chars().take(200).collect();
    if status == 400 {
        DexError::Refused(said)
    } else {
        DexError::Unavailable(format!("HTTP {status} from the token endpoint: {said}"))
    }
}

/// What a failed gRPC call means. Dex answers its own refusals (a missing
/// field, a feature flag that is off) as `Unknown`; tonic reports a
/// connection that fails as `Unavailable`.
fn grpc_error(status: &tonic::Status) -> DexError {
    use tonic::Code;
    let said = format!("gRPC {:?}: {}", status.code(), status.message());
    match status.code() {
        Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled => DexError::Unreachable(said),
        Code::ResourceExhausted | Code::Aborted | Code::Internal => DexError::Unavailable(said),
        _ => DexError::Invalid(said),
    }
}

/// An error and its causes, for a message that says what went wrong.
fn chain(e: &dyn std::error::Error) -> String {
    let mut said = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        said.push_str(": ");
        said.push_str(&c.to_string());
        cause = c.source();
    }
    said
}

/// RFC 6749 section 2.3.1: client credentials are form-encoded before Basic
/// authentication.
fn form_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Sekisho's [`Dex`], over Dex's HTTP endpoints and gRPC API.
pub struct DexClient {
    config: DexConfig,
    http: reqwest::Client,
    grpc: Result<Grpc<Channel>, String>,
    /// Dex's signing keys, fetched once and again on an unknown `kid`.
    keys: RwLock<Option<Arc<JwkSet>>>,
}

impl DexClient {
    /// The gRPC channel connects on first use, so this needs no Dex, but it
    /// must be called inside a Tokio runtime.
    pub fn new(config: DexConfig) -> Self {
        let grpc = Endpoint::from_shared(format!("http://{}", config.grpc))
            .map(|e| {
                Grpc::new(
                    e.connect_timeout(DEX_TIMEOUT)
                        .timeout(DEX_TIMEOUT)
                        .connect_lazy(),
                )
            })
            .map_err(|_| format!("Dex's gRPC address {:?} is not host:port", config.grpc));
        let http = reqwest::Client::builder()
            .timeout(DEX_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("an HTTP client without TLS always builds");
        Self {
            config,
            http,
            grpc,
            keys: RwLock::new(None),
        }
    }

    /// One gRPC call, with Dex's timeout.
    async fn call<T, F, Fut>(&self, f: F) -> Result<T, DexError>
    where
        F: FnOnce(Grpc<Channel>) -> Fut,
        Fut: Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    {
        let client = self.grpc.clone().map_err(DexError::Invalid)?;
        match tokio::time::timeout(DEX_TIMEOUT, f(client)).await {
            Err(_) => Err(DexError::Unreachable("gRPC call timed out".to_owned())),
            Ok(Err(status)) => Err(grpc_error(&status)),
            Ok(Ok(response)) => Ok(response.into_inner()),
        }
    }

    /// One call to Dex's token endpoint, and the identity in its answer.
    async fn token(
        &self,
        form: &[(&str, &str)],
        nonce: Option<&str>,
    ) -> Result<DexIdentity, DexError> {
        let response = self
            .http
            .post(endpoint(&self.config, "token"))
            .basic_auth(
                form_encode(&self.config.client_id),
                Some(form_encode(&self.config.client_secret)),
            )
            .form(form)
            .send()
            .await
            .map_err(|e| DexError::Unreachable(format!("token endpoint: {}", chain(&e))))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|e| DexError::Unreachable(format!("token endpoint: {e}")))?;
        if !status.is_success() {
            return Err(token_error(status.as_u16(), &body));
        }
        let tokens: TokenResponse = serde_json::from_slice(&body).map_err(|_| {
            DexError::Invalid("the token endpoint answered without an ID token".to_owned())
        })?;
        let claims = self.id_token(&tokens.id_token, nonce).await?;
        let federated = claims
            .federated_claims
            .ok_or_else(|| DexError::Invalid("ID token has no federated_claims".to_owned()))?;
        Ok(DexIdentity {
            subject: claims.sub,
            email: claims.email,
            email_verified: claims.email_verified,
            name: claims.name,
            groups: claims.groups,
            connector_id: federated.connector_id,
            connector_user_id: federated.user_id,
            refresh_token: tokens.refresh_token,
        })
    }

    async fn id_token(&self, raw: &str, nonce: Option<&str>) -> Result<IdTokenClaims, DexError> {
        let check = IdTokenCheck {
            // Dex's `iss` is its configured issuer, which has no trailing slash.
            issuer: self.config.issuer.as_str().trim_end_matches('/').to_owned(),
            client_id: self.config.client_id.clone(),
            nonce: nonce.map(str::to_owned),
        };
        let kid = jsonwebtoken::decode_header(raw).ok().and_then(|h| h.kid);
        let cached = self.keys.read().await.clone();
        let mut keys = match cached {
            Some(keys) => keys,
            None => self.fetch_keys().await?,
        };
        if kid.as_deref().is_some_and(|kid| keys.find(kid).is_none()) {
            keys = self.fetch_keys().await?;
        }
        verify_id_token(raw, &keys, &check, OffsetDateTime::now_utc())
    }

    async fn fetch_keys(&self) -> Result<Arc<JwkSet>, DexError> {
        let unreachable = |e: reqwest::Error| DexError::Unreachable(format!("keys endpoint: {e}"));
        let response = self
            .http
            .get(endpoint(&self.config, "keys"))
            .send()
            .await
            .map_err(unreachable)?;
        let status = response.status();
        if !status.is_success() {
            return Err(DexError::Unavailable(format!(
                "HTTP {status} from the keys endpoint"
            )));
        }
        let body = response.bytes().await.map_err(unreachable)?;
        let keys: JwkSet = serde_json::from_slice(&body).map_err(|_| {
            DexError::Invalid("the keys endpoint answered without a JWK set".to_owned())
        })?;
        let keys = Arc::new(keys);
        *self.keys.write().await = Some(keys.clone());
        Ok(keys)
    }

    fn client(&self) -> api::Client {
        api::Client {
            id: self.config.client_id.clone(),
            secret: self.config.client_secret.clone(),
            redirect_uris: vec![self.config.redirect_uri.to_string()],
            name: "Sekisho".to_owned(),
            ..Default::default()
        }
    }
}

#[async_trait]
impl Dex for DexClient {
    fn authorize_url(&self, state: &str, nonce: &str, code_challenge: &str) -> Url {
        let mut url = self.config.issuer.clone();
        url.set_path(&format!(
            "{}/auth",
            self.config.issuer.path().trim_end_matches('/')
        ));
        url.query_pairs_mut()
            .clear()
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", self.config.redirect_uri.as_str())
            .append_pair("response_type", "code")
            .append_pair("scope", SCOPES)
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("code_challenge", code_challenge)
            .append_pair("code_challenge_method", "S256");
        url
    }

    async fn sign_in(
        &self,
        code: &str,
        code_verifier: &str,
        nonce: &str,
    ) -> Result<DexIdentity, DexError> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", self.config.redirect_uri.as_str()),
            ("code_verifier", code_verifier),
        ];
        self.token(&form, Some(nonce)).await
    }

    async fn refresh(&self, refresh_token: &str) -> Result<DexIdentity, DexError> {
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ];
        self.token(&form, None).await
    }

    async fn put_password(&self, p: &DexPassword) -> Result<(), DexError> {
        let create = api::CreatePasswordReq {
            password: Some(api::Password {
                email: p.email.clone(),
                hash: p.bcrypt_hash.as_bytes().to_vec(),
                username: p.username.clone(),
                user_id: p.user_id.clone(),
                groups: vec![],
            }),
        };
        let created = self
            .call(move |mut c| async move { c.create_password(create).await })
            .await?;
        if !created.already_exists {
            return Ok(());
        }
        let update = api::UpdatePasswordReq {
            email: p.email.clone(),
            new_hash: p.bcrypt_hash.as_bytes().to_vec(),
            new_username: p.username.clone(),
            new_groups: vec![],
        };
        let updated = self
            .call(move |mut c| async move { c.update_password(update).await })
            .await?;
        if updated.not_found {
            return Err(DexError::Invalid(
                "the password entry vanished while it was updated".to_owned(),
            ));
        }
        Ok(())
    }

    async fn delete_password(&self, email: &str) -> Result<(), DexError> {
        let req = api::DeletePasswordReq {
            email: email.to_owned(),
        };
        self.call(move |mut c| async move { c.delete_password(req).await })
            .await?;
        Ok(())
    }

    async fn revoke_refresh(&self, subject: &str) -> Result<(), DexError> {
        let req = api::RevokeRefreshReq {
            user_id: subject.to_owned(),
            client_id: self.config.client_id.clone(),
        };
        self.call(move |mut c| async move { c.revoke_refresh(req).await })
            .await?;
        Ok(())
    }

    async fn connectors(&self) -> Result<Vec<DexConnector>, DexError> {
        let listed = self
            .call(|mut c| async move { c.list_connectors(api::ListConnectorReq {}).await })
            .await?;
        // `local` (Dex's password database) and any ID Sekisho wouldn't make
        // aren't Sekisho's to manage.
        let mut connectors: Vec<DexConnector> = listed
            .connectors
            .into_iter()
            .filter_map(|c| {
                Some(DexConnector {
                    id: ConnectorId::parse(&c.id).ok()?,
                    kind: c.r#type,
                    name: c.name,
                })
            })
            .collect();
        connectors.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
        Ok(connectors)
    }

    async fn put_connector(
        &self,
        c: &DexConnector,
        config: &serde_json::Value,
    ) -> Result<(), DexError> {
        let config = serde_json::to_vec(config).expect("a JSON value always serializes");
        let create = api::CreateConnectorReq {
            connector: Some(api::Connector {
                id: c.id.as_str().to_owned(),
                r#type: c.kind.clone(),
                name: c.name.clone(),
                config: config.clone(),
                grant_types: vec![],
            }),
        };
        let created = self
            .call(move |mut g| async move { g.create_connector(create).await })
            .await?;
        if !created.already_exists {
            return Ok(());
        }
        let update = api::UpdateConnectorReq {
            id: c.id.as_str().to_owned(),
            new_type: c.kind.clone(),
            new_name: c.name.clone(),
            new_config: config,
            new_grant_types: None,
        };
        let updated = self
            .call(move |mut g| async move { g.update_connector(update).await })
            .await?;
        if updated.not_found {
            return Err(DexError::Invalid(
                "the connector vanished while it was updated".to_owned(),
            ));
        }
        Ok(())
    }

    async fn delete_connector(&self, id: &ConnectorId) -> Result<bool, DexError> {
        let req = api::DeleteConnectorReq {
            id: id.as_str().to_owned(),
        };
        let deleted = self
            .call(move |mut c| async move { c.delete_connector(req).await })
            .await?;
        Ok(!deleted.not_found)
    }

    /// Dex can't change a client's secret, so a client with another secret is
    /// deleted and created again; other redirect URIs are updated in place.
    async fn ensure_client(&self) -> Result<(), DexError> {
        let create = api::CreateClientReq {
            client: Some(self.client()),
        };
        let created = self
            .call(move |mut c| async move { c.create_client(create).await })
            .await?;
        if !created.already_exists {
            return Ok(());
        }
        let get = api::GetClientReq {
            id: self.config.client_id.clone(),
        };
        let existing = self
            .call(move |mut c| async move { c.get_client(get).await })
            .await?
            .client
            .ok_or_else(|| DexError::Invalid("Dex answered without the client".to_owned()))?;
        let wanted = self.client();
        if existing.secret != wanted.secret {
            let delete = api::DeleteClientReq {
                id: wanted.id.clone(),
            };
            self.call(move |mut c| async move { c.delete_client(delete).await })
                .await?;
            let create = api::CreateClientReq {
                client: Some(wanted),
            };
            let created = self
                .call(move |mut c| async move { c.create_client(create).await })
                .await?;
            if created.already_exists {
                return Err(DexError::Invalid(
                    "Sekisho's client was registered again while it was replaced".to_owned(),
                ));
            }
            return Ok(());
        }
        if existing.redirect_uris != wanted.redirect_uris {
            let update = api::UpdateClientReq {
                id: wanted.id,
                redirect_uris: wanted.redirect_uris,
                ..Default::default()
            };
            let updated = self
                .call(move |mut c| async move { c.update_client(update).await })
                .await?;
            if updated.not_found {
                return Err(DexError::Invalid(
                    "Sekisho's client vanished while it was updated".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{Algorithm, EncodingKey, Header};
    use sekisho_core::session_token::SigningKey;
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use time::macros::datetime;

    const ISSUER: &str = "http://dex.test:5556/dex";
    const CLIENT: &str = "sekisho";
    /// PKCS#8 v1 DER of an Ed25519 key up to its seed.
    const PKCS8_PREFIX: [u8; 16] = [
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];

    fn now() -> OffsetDateTime {
        datetime!(2026-11-12 12:00 UTC)
    }

    fn jwks(keys: &[&SigningKey]) -> JwkSet {
        let keys: Vec<Value> = keys.iter().map(|k| k.public().to_jwk()).collect();
        serde_json::from_value(json!({ "keys": keys })).unwrap()
    }

    fn sign(key: &SigningKey, claims: &Value) -> String {
        let mut der = PKCS8_PREFIX.to_vec();
        der.extend_from_slice(key.seed());
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(key.kid().to_owned());
        jsonwebtoken::encode(&header, claims, &EncodingKey::from_ed_der(&der)).unwrap()
    }

    /// An ID token as Dex v2.46.1 writes it for a local account.
    fn good() -> Value {
        let now = now().unix_timestamp();
        json!({
            "iss": ISSUER,
            "sub": "CgV1LWFkYRIFbG9jYWw",
            "aud": CLIENT,
            "exp": now + 3600,
            "iat": now,
            "nonce": "n",
            "at_hash": "x",
            "email": "ada@example.com",
            "email_verified": true,
            "name": "ada",
            "groups": ["eng"],
            "federated_claims": {"connector_id": "local", "user_id": "u-ada"},
        })
    }

    fn check(nonce: Option<&str>) -> IdTokenCheck {
        IdTokenCheck {
            issuer: ISSUER.into(),
            client_id: CLIENT.into(),
            nonce: nonce.map(Into::into),
        }
    }

    fn with(mut claims: Value, field: &str, value: Value) -> Value {
        claims[field] = value;
        claims
    }

    #[test]
    fn id_token_checks() {
        let key = SigningKey::generate();
        let keys = jwks(&[&key]);
        let c = verify_id_token(&sign(&key, &good()), &keys, &check(Some("n")), now()).unwrap();
        assert_eq!(c.sub, "CgV1LWFkYRIFbG9jYWw");
        assert_eq!(
            (c.email.as_str(), c.email_verified),
            ("ada@example.com", true)
        );
        assert_eq!(
            (c.name.as_str(), c.groups.clone()),
            ("ada", vec!["eng".to_owned()])
        );
        let fed = c.federated_claims.unwrap();
        assert_eq!(
            (fed.connector_id.as_str(), fed.user_id.as_str()),
            ("local", "u-ada")
        );

        // An audience list naming the client, and a refresh's token, whose
        // nonce isn't checked, are fine too.
        let listed = with(good(), "aud", json!(["other", CLIENT]));
        assert!(verify_id_token(&sign(&key, &listed), &keys, &check(Some("n")), now()).is_ok());
        assert!(verify_id_token(&sign(&key, &good()), &keys, &check(None), now()).is_ok());

        let same_kid = SigningKey::from_seed(key.kid().to_owned(), [9; 32]);
        let mut no_nonce = good();
        no_nonce.as_object_mut().unwrap().remove("nonce");
        let hs256 = {
            let mut header = Header::new(Algorithm::HS256);
            header.kid = Some(key.kid().to_owned());
            jsonwebtoken::encode(&header, &good(), &EncodingKey::from_secret(&key.public().x))
                .unwrap()
        };
        let cases = [
            (
                "wrong iss",
                sign(&key, &with(good(), "iss", json!("http://evil.test/dex"))),
            ),
            (
                "wrong aud",
                sign(&key, &with(good(), "aud", json!("other"))),
            ),
            (
                "wrong nonce",
                sign(&key, &with(good(), "nonce", json!("m"))),
            ),
            ("no nonce", sign(&key, &no_nonce)),
            (
                "exp == now",
                sign(&key, &with(good(), "exp", json!(now().unix_timestamp()))),
            ),
            ("unknown kid", sign(&SigningKey::generate(), &good())),
            ("bad signature", sign(&same_kid, &good())),
            ("HS256 keyed with the public key", hs256),
            ("garbage", "garbage".to_owned()),
        ];
        for (what, token) in cases {
            let r = verify_id_token(&token, &keys, &check(Some("n")), now());
            assert!(matches!(r, Err(DexError::Invalid(_))), "{what}: {r:?}");
        }
    }

    #[test]
    fn only_a_400_from_the_token_endpoint_is_a_refusal() {
        let body = br#"{"error":"invalid_grant","error_description":"Refresh token is invalid or has already been claimed by another client."}"#;
        let refused = token_error(400, body);
        assert!(
            matches!(&refused, DexError::Refused(m) if m.contains("invalid_grant")),
            "{refused:?}"
        );
        assert!(matches!(
            token_error(400, b"not json"),
            DexError::Refused(_)
        ));
        // A wrong client secret (401), a proxy's refusal or Dex's own failure
        // is Sekisho's or Dex's problem, never the person's.
        for status in [401, 403, 404, 429, 500, 502, 503, 504] {
            let e = token_error(status, br#"{"error":"invalid_request"}"#);
            assert!(matches!(e, DexError::Unavailable(_)), "{status}: {e:?}");
        }
    }

    fn config() -> DexConfig {
        DexConfig {
            issuer: Url::parse(ISSUER).unwrap(),
            http_url: Url::parse("http://127.0.0.1:15556").unwrap(),
            grpc: "127.0.0.1:15557".into(),
            client_id: CLIENT.into(),
            client_secret: "client-secret-value".into(),
            redirect_uri: Url::parse("https://sekisho.test/oauth/callback").unwrap(),
        }
    }

    #[tokio::test]
    async fn the_authorize_url_asks_for_pkce_and_federated_ids() {
        let dex = DexClient::new(config());
        let url = dex.authorize_url("st", "no", "ch");
        assert_eq!(
            url.as_str().split('?').next(),
            Some("http://dex.test:5556/dex/auth")
        );
        let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(q.len(), 8, "{q:?}");
        assert_eq!(q["client_id"], CLIENT);
        assert_eq!(q["redirect_uri"], "https://sekisho.test/oauth/callback");
        assert_eq!(q["response_type"], "code");
        assert_eq!(
            q["scope"],
            "openid email profile groups offline_access federated:id"
        );
        assert_eq!((q["state"].as_str(), q["nonce"].as_str()), ("st", "no"));
        assert_eq!(q["code_challenge"], "ch");
        assert_eq!(q["code_challenge_method"], "S256");
    }

    #[test]
    fn dex_endpoints_are_on_the_http_url_under_the_issuer_path() {
        let c = config();
        assert_eq!(
            endpoint(&c, "token").as_str(),
            "http://127.0.0.1:15556/dex/token"
        );
        assert_eq!(
            endpoint(&c, "keys").as_str(),
            "http://127.0.0.1:15556/dex/keys"
        );
    }

    #[test]
    fn secrets_stay_out_of_debug() {
        let shown = format!("{:?}", config());
        assert!(
            shown.contains(CLIENT) && !shown.contains("client-secret-value"),
            "{shown}"
        );
        let id = DexIdentity {
            subject: "s".into(),
            email: "ada@example.com".into(),
            email_verified: true,
            name: "ada".into(),
            groups: vec![],
            connector_id: "local".into(),
            connector_user_id: "u-ada".into(),
            refresh_token: Some("refresh-token-value".into()),
        };
        let shown = format!("{id:?}");
        assert!(
            shown.contains("u-ada") && !shown.contains("refresh-token-value"),
            "{shown}"
        );
        let p = DexPassword {
            email: "ada@example.com".into(),
            bcrypt_hash: "$2b$12$hash-value".into(),
            username: "ada".into(),
            user_id: "u-ada".into(),
        };
        let shown = format!("{p:?}");
        assert!(
            shown.contains("u-ada") && !shown.contains("hash-value"),
            "{shown}"
        );
    }
}
