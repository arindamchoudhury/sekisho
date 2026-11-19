//! `sekisho login` and `sekisho logout`. The browser flow is OAuth's code
//! flow with PKCE (S256) back to a loopback port (RFC 8252); the device flow
//! is RFC 8628, for a machine without a browser. Both are the pre-registered
//! public client `sekisho-cli` (plan 2).

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sekisho_core::keys::random_secret;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::sync::{Mutex, oneshot};
use url::Url;

use crate::config::{Config, Dirs, ServerUrl};
use crate::credentials::{Credential, Store, Tokens};
use crate::output::{CliError, note, warn};
use crate::server::{CallError, Server};

/// The CLI's client ID at Sekisho.
pub const CLIENT_ID: &str = "sekisho-cli";

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// How long `login` waits for the person.
pub const WAIT: Duration = Duration::from_secs(10 * 60);

/// Which flow signs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// A browser on this machine, back to a loopback port.
    Browser,
    /// A code typed into a browser anywhere.
    Device,
}

/// What the device flow asks the person to do.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct DeviceCode {
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
}

/// How sign-in talks to the person.
pub trait SignInUi: Send + Sync {
    /// The link that starts the browser flow.
    fn browser_link(&self, url: &Url);
    /// The device flow's code and where to type it.
    fn device_code(&self, code: &DeviceCode);
}

/// The terminal: prompts on stderr, and the browser opened unless asked not
/// to.
pub struct TerminalUi {
    pub open_browser: bool,
}

impl SignInUi for TerminalUi {
    fn browser_link(&self, url: &Url) {
        note("Sign in to Sekisho in your browser:");
        note(&format!("  {url}"));
        if self.open_browser && open::that(url.as_str()).is_err() {
            note("(the browser didn't open; open the link yourself)");
        }
    }

    fn device_code(&self, code: &DeviceCode) {
        note(&format!(
            "Go to {} and enter the code {}",
            code.verification_uri, code.user_code
        ));
        if let Some(complete) = &code.verification_uri_complete {
            note(&format!("  or open {complete}"));
        }
    }
}

/// Who signed in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed {
    pub email: String,
    /// Whether the tokens were kept (a keyring that refused the write keeps
    /// nothing).
    pub saved: bool,
}

#[derive(Deserialize)]
struct TokenAnswer {
    access_token: String,
    expires_in: i64,
    refresh_token: String,
}

impl TokenAnswer {
    fn tokens(self, received_at: OffsetDateTime) -> Tokens {
        Tokens {
            access: self.access_token,
            access_expires_at: received_at + time::Duration::seconds(self.expires_in),
            refresh: self.refresh_token,
        }
    }
}

#[derive(Deserialize)]
struct Me {
    email: String,
}

#[derive(Deserialize)]
struct Metadata {
    issuer: String,
}

#[derive(Deserialize)]
struct DeviceStart {
    device_code: String,
    expires_in: u64,
    interval: Option<u64>,
    #[serde(flatten)]
    shown: DeviceCode,
}

/// What `login` says when no keyring answers (D195).
pub fn no_keyring_error() -> CliError {
    CliError::error(
        "no keyring can keep your sign-in on this machine (as over SSH, in a service, \
         or on Linux without Secret Service or keyutils), so `login` would keep nothing",
    )
    .with_hint(
        "start your coding tool with `sekisho run <tool>`, which signs in for its own \
         lifetime, or set SEKISHO_URL and SEKISHO_KEY",
    )
}

/// Signs in, keeps the tokens in `store` and the server and email in
/// `config.json`, and ends any other server's session (D196).
pub async fn login(
    server: &Server,
    method: Method,
    ui: &dyn SignInUi,
    store: &dyn Store,
    dirs: &Dirs,
    wait: Duration,
) -> Result<Signed, CliError> {
    let tokens = match method {
        Method::Browser => login_browser(server, ui, wait).await?,
        Method::Device => login_device(server, ui, wait).await?,
    };
    let me: Me = server
        .get_json(
            "/api/v1/me",
            Some(&Credential::Session(tokens.access.clone())),
        )
        .await?;

    let mut config = Config::load(dirs)?;
    let key = server.url().key().to_owned();
    if let Some(old) = config.server.clone().filter(|old| *old != key) {
        end_session_at(&old, store).await;
    }
    let saved = match store.save(&key, &tokens) {
        Ok(()) => true,
        Err(e) => {
            warn(&format!(
                "{e}, so this sign-in isn't kept; use `sekisho run <tool>`, which signs in \
                 for its own lifetime, or SEKISHO_URL with SEKISHO_KEY"
            ));
            false
        }
    };
    config.server = Some(key);
    config.email = Some(me.email.clone());
    config.save(dirs)?;
    Ok(Signed {
        email: me.email,
        saved,
    })
}

/// Revokes the session at `server` and forgets its tokens, quietly: a
/// session that can't be revoked still ends when its tokens are gone.
async fn end_session_at(server: &str, store: &dyn Store) {
    let Ok(url) = ServerUrl::parse(server) else {
        return;
    };
    if let Ok(Some(tokens)) = store.load(url.key())
        && let Ok(old) = Server::new(url.clone())
    {
        let _ = revoke(&old, &tokens.refresh).await;
    }
    let _ = store.delete(url.key());
}

async fn revoke(server: &Server, refresh: &str) -> Result<(), CallError> {
    server
        .post_form::<serde_json::Value>(
            "/oauth/revoke",
            &[("token", refresh), ("client_id", CLIENT_ID)],
        )
        .await
        .map(|_| ())
}

/// Revokes the session and forgets it, even when the revoke fails.
pub async fn logout(server: &Server, store: &dyn Store, dirs: &Dirs) -> Result<(), CliError> {
    let key = server.url().key();
    let stored = store
        .load(key)
        .map_err(|e| CliError::error(e.to_string()))?;
    if let Some(tokens) = &stored
        && let Err(e) = revoke(server, &tokens.refresh).await
    {
        warn(&format!(
            "Sekisho didn't confirm the session ended ({e}); it is forgotten here"
        ));
    }
    store
        .delete(key)
        .map_err(|e| CliError::error(e.to_string()))?;
    let mut config = Config::load(dirs)?;
    if config.email.is_some() {
        config.email = None;
        config.save(dirs)?;
    }
    if stored.is_none() {
        note("not signed in here; nothing to end");
    }
    Ok(())
}

/// The S256 challenge of a PKCE verifier.
fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// What the loopback listener hands back.
type Callback = Result<String, CliError>;

#[derive(Clone)]
struct Waiting {
    state: String,
    issuer: String,
    done: Arc<Mutex<Option<oneshot::Sender<Callback>>>>,
}

#[derive(Deserialize)]
struct CallbackQuery {
    state: Option<String>,
    code: Option<String>,
    iss: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

fn page(status: StatusCode, title: &str, text: &str) -> Response {
    (
        status,
        [("cache-control", "no-store")],
        Html(format!(
            "<!doctype html><meta charset=utf-8><title>{title}</title>\
             <h1>{title}</h1><p>{text}</p>"
        )),
    )
        .into_response()
}

/// The browser's return: only the request carrying this sign-in's `state`
/// and Sekisho's issuer counts; anything else is refused and the CLI keeps
/// waiting.
async fn callback(State(waiting): State<Waiting>, Query(q): Query<CallbackQuery>) -> Response {
    if q.state.as_deref() != Some(waiting.state.as_str()) {
        return page(
            StatusCode::BAD_REQUEST,
            "Not this sign-in",
            "This link isn't the sign-in sekisho login is waiting for.",
        );
    }
    if q.iss.as_deref().is_some_and(|iss| iss != waiting.issuer) {
        return page(
            StatusCode::BAD_REQUEST,
            "Not this server",
            "This sign-in came back from another server than the one sekisho login asked.",
        );
    }
    let answer = match (q.code, q.error) {
        (Some(code), None) => Ok(code),
        (_, Some(error)) => Err(CliError::error(format!(
            "the sign-in failed: {}",
            q.error_description.unwrap_or(error)
        ))),
        (None, None) => Err(CliError::error("the sign-in came back without a code")),
    };
    let ok = answer.is_ok();
    if let Some(done) = waiting.done.lock().await.take() {
        let _ = done.send(answer);
    }
    if ok {
        page(
            StatusCode::OK,
            "Signed in",
            "You are signed in to Sekisho. You can close this tab.",
        )
    } else {
        page(
            StatusCode::BAD_REQUEST,
            "Sign-in failed",
            "The sign-in didn't finish. See the terminal.",
        )
    }
}

/// The code flow with PKCE, back to `http://127.0.0.1:<port>/callback`.
pub async fn login_browser(
    server: &Server,
    ui: &dyn SignInUi,
    wait: Duration,
) -> Result<Tokens, CliError> {
    let metadata: Metadata = server
        .get_json("/.well-known/oauth-authorization-server", None)
        .await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| {
            CliError::error(format!(
                "can't listen on this computer for the sign-in: {e}"
            ))
        })?;
    let port = listener
        .local_addr()
        .map_err(|e| CliError::error(e.to_string()))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let state = random_secret();
    let verifier = random_secret();

    let (sender, receiver) = oneshot::channel();
    let waiting = Waiting {
        state: state.clone(),
        issuer: metadata.issuer,
        done: Arc::new(Mutex::new(Some(sender))),
    };
    let (stop, stopped) = oneshot::channel::<()>();
    let app = Router::new()
        .route("/callback", get(callback))
        .with_state(waiting);
    let listening = tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await;
    });

    let mut link = Url::parse(&server.url().join("/oauth/authorize"))
        .map_err(|e| CliError::error(e.to_string()))?;
    link.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge(&verifier))
        .append_pair("code_challenge_method", "S256");
    ui.browser_link(&link);

    let answer = tokio::time::timeout(wait, receiver).await;
    // Let the browser's page finish before the listener goes.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = stop.send(());
    let _ = listening.await;
    let code = match answer {
        Ok(Ok(code)) => code?,
        Ok(Err(_)) => return Err(CliError::error("the sign-in stopped")),
        Err(_) => {
            return Err(CliError::error(format!(
                "no sign-in within {} minutes",
                wait.as_secs() / 60
            ))
            .with_hint("run `sekisho login` again, or `sekisho login --device` on a machine without a browser"));
        }
    };
    let received_at = OffsetDateTime::now_utc();
    let answer: TokenAnswer = server
        .post_form(
            "/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("client_id", CLIENT_ID),
                ("code", &code),
                ("redirect_uri", &redirect_uri),
                ("code_verifier", &verifier),
            ],
        )
        .await?;
    Ok(answer.tokens(received_at))
}

/// RFC 8628: shows a code, then polls every `interval` seconds, 5 more after
/// each `slow_down`.
pub async fn login_device(
    server: &Server,
    ui: &dyn SignInUi,
    wait: Duration,
) -> Result<Tokens, CliError> {
    let start: DeviceStart = server
        .post_form("/oauth/device_authorization", &[("client_id", CLIENT_ID)])
        .await?;
    ui.device_code(&start.shown);
    let deadline = tokio::time::Instant::now() + wait.min(Duration::from_secs(start.expires_in));
    let mut interval = Duration::from_secs(start.interval.unwrap_or(5).max(1));
    loop {
        tokio::time::sleep(interval).await;
        if tokio::time::Instant::now() > deadline {
            return Err(CliError::error("the code expired before it was approved")
                .with_hint("run `sekisho login --device` again"));
        }
        let received_at = OffsetDateTime::now_utc();
        let polled: Result<TokenAnswer, CallError> = server
            .post_form(
                "/oauth/token",
                &[
                    ("grant_type", DEVICE_GRANT),
                    ("client_id", CLIENT_ID),
                    ("device_code", &start.device_code),
                ],
            )
            .await;
        match polled {
            Ok(answer) => return Ok(answer.tokens(received_at)),
            Err(e) => match e.code() {
                Some("authorization_pending") => {}
                Some("slow_down") => interval += Duration::from_secs(5),
                Some("access_denied") => {
                    return Err(CliError::error("the sign-in was denied"));
                }
                Some("expired_token") => {
                    return Err(CliError::error("the code expired before it was approved")
                        .with_hint("run `sekisho login --device` again"));
                }
                _ => return Err(e.into()),
            },
        }
    }
}
