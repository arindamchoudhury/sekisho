//! The commands as the `sekisho` binary runs them: the process's variables
//! and folders, the platform's credential store and the terminal.
//!
//! `env list` and `keys` run on a [`Context`], so tests can give them
//! their own folders and store; they return what goes to stdout.

mod env;
mod keys;
mod use_;

use std::io::Write;
use std::path::{Path, PathBuf};

use time::OffsetDateTime;

use crate::config::{Config, Dirs, ServerUrl, Vars};
use crate::credentials::{Credential, Store, open_store};
use crate::install::{Scope, ToolHomes};
use crate::login::{self, Method, TerminalUi, WAIT, no_keyring_error};
use crate::output::{CliError, note};
use crate::server::Server;
use crate::token::{self, Sources};

pub use env::env_list;
pub use keys::{KeyAction, keys_act, keys_create, keys_list};
pub use use_::{UninstallArgs, UseArgs, uninstall, use_, use_notices};

/// What a command runs with.
pub struct Context<'a> {
    pub vars: Vars,
    pub dirs: Dirs,
    pub store: &'a dyn Store,
    /// The folder it runs in: a project's root.
    pub folder: PathBuf,
    /// The user's home folder.
    pub home: PathBuf,
    /// `sekisho` as the user would type it: `sekisho` when that is this
    /// program on `PATH`, else its absolute path.
    pub sekisho: String,
    pub now: OffsetDateTime,
}

impl Context<'_> {
    /// The server and a signed-in person's session token. These commands
    /// act as a person, so an environment key (`SEKISHO_KEY`) won't do.
    pub(crate) async fn person(&self) -> Result<(Server, Credential), CliError> {
        if self.vars.get("SEKISHO_KEY").is_some() {
            return Err(CliError::not_signed_in(
                "this command acts as a signed-in person; SEKISHO_KEY is an environment key",
            )
            .with_hint("unset SEKISHO_KEY and run `sekisho login`"));
        }
        let server = Server::new(Config::server(&self.vars, &self.dirs)?)?;
        let credential = token::current(
            &server,
            self.store,
            &Sources::from_vars(&self.vars),
            &self.dirs.refresh_lock(),
        )
        .await?;
        Ok((server, credential))
    }

    /// The server and this machine's credential: an environment key
    /// (`SEKISHO_KEY`) or the signed-in person's session.
    pub(crate) async fn caller(&self) -> Result<(Server, Credential), CliError> {
        let server = Server::new(Config::server(&self.vars, &self.dirs)?)?;
        let credential = token::current(
            &server,
            self.store,
            &Sources::from_vars(&self.vars),
            &self.dirs.refresh_lock(),
        )
        .await?;
        Ok((server, credential))
    }

    /// The project folder's install (`dir`, else the current folder), or the
    /// global one.
    pub fn scope(&self, global: bool, dir: Option<&std::path::Path>) -> Scope {
        let homes = ToolHomes::from_env(&|k| self.vars.get(k).map(str::to_owned), &self.home);
        if global {
            Scope::global(&self.dirs.config, homes)
        } else {
            Scope::project(dir.unwrap_or(&self.folder), &self.dirs.config, homes)
        }
    }
}

/// `sekisho` as the user would type it.
fn sekisho_command() -> String {
    let Ok(me) = std::env::current_exe() else {
        return "sekisho".to_owned();
    };
    let me = std::fs::canonicalize(&me).unwrap_or(me);
    let name = if cfg!(windows) {
        "sekisho.exe"
    } else {
        "sekisho"
    };
    let on_path = std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
            .and_then(|p| std::fs::canonicalize(p).ok())
            .is_some_and(|p| p == me)
    });
    if on_path {
        "sekisho".to_owned()
    } else {
        shown(&me)
    }
}

/// A path as a shell runs it: Windows' verbatim form (`\\?\C:\…`, which
/// `canonicalize` gives) as the ordinary one, since `cmd` can't run it.
fn shown(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(local) = text.strip_prefix(r"\\?\") {
        local.to_owned()
    } else {
        text.into_owned()
    }
}

/// The user's home folder.
fn home_dir() -> Result<PathBuf, CliError> {
    etcetera::home_dir().map_err(|e| CliError::error(format!("can't find your home folder: {e}")))
}

/// Runs `command` on this process's variables, folders and keyring, and
/// prints its answer on stdout.
pub async fn run_here<F>(command: F) -> Result<(), CliError>
where
    F: AsyncFnOnce(&Context<'_>) -> Result<String, CliError>,
{
    let vars = Vars::from_process();
    let dirs = Dirs::from_vars(&vars)?;
    let opened = open_store();
    let folder = std::env::current_dir()
        .map_err(|e| CliError::error(format!("can't read the current folder: {e}")))?;
    let context = Context {
        vars,
        dirs,
        store: opened.store.as_ref(),
        folder,
        home: home_dir()?,
        sekisho: sekisho_command(),
        now: OffsetDateTime::now_utc(),
    };
    let text = command(&context).await?;
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|e| CliError::error(format!("can't write the answer: {e}")))
}
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_shown_as_a_shell_runs_it() {
        assert_eq!(
            shown(Path::new(r"\\?\C:\Tools\sekisho.exe")),
            r"C:\Tools\sekisho.exe"
        );
        assert_eq!(
            shown(Path::new(r"\\?\UNC\server\share\sekisho.exe")),
            r"\\server\share\sekisho.exe"
        );
        assert_eq!(
            shown(Path::new("/usr/local/bin/sekisho")),
            "/usr/local/bin/sekisho"
        );
    }
}
