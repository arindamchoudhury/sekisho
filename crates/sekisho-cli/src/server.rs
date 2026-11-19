//! The CLI's HTTP client for Sekisho. TLS is rustls with `ring` and the
//! platform's certificate verifier, as in `sekisho_server::catalog::outbound`
//! (D178: reqwest stays plain HTTP, so this is hyper). A redirect is an error
//! naming its status, never followed (D197). Errors never hold a token.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue, USER_AGENT};
use http::{Method, Request};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::ServerUrl;
use crate::credentials::Credential;
use crate::output::CliError;

/// Each request's deadline, connecting included.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// The most an answer may hold.
pub const LIMIT: usize = 64 << 20;

type Https = hyper_rustls::HttpsConnector<HttpConnector>;

/// Sekisho at one URL.
#[derive(Clone)]
pub struct Server {
    url: ServerUrl,
    client: Client<Https, Full<Bytes>>,
    user_agent: HeaderValue,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("url", &self.url.key())
            .finish()
    }
}

/// Sekisho's error answer: `{"error", "message"}` from its API, or RFC 6749's
/// `{"error", "error_description"}` from its OAuth endpoints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    pub status: u16,
    pub error: String,
    pub message: String,
}

/// Why a call failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    #[error("Sekisho answered {}: {}", .0.status, if .0.message.is_empty() { &.0.error } else { &.0.message })]
    Api(ApiError),
    #[error("Sekisho answered {0} with no explanation")]
    Status(u16),
    #[error(
        "Sekisho answered with a redirect ({0}), which the CLI never follows; check the server URL"
    )]
    Redirect(u16),
    #[error("can't reach Sekisho: {0}")]
    Transport(String),
    #[error("Sekisho didn't answer within {} seconds", TIMEOUT.as_secs())]
    Timeout,
    #[error("Sekisho's answer isn't what the CLI expects: {0}")]
    Decode(String),
    #[error("Sekisho's answer is larger than {} MiB", LIMIT >> 20)]
    TooLarge,
}

impl CallError {
    /// The OAuth or API error code, when Sekisho gave one.
    pub fn code(&self) -> Option<&str> {
        match self {
            CallError::Api(e) => Some(&e.error),
            _ => None,
        }
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            CallError::Api(e) => Some(e.status),
            CallError::Status(s) | CallError::Redirect(s) => Some(*s),
            _ => None,
        }
    }
}

impl From<CallError> for CliError {
    fn from(e: CallError) -> Self {
        match e.status() {
            Some(401) => CliError::not_signed_in(e.to_string()),
            _ => CliError::error(e.to_string()),
        }
    }
}

/// rustls with ring and the platform's certificate verifier, made once.
fn tls_config() -> Result<rustls::ClientConfig, String> {
    static CONFIG: OnceLock<Result<rustls::ClientConfig, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let verifier = rustls_platform_verifier::Verifier::new(provider.clone())
                .map_err(|e| format!("can't load the platform's certificates: {e}"))?;
            Ok(rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| format!("can't set up TLS: {e}"))?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth())
        })
        .clone()
}

/// An error and its causes.
fn describe(e: &(dyn std::error::Error + 'static)) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

impl Server {
    pub fn new(url: ServerUrl) -> Result<Self, CliError> {
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        http.set_connect_timeout(Some(TIMEOUT));
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls_config().map_err(CliError::error)?)
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        Ok(Self {
            url,
            client: Client::builder(TokioExecutor::new()).build(connector),
            user_agent: HeaderValue::from_str(&format!("sekisho/{}", env!("CARGO_PKG_VERSION")))
                .expect("a valid header"),
        })
    }

    pub fn url(&self) -> &ServerUrl {
        &self.url
    }

    pub async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        credential: Option<&Credential>,
    ) -> Result<T, CallError> {
        let body = self.send(Method::GET, path, credential, None).await?;
        decode(&body)
    }

    pub async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        credential: Option<&Credential>,
    ) -> Result<T, CallError> {
        let bytes = serde_json::to_vec(body).map_err(|e| CallError::Decode(e.to_string()))?;
        let answer = self
            .send(
                Method::POST,
                path,
                credential,
                Some(("application/json", bytes)),
            )
            .await?;
        decode(&answer)
    }

    /// A form `POST`, as OAuth's endpoints take; the answer as JSON (an
    /// empty answer reads as `null`).
    pub async fn post_form<T: DeserializeOwned>(
        &self,
        path: &str,
        form: &[(&str, &str)],
    ) -> Result<T, CallError> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        let answer = self
            .send(
                Method::POST,
                path,
                None,
                Some(("application/x-www-form-urlencoded", body.into_bytes())),
            )
            .await?;
        decode(&answer)
    }

    pub async fn get_bytes(
        &self,
        path: &str,
        credential: Option<&Credential>,
    ) -> Result<Vec<u8>, CallError> {
        self.send(Method::GET, path, credential, None).await
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        credential: Option<&Credential>,
        body: Option<(&'static str, Vec<u8>)>,
    ) -> Result<Vec<u8>, CallError> {
        let mut request = Request::builder()
            .method(method)
            .uri(self.url.join(path))
            .header(USER_AGENT, self.user_agent.clone())
            .header(ACCEPT, "application/json");
        if let Some(credential) = credential {
            let mut value = HeaderValue::from_str(&format!("Bearer {}", credential.secret()))
                .map_err(|_| CallError::Transport("the token can't be sent".to_owned()))?;
            value.set_sensitive(true);
            request = request.header(AUTHORIZATION, value);
        }
        let (content_type, bytes) = match body {
            Some((content_type, bytes)) => (Some(content_type), bytes),
            None => (None, Vec::new()),
        };
        if let Some(content_type) = content_type {
            request = request.header(CONTENT_TYPE, content_type);
        }
        let request = request
            .body(Full::new(Bytes::from(bytes)))
            .map_err(|_| CallError::Transport("the request can't be made".to_owned()))?;

        let exchange = async {
            let response = self
                .client
                .request(request)
                .await
                .map_err(|e| CallError::Transport(describe(&e)))?;
            let status = response.status().as_u16();
            let mut body = response.into_body();
            let mut read = Vec::new();
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|e| CallError::Transport(describe(&e)))?;
                if let Ok(chunk) = frame.into_data() {
                    if read.len() + chunk.len() > LIMIT {
                        return Err(CallError::TooLarge);
                    }
                    read.extend_from_slice(&chunk);
                }
            }
            Ok((status, read))
        };
        let (status, body) = tokio::time::timeout(TIMEOUT, exchange)
            .await
            .map_err(|_| CallError::Timeout)??;
        match status {
            200..=299 => Ok(body),
            300..=399 => Err(CallError::Redirect(status)),
            _ => Err(api_error(status, &body)),
        }
    }
}

fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, CallError> {
    let body: &[u8] = if body.is_empty() { b"null" } else { body };
    serde_json::from_slice(body).map_err(|e| CallError::Decode(e.to_string()))
}

/// Sekisho's error code and message, when the answer carries them.
fn api_error(status: u16, body: &[u8]) -> CallError {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return CallError::Status(status);
    };
    let Some(error) = value["error"].as_str() else {
        return CallError::Status(status);
    };
    let message = value["message"]
        .as_str()
        .or_else(|| value["error_description"].as_str())
        .unwrap_or_default();
    CallError::Api(ApiError {
        status,
        error: error.to_owned(),
        message: message.to_owned(),
    })
}
