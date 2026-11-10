//! `sekisho gateway-agent`: runs next to agentgateway, long-polls Sekisho
//! for new configs and writes them where agentgateway reads its config.
//!
//! The file on disk is always the last good config: a server that can't be
//! reached, answers an error, or sends YAML that doesn't parse or fails
//! agentgateway's schema leaves it untouched.

use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use reqwest::Url;
use serde::Deserialize;

/// How long one poll may take; the server answers within 30 s.
const POLL_TIMEOUT: Duration = Duration::from_secs(60);
const ENDPOINT: &str = "/internal/gateway/config";

#[derive(Clone)]
pub struct AgentConfig {
    /// Sekisho's admin listener, `http://host:port/`.
    pub server_url: Url,
    pub gateway_token: String,
    /// The file agentgateway reads (`-f`).
    pub config_path: PathBuf,
    /// The wait after a failed poll or write, and the least time between
    /// two polls, so a server that answers "unchanged" at once can't make
    /// the agent spin.
    pub retry: Duration,
}

/// Leaves out the token.
impl fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentConfig")
            .field("server_url", &self.server_url.as_str())
            .field("config_path", &self.config_path)
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

/// Why the agent could not start. Once running, it only logs and retries.
#[derive(Debug)]
pub enum AgentError {
    UnsupportedScheme(String),
    Client(reqwest::Error),
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::UnsupportedScheme(scheme) => write!(
                f,
                "--server must be an http:// URL, not {scheme}://: https to the Sekisho server \
                 comes in a later release, so keep the agent and server on a private network"
            ),
            AgentError::Client(e) => write!(f, "cannot build the HTTP client: {e}"),
        }
    }
}

impl std::error::Error for AgentError {}

/// A published config, as `/internal/gateway/config` answers it.
#[derive(Deserialize)]
struct Published {
    version: i64,
    yaml: String,
}

enum Poll {
    Newer(Published),
    Unchanged,
}

/// Polls and writes forever. Returns only if it cannot start.
pub async fn run_agent(config: AgentConfig) -> Result<(), AgentError> {
    if config.server_url.scheme() != "http" {
        return Err(AgentError::UnsupportedScheme(
            config.server_url.scheme().into(),
        ));
    }
    let mut endpoint = config.server_url.clone();
    endpoint.set_path(ENDPOINT);
    let client = reqwest::Client::builder()
        .timeout(POLL_TIMEOUT)
        .build()
        .map_err(AgentError::Client)?;
    tracing::info!(server = %config.server_url, path = %config.config_path.display(), "gateway agent started");

    // The newest version applied or skipped. 0 fetches the latest on start.
    let mut after = 0;
    loop {
        let polled = tokio::time::Instant::now();
        match poll(&client, &endpoint, &config.gateway_token, after).await {
            // A long poll returns after a while; an instant answer waits.
            Ok(Poll::Unchanged) => tokio::time::sleep_until(polled + config.retry).await,
            Ok(Poll::Newer(published)) => {
                if let Err(errors) = crate::validate(&published.yaml) {
                    tracing::error!(
                        version = published.version,
                        ?errors,
                        "received config is invalid; keeping the last good file"
                    );
                    after = published.version; // it won't become valid; wait for the next
                    continue;
                }
                match write_atomic(&config.config_path, &published.yaml) {
                    Ok(()) => {
                        after = published.version;
                        tracing::info!(version = after, "gateway config applied");
                    }
                    Err(error) => {
                        tracing::error!(%error, "writing the gateway config failed; retrying");
                        tokio::time::sleep(config.retry).await;
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, "config poll failed; keeping the last good file");
                tokio::time::sleep(config.retry).await;
            }
        }
    }
}

async fn poll(
    client: &reqwest::Client,
    endpoint: &Url,
    token: &str,
    after: i64,
) -> Result<Poll, String> {
    let mut url = endpoint.clone();
    url.query_pairs_mut()
        .clear()
        .append_pair("after", &after.to_string());
    let response = client
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    match response.status().as_u16() {
        200 => {
            let body = response.text().await.map_err(|e| e.to_string())?;
            serde_json::from_str(&body)
                .map(Poll::Newer)
                .map_err(|e| format!("unreadable answer: {e}"))
        }
        204 => Ok(Poll::Unchanged),
        status => Err(format!("server answered {status}")),
    }
}

/// Replaces `path` with `contents` in one step: a temp file in the same
/// directory is written, synced and renamed over the target, so a reader
/// sees the old file or the new one, never a mix. On Unix the file is
/// readable by its owner only, as it holds provider keys.
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut file = tempfile::Builder::new()
        .prefix(".sekisho-config-")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    file.write_all(contents.as_bytes())?;
    file.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.persist(path).map_err(|e| e.error)?;
    #[cfg(unix)]
    if let Ok(dir) = std::fs::File::open(dir) {
        let _ = dir.sync_all(); // make the rename durable
    }
    Ok(())
}
