//! The commands as the `sekisho` binary runs them: the process's variables
//! and folders, the platform's credential store and the terminal.

use std::io::Write;

use crate::config::{Config, Dirs, ServerUrl, Vars};
use crate::credentials::open_store;
use crate::login::{self, Method, TerminalUi, WAIT, no_keyring_error};
use crate::output::{CliError, note};
use crate::server::Server;
use crate::token::{self, Sources};

/// `sekisho login [<server URL>] [--device] [--no-browser]`.
pub async fn login(server: Option<String>, device: bool, no_browser: bool) -> Result<(), CliError> {
    let vars = Vars::from_process();
    let dirs = Dirs::from_vars(&vars)?;
    let url = match server {
        Some(text) => ServerUrl::parse(&text).map_err(|e| CliError::usage(e.to_string()))?,
        None => Config::server(&vars, &dirs).map_err(|_| {
            CliError::usage("which Sekisho server? run `sekisho login <server URL>`")
        })?,
    };
    let opened = open_store();
    if opened.no_keyring {
        return Err(no_keyring_error());
    }
    let server = Server::new(url)?;
    let method = if device {
        Method::Device
    } else {
        Method::Browser
    };
    let ui = TerminalUi {
        open_browser: !no_browser,
    };
    let signed = login::login(&server, method, &ui, opened.store.as_ref(), &dirs, WAIT).await?;
    note(&format!(
        "Signed in to {} as {}.",
        server.url(),
        signed.email
    ));
    Ok(())
}

/// `sekisho logout`.
pub async fn logout() -> Result<(), CliError> {
    let vars = Vars::from_process();
    let dirs = Dirs::from_vars(&vars)?;
    let server = Server::new(Config::server(&vars, &dirs)?)?;
    let opened = open_store();
    login::logout(&server, opened.store.as_ref(), &dirs).await?;
    note(&format!("Signed out of {}.", server.url()));
    Ok(())
}

/// `sekisho token`: the credential's secret and a newline on stdout,
/// nothing else.
pub async fn token() -> Result<(), CliError> {
    let vars = Vars::from_process();
    let sources = Sources::from_vars(&vars);
    let credential = if let Some(key) = &sources.key {
        crate::credentials::Credential::Key(key.clone())
    } else if let Some((url, key)) = &sources.run {
        token::from_run(url, key).await?
    } else {
        let dirs = Dirs::from_vars(&vars)?;
        let server = Server::new(Config::server(&vars, &dirs)?)?;
        let opened = open_store();
        token::current(
            &server,
            opened.store.as_ref(),
            &sources,
            &dirs.refresh_lock(),
        )
        .await?
    };
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", credential.secret())
        .and_then(|()| out.flush())
        .map_err(|e| CliError::error(format!("can't write the token: {e}")))
}
