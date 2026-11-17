//! Requests Sekisho makes itself at sync: container registries and A2A
//! agents' cards (D178). Each resolves the host and checks every address
//! (`sekisho_store::address`), then connects only to those addresses: the
//! connector's resolver answers nothing else. No redirects (hyper follows
//! none), no proxy, 30 s, and at most `limit` bytes read.
//!
//! TLS is rustls with `ring` and the platform's verifier, passed to this
//! client alone: no process-wide provider, so every other HTTP client in
//! Sekisho stays as it was.

use std::future::{Ready, ready};
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::Duration;

use http::header::{HOST, HeaderMap, HeaderValue, USER_AGENT};
use http::{Method, Request};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::rt::TokioExecutor;
use sekisho_store::address::resolve_checked;
use url::Url;

/// Every outside request's deadline.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// Installs `ring` as rustls' process-wide provider. Outside requests pass
/// their provider explicitly and don't need it; `main` installs it so no
/// other rustls user in the process falls back to another.
pub fn install_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
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

/// A resolver that answers the checked addresses, whatever it is asked.
#[derive(Clone)]
struct Pinned(Arc<Vec<SocketAddr>>);

impl tower_service::Service<Name> for Pinned {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = std::io::Error;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Name) -> Self::Future {
        ready(Ok(self.0.as_ref().clone().into_iter()))
    }
}

/// An answer, read in full up to the request's limit.
#[derive(Debug)]
pub struct Fetched {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

/// How outside requests are made.
#[derive(Clone, Copy, Debug, Default)]
pub struct Outbound {
    /// `http://` too: tests and the e2e build only.
    pub allow_plain_http: bool,
}

/// An error and its causes, which never hold the URL.
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

impl Outbound {
    pub fn new(allow_plain_http: bool) -> Self {
        Self { allow_plain_http }
    }

    pub async fn get(
        &self,
        url: &Url,
        allow_private: bool,
        headers: HeaderMap,
        limit: usize,
    ) -> Result<Fetched, String> {
        self.send(Method::GET, url, allow_private, headers, None, limit)
            .await
    }

    pub async fn head(
        &self,
        url: &Url,
        allow_private: bool,
        headers: HeaderMap,
    ) -> Result<Fetched, String> {
        self.send(Method::HEAD, url, allow_private, headers, None, 0)
            .await
    }

    /// One request to `url`, connected only to its checked addresses.
    pub async fn send(
        &self,
        method: Method,
        url: &Url,
        allow_private: bool,
        headers: HeaderMap,
        body: Option<Vec<u8>>,
        limit: usize,
    ) -> Result<Fetched, String> {
        match url.scheme() {
            "https" => {}
            "http" if self.allow_plain_http => {}
            _ => return Err("only https:// is reached".to_owned()),
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err("a URL with credentials isn't reached".to_owned());
        }
        let host = url.host().ok_or("the URL has no host")?.to_owned();
        let port = url.port_or_known_default().ok_or("the URL has no port")?;
        let addresses = resolve_checked(&host, port, allow_private).await?;

        let mut http = HttpConnector::new_with_resolver(Pinned(Arc::new(addresses)));
        http.enforce_http(false);
        http.set_connect_timeout(Some(TIMEOUT));
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls_config()?)
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).build(connector);

        let mut request = Request::builder()
            .method(method)
            .uri(url.as_str())
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .map_err(|_| "the request can't be made".to_owned())?;
        *request.headers_mut() = headers;
        request
            .headers_mut()
            .insert(USER_AGENT, HeaderValue::from_static("sekisho"));
        if let Ok(value) = HeaderValue::from_str(&host_header(url)) {
            request.headers_mut().insert(HOST, value);
        }

        let exchange = async {
            let response = client.request(request).await.map_err(|e| describe(&e))?;
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            let mut body = response.into_body();
            let mut read = Vec::new();
            while let Some(frame) = body.frame().await {
                let frame = frame.map_err(|e| describe(&e))?;
                if let Ok(chunk) = frame.into_data() {
                    if read.len() + chunk.len() > limit {
                        return Err(format!("the answer is larger than {limit} bytes"));
                    }
                    read.extend_from_slice(&chunk);
                }
            }
            Ok(Fetched {
                status,
                headers,
                body: read,
            })
        };
        tokio::time::timeout(TIMEOUT, exchange)
            .await
            .map_err(|_| "the request timed out".to_owned())?
    }
}

/// `host[:port]` as the Host header carries it, the port only when it
/// isn't the scheme's own.
fn host_header(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}
