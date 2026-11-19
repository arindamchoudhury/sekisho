//! `sekisho token`: the credential a request carries. `SEKISHO_KEY` first;
//! then a running `sekisho run`'s proxy; then the stored session, its access
//! token reused until 5 minutes before it expires and refreshed under an
//! exclusive file lock, since refresh tokens rotate and a reused one ends the
//! session (plan 2).

use std::fs::{self, File, OpenOptions};
use std::path::Path;

use serde::Deserialize;
use time::OffsetDateTime;

use crate::config::{ServerUrl, Vars};
use crate::credentials::{Credential, Store, Tokens};
use crate::login::CLIENT_ID;
use crate::output::CliError;
use crate::server::Server;

/// An access token is reused until this long before it expires.
pub const REUSE_MARGIN: time::Duration = time::Duration::minutes(5);

/// Where a credential can come from besides the store.
#[derive(Clone, Debug, Default)]
pub struct Sources {
    /// `SEKISHO_KEY`.
    pub key: Option<String>,
    /// `SEKISHO_RUN_URL` and `SEKISHO_RUN_KEY`, inside `sekisho run`.
    pub run: Option<(String, String)>,
}

impl Sources {
    pub fn from_vars(vars: &Vars) -> Self {
        let run = match (vars.get("SEKISHO_RUN_URL"), vars.get("SEKISHO_RUN_KEY")) {
            (Some(url), Some(key)) => Some((url.to_owned(), key.to_owned())),
            _ => None,
        };
        Self {
            key: vars.get("SEKISHO_KEY").map(str::to_owned),
            run,
        }
    }
}

#[derive(Deserialize)]
struct Refreshed {
    access_token: String,
    expires_in: i64,
    refresh_token: String,
}

/// The current credential, with the real clock.
pub async fn current(
    server: &Server,
    store: &dyn Store,
    sources: &Sources,
    lock: &Path,
) -> Result<Credential, CliError> {
    current_with(server, store, sources, lock, &OffsetDateTime::now_utc).await
}

/// `current`, with `now` as the clock.
pub async fn current_with(
    server: &Server,
    store: &dyn Store,
    sources: &Sources,
    lock: &Path,
    now: &(dyn Fn() -> OffsetDateTime + Send + Sync),
) -> Result<Credential, CliError> {
    if let Some(key) = &sources.key {
        return Ok(Credential::Key(key.clone()));
    }
    if let Some((url, key)) = &sources.run {
        return from_run(url, key).await;
    }
    let origin = server.url().key();
    let stored = load(store, origin)?;
    if let Some(access) = fresh(&stored, now()) {
        return Ok(Credential::Session(access));
    }

    // Under the lock, look again: another process may have refreshed.
    let _held = take_lock(lock).await?;
    let Some(tokens) = load(store, origin)? else {
        return Err(CliError::not_signed_in("not signed in to Sekisho"));
    };
    if let Some(access) = fresh(&Some(tokens.clone()), now()) {
        return Ok(Credential::Session(access));
    }
    let received_at = now();
    let answer = server
        .post_form::<Refreshed>(
            "/oauth/token",
            &[
                ("grant_type", "refresh_token"),
                ("client_id", CLIENT_ID),
                ("refresh_token", &tokens.refresh),
            ],
        )
        .await;
    let answer = match answer {
        Ok(answer) => answer,
        Err(e) if e.code() == Some("invalid_grant") => {
            let _ = store.delete(origin);
            return Err(CliError::not_signed_in(
                "your Sekisho session has ended (signed out, expired or revoked)",
            ));
        }
        Err(e) => return Err(e.into()),
    };
    let refreshed = Tokens {
        access: answer.access_token,
        access_expires_at: received_at + time::Duration::seconds(answer.expires_in),
        refresh: answer.refresh_token,
    };
    store
        .save(origin, &refreshed)
        .map_err(|e| CliError::error(format!("{e}: the refreshed session can't be kept")))?;
    Ok(Credential::Session(refreshed.access))
}

fn load(store: &dyn Store, origin: &str) -> Result<Option<Tokens>, CliError> {
    store
        .load(origin)
        .map_err(|e| CliError::error(e.to_string()))
}

/// The access token, while it has more than `REUSE_MARGIN` left.
fn fresh(stored: &Option<Tokens>, now: OffsetDateTime) -> Option<String> {
    stored
        .as_ref()
        .filter(|t| t.access_expires_at - REUSE_MARGIN > now)
        .map(|t| t.access.clone())
}

/// An exclusive lock on `path`, held until the file is dropped.
async fn take_lock(path: &Path) -> Result<File, CliError> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)?;
        file.lock()?;
        Ok::<_, std::io::Error>(file)
    })
    .await
    .map_err(|e| CliError::error(e.to_string()))?
    .map_err(|e| CliError::error(format!("can't take the refresh lock: {e}")))
}

/// The token `sekisho run`'s proxy holds (Task 7): `GET <run URL>/token`
/// with the run's secret.
pub async fn from_run(url: &str, key: &str) -> Result<Credential, CliError> {
    let url = ServerUrl::parse(url)
        .ok()
        .filter(ServerUrl::is_plain)
        .ok_or_else(|| {
            CliError::error("SEKISHO_RUN_URL isn't a sekisho run proxy on this computer")
        })?;
    let proxy = Server::new(url)?;
    let token: TokenBody = proxy
        .get_json("/token", Some(&Credential::Key(key.to_owned())))
        .await?;
    Ok(Credential::Session(token.token))
}

#[derive(Deserialize)]
struct TokenBody {
    token: String,
}
