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

/// The quiet spell after which the agent writes the current config once
/// more. agentgateway v1.7.0 stops watching its config file when the file
/// is replaced, reloads, and only then watches again (`util.rs`
/// `watch_files_with_options`, `state_manager.rs` `watch_config_file`), so
/// a write that lands during a reload is never loaded, and the gateway
/// keeps an older config until the next write. A reload takes about 25 ms
/// for a small config and about 0.6 s at 1,500 environments; agentgateway
/// waits 250 ms for a file to settle first. 2 s covers both, and keeps a
/// change live within the design's 5 s.
pub const REWRITE_AFTER: Duration = Duration::from_secs(2);

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
    /// How long after a write, with no newer version, the agent writes the
    /// same config once more ([`REWRITE_AFTER`] in production).
    pub rewrite_after: Duration,
}

/// Leaves out the token.
impl fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentConfig")
            .field("server_url", &self.server_url.as_str())
            .field("config_path", &self.config_path)
            .field("retry", &self.retry)
            .field("rewrite_after", &self.rewrite_after)
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
    // The config last written, and when to write it once more.
    let mut rewrite: Option<(tokio::time::Instant, i64, String)> = None;
    loop {
        let polled = tokio::time::Instant::now();
        let request = poll(&client, &endpoint, &config.gateway_token, after);
        tokio::pin!(request);
        // The long poll keeps running while the re-write is due.
        let answer = loop {
            let due = rewrite.as_ref().map(|(at, _, _)| *at);
            tokio::select! {
                answer = &mut request => break answer,
                () = async { tokio::time::sleep_until(due.unwrap()).await }, if due.is_some() => {
                    let (_, version, yaml) = rewrite.take().expect("due");
                    match write_atomic(&config.config_path, &yaml) {
                        Ok(()) => tracing::debug!(version, "gateway config written again"),
                        Err(error) => tracing::warn!(%error, "writing the gateway config again failed"),
                    }
                }
            }
        };
        match answer {
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
                        rewrite = Some((
                            tokio::time::Instant::now() + config.rewrite_after,
                            published.version,
                            published.yaml,
                        ));
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
