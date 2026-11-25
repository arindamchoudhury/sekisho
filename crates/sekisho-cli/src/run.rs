//! `sekisho run <tool> [-- <args>…]` (D191, D194): a coding tool that can't
//! run a command for its key is started behind a proxy on `127.0.0.1`,
//! which swaps the run's own secret for a fresh Sekisho token on every
//! request.
//!
//! The run reads the folder's lock (then the global one's) only for which
//! environments are installed for the tool and which gives its models
//! (D216). Everything that decides what runs and where tokens go comes from
//! the server: the model environment's delivery document gives the gateway
//! URL, and the adapter turns that document into the program and its
//! variables. A lock is a file in the folder, which a repository could
//! carry, so it never names the program or the host.
//!
//! The proxy answers only requests carrying the secret in `Authorization`,
//! `x-api-key` or `x-goog-api-key`, only under `/env/<name>/` for those
//! environments, removes every credential the tool sent, adds
//! `Authorization: Bearer <token>`, and streams both ways to the gateway.
//! A redirect from the gateway is an error: the tool could follow it with
//! the secret. `GET /token` with the secret answers the token itself, for
//! `sekisho token` inside the run.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, StatusCode, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use sha2::{Digest, Sha256};

use sekisho_catalog::delivery::Delivery;
use sekisho_core::keys::random_secret;
use sekisho_core::names::EnvName;

use crate::adapters::{RunSpec, Tool, find_on_path};
use crate::config::ServerUrl;
use crate::install::Scope;
use crate::install::lock::Lock;
use crate::output::{CliError, Exit};
use crate::server::{Https, connector, describe};

/// The run's tokens: called for every request, so a long run refreshes.
pub type TokenFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<String, CliError>> + Send>> + Send + Sync>;

/// What the lock says a run of a tool reaches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The tool's model environment.
    pub env: String,
    /// Every environment installed for the tool in that scope, in name
    /// order: their gateway paths are the only ones the proxy serves.
    pub envs: Vec<String>,
}

/// The folder's install for `tool`, else the global one (D214).
pub fn plan(tool: Tool, scope: &Scope) -> Result<Plan, CliError> {
    for s in std::iter::once(scope.clone()).chain(scope.other()) {
        let lock = Lock::read(&s.lock_path())?;
        let Some(env) = lock.models.get(tool.as_str()) else {
            continue;
        };
        if !lock
            .environments
            .get(env)
            .is_some_and(|e| e.tools.contains_key(&tool))
        {
            continue;
        }
        let envs = lock
            .environments
            .iter()
            .filter(|(_, e)| e.tools.contains_key(&tool))
            .map(|(name, _)| {
                EnvName::parse(name)
                    .map(|n| n.to_string())
                    .map_err(|e| CliError::error(format!("{}: {e}", s.lock_path().display())))
            })
            .collect::<Result<_, _>>()?;
        return Ok(Plan {
            env: env.clone(),
            envs,
        });
    }
    Err(CliError::error(format!(
        "no environment is installed for {tool} in this folder or globally"
    ))
    .with_hint(format!("run `sekisho use <env> --target {tool}` first")))
}

/// Each environment's gateway URL, from the model environment's delivery
/// document: its `gateway_url` is `<gateway>/env/<name>`, the gateway the
/// same for every environment of the install.
pub fn gateways(d: &Delivery, envs: &[String]) -> Result<BTreeMap<String, ServerUrl>, CliError> {
    let refused = || {
        CliError::new(
            Exit::Refused,
            "the server's gateway URL isn't one sekisho run sends tokens to",
        )
    };
    let base = d
        .gateway_url
        .strip_suffix(&format!("/env/{}", d.environment))
        .ok_or_else(refused)?;
    envs.iter()
        .map(|name| {
            let url = ServerUrl::parse(&format!("{base}/env/{name}")).map_err(|e| {
                CliError::new(Exit::Refused, format!("the server's gateway URL: {e}"))
            })?;
            Ok((name.clone(), url))
        })
        .collect()
}

/// The proxy, answering on `127.0.0.1` until stopped.
pub struct Proxy {
    url: String,
    secret: String,
    task: tokio::task::JoinHandle<()>,
}

impl Proxy {
    /// `http://127.0.0.1:<port>`, the run's `SEKISHO_RUN_URL`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The run's `SEKISHO_RUN_KEY`.
    pub fn secret(&self) -> &str {
        &self.secret
    }

    pub async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proxy").field("url", &self.url).finish()
    }
}

#[derive(Clone)]
struct Shared {
    secret: [u8; 32],
    gateways: Arc<BTreeMap<String, ServerUrl>>,
    tokens: TokenFn,
    client: Client<Https, Body>,
    /// The tool the proxy names to the gateway on every request (D227).
    tool: Option<HeaderValue>,
}

/// Starts the proxy for `gateways`, with a new 32-byte secret; with
/// `tool_header`, every request it forwards names that tool (D227).
pub async fn start(
    gateways: BTreeMap<String, ServerUrl>,
    tokens: TokenFn,
    tool_header: Option<Tool>,
) -> Result<Proxy, CliError> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|e| CliError::error(format!("can't listen on 127.0.0.1: {e}")))?;
    let addr = listener
        .local_addr()
        .map_err(|e| CliError::error(e.to_string()))?;
    let secret = random_secret();
    let shared = Shared {
        secret: Sha256::digest(secret.as_bytes()).into(),
        gateways: Arc::new(gateways),
        tokens,
        client: Client::builder(TokioExecutor::new()).build(connector()?),
        tool: tool_header.map(|t| HeaderValue::from_static(t.as_str())),
    };
    let app = Router::new().fallback(handle).with_state(shared);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(Proxy {
        url: format!("http://{addr}"),
        secret,
        task,
    })
}

/// Sekisho's error shape, which tools show as the answer's text.
fn refusal(status: StatusCode, error: &str, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({"error": error, "message": message}).to_string(),
    )
        .into_response()
}

/// The headers a tool may put its key in.
const KEY_HEADERS: [&str; 2] = ["x-api-key", "x-goog-api-key"];

/// Removed from every request: credentials, then hop-by-hop headers.
const STRIPPED: [&str; 13] = [
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "x-goog-api-key",
    "api-key",
    "cookie",
    "host",
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "upgrade",
];

impl Shared {
    fn is_secret(&self, offered: &str) -> bool {
        let offered: [u8; 32] = Sha256::digest(offered.as_bytes()).into();
        offered == self.secret
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        let bearer = headers
            .get_all(header::AUTHORIZATION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|v| {
                v.get(..7)
                    .filter(|p| p.eq_ignore_ascii_case("bearer "))
                    .map(|_| v[7..].trim())
            });
        let keys = KEY_HEADERS
            .iter()
            .flat_map(|h| headers.get_all(*h).iter())
            .filter_map(|v| v.to_str().ok());
        bearer.chain(keys).any(|v| self.is_secret(v))
    }
}

/// `/env/<name>/<rest>` as `(name, "/<rest>")`, refusing dot segments and
/// encoded slashes or backslashes, which a gateway could read as leaving
/// the environment's path.
fn route(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/env/")?;
    let slash = rest.find('/')?;
    let (name, rest) = rest.split_at(slash);
    for segment in path.split('/') {
        let lower = segment.to_ascii_lowercase();
        if lower.contains("%2f") || lower.contains("%5c") || segment.contains('\\') {
            return None;
        }
        let decoded = lower.replace("%2e", ".");
        if decoded == "." || decoded == ".." {
            return None;
        }
    }
    Some((name, rest))
}

/// The query without a `key` parameter, where a Gemini client could put
/// its key.
fn query_without_key(query: &str) -> String {
    query
        .split('&')
        .filter(|p| !p.is_empty() && p.split('=').next() != Some("key"))
        .collect::<Vec<_>>()
        .join("&")
}

fn strip(headers: &mut HeaderMap) {
    let listed: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|n| HeaderName::try_from(n.trim()).ok())
        .collect();
    for name in STRIPPED.iter().copied() {
        headers.remove(name);
    }
    headers.remove(header::TRANSFER_ENCODING);
    for name in listed {
        headers.remove(name);
    }
}

async fn handle(State(s): State<Shared>, req: Request) -> Response {
    let (mut parts, body) = req.into_parts();
    if !s.authorized(&parts.headers) {
        return refusal(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "this request doesn't carry the key of this sekisho run",
        );
    }
    if parts.uri.path() == "/token" {
        if parts.method != Method::GET {
            return refusal(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "GET only",
            );
        }
        return match (s.tokens)().await {
            Ok(token) => (
                [(header::CONTENT_TYPE, "application/json")],
                serde_json::json!({ "token": token }).to_string(),
            )
                .into_response(),
            Err(e) => token_refusal(&e),
        };
    }
    let not_found = || {
        refusal(
            StatusCode::NOT_FOUND,
            "not_found",
            "sekisho run serves only the model routes of the environments installed for this tool",
        )
    };
    let Some((env, rest)) = route(parts.uri.path()) else {
        return not_found();
    };
    let Some(gateway) = s.gateways.get(env) else {
        return not_found();
    };
    let mut target = format!("{}{rest}", gateway.key());
    if let Some(q) = parts
        .uri
        .query()
        .map(query_without_key)
        .filter(|q| !q.is_empty())
    {
        target.push('?');
        target.push_str(&q);
    }
    let Ok(uri) = target.parse::<Uri>() else {
        return not_found();
    };
    let token = match (s.tokens)().await {
        Ok(token) => token,
        Err(e) => return token_refusal(&e),
    };
    let Ok(mut bearer) = HeaderValue::from_str(&format!("Bearer {token}")) else {
        return refusal(
            StatusCode::BAD_GATEWAY,
            "bad_token",
            "the token can't be sent",
        );
    };
    bearer.set_sensitive(true);
    strip(&mut parts.headers);
    parts.headers.insert(header::AUTHORIZATION, bearer);
    if let Some(tool) = &s.tool {
        // `insert` replaces every copy the tool sent.
        parts.headers.insert(
            HeaderName::from_static(sekisho_core::TOOL_HEADER),
            tool.clone(),
        );
    }
    parts.uri = uri;
    let answer = match s.client.request(Request::from_parts(parts, body)).await {
        Ok(answer) => answer,
        Err(e) => {
            return refusal(
                StatusCode::BAD_GATEWAY,
                "gateway_unreachable",
                &format!("can't reach the gateway: {}", describe(&e)),
            );
        }
    };
    if answer.status().is_redirection() {
        return refusal(
            StatusCode::BAD_GATEWAY,
            "gateway_redirect",
            &format!(
                "the gateway answered with a redirect ({}), which sekisho run never follows",
                answer.status().as_u16()
            ),
        );
    }
    let (mut parts, body) = answer.into_parts();
    strip(&mut parts.headers);
    Response::from_parts(parts, Body::new(body))
}

/// The tool's answer when the run has no token: 401 when the person must
/// sign in again, else 502; Sekisho's reason either way, never a token.
fn token_refusal(e: &CliError) -> Response {
    let status = if e.exit == Exit::NotSignedIn {
        StatusCode::UNAUTHORIZED
    } else {
        StatusCode::BAD_GATEWAY
    };
    refusal(status, "no_token", &e.message)
}

/// `program` as the run starts it: a path as given, else found on `PATH`
/// (with `PATHEXT` on Windows, where npm installs the tools as `.cmd`).
fn program(name: &str) -> Result<PathBuf, CliError> {
    if name.contains('/') || name.contains('\\') {
        return Ok(PathBuf::from(name));
    }
    find_on_path(name).ok_or_else(|| {
        CliError::error(format!("can't find {name} on PATH")).with_hint(format!(
            "install it, or add the folder holding {name} to PATH"
        ))
    })
}

/// Starts `spec` with the run's variables and `extra` arguments, the
/// terminal handed over, and waits for it: its exit code. Ctrl+C reaches
/// the tool from the terminal; the run only outlives it.
pub async fn run_child(spec: &RunSpec, extra: &[String], proxy: &Proxy) -> Result<i32, CliError> {
    let fill = |s: &str| {
        s.replace("${SEKISHO_RUN_URL}", proxy.url())
            .replace("${SEKISHO_RUN_KEY}", proxy.secret())
    };
    let path = program(&spec.program)?;
    let mut command = tokio::process::Command::new(&path);
    command
        .args(spec.args.iter().map(|a| fill(a)))
        .args(extra)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (k, v) in &spec.env {
        command.env(k, fill(v));
    }
    command
        .env("SEKISHO_RUN_URL", proxy.url())
        .env("SEKISHO_RUN_KEY", proxy.secret());
    let mut child = command
        .spawn()
        .map_err(|e| CliError::error(format!("can't start {}: {e}", spec.program)))?;
    let mut listening = true;
    let status = loop {
        tokio::select! {
            status = child.wait() => break status,
            signal = tokio::signal::ctrl_c(), if listening => {
                // The terminal sent it to the tool too, which decides.
                listening = signal.is_ok();
            }
        }
    }
    .map_err(|e| CliError::error(format!("{} didn't finish: {e}", spec.program)))?;
    Ok(exit_code(status))
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_stay_inside_an_environment() {
        assert_eq!(
            route("/env/a/anthropic/v1/messages"),
            Some(("a", "/anthropic/v1/messages"))
        );
        assert_eq!(route("/env/a/"), Some(("a", "/")));
        for bad in [
            "/env/a",
            "/v1/messages",
            "/env/a/../b/x",
            "/env/a/%2E%2e/b",
            "/env/a/x%2fy",
            "/env/a/x%5Cy",
            "/env/a/x\\y",
            "/env/a/./x",
        ] {
            assert_eq!(route(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_key_in_the_query_is_dropped() {
        assert_eq!(query_without_key("alt=sse&key=abc"), "alt=sse");
        assert_eq!(query_without_key("key=abc"), "");
        assert_eq!(query_without_key("keys=1&alt=sse"), "keys=1&alt=sse");
    }
}
