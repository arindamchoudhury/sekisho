//! The CLI's settings: the server URL rule (D197), the variables it reads,
//! its folders, and `config.json`, which holds the server URL and the
//! signed-in person's email only (D196). Tokens are never here.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use etcetera::BaseStrategy;
use serde::{Deserialize, Serialize};
use url::{Host, Url};

use crate::output::CliError;

/// The variables the CLI reads, captured once, so tests can give their own.
#[derive(Clone, Debug, Default)]
pub struct Vars(HashMap<String, String>);

impl Vars {
    /// The process environment's `SEKISHO_*` variables.
    pub fn from_process() -> Self {
        Self(
            std::env::vars()
                .filter(|(k, _)| k.starts_with("SEKISHO_"))
                .collect(),
        )
    }

    pub fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )
    }

    /// A variable, unless it is unset or empty.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .get(name)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
}

/// Why a server URL isn't one the CLI talks to.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UrlError {
    #[error("{0:?} isn't a URL")]
    NotAUrl(String),
    #[error("the server URL must start with https:// (or http:// to this computer)")]
    Scheme,
    #[error(
        "plain http:// is only for this computer (127.0.0.1, [::1] or localhost); use https://"
    )]
    PlainHttp,
    #[error("the server URL can't hold a user name or password")]
    Credentials,
    #[error("the server URL can't have a query or a fragment")]
    QueryOrFragment,
}

/// Sekisho's address: `https://`, or `http://` to a loopback address, with
/// no user information, query or fragment (D197). Kept without a trailing
/// slash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerUrl(Url);

impl ServerUrl {
    pub fn parse(text: &str) -> Result<Self, UrlError> {
        Self::parse_with(text, plain_http_allowed())
    }

    /// `parse`, where `plain_http` also accepts `http://` to any host (the
    /// e2e stack only).
    pub fn parse_with(text: &str, plain_http: bool) -> Result<Self, UrlError> {
        let url = Url::parse(text.trim()).map_err(|_| UrlError::NotAUrl(text.to_owned()))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(UrlError::Credentials);
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(UrlError::QueryOrFragment);
        }
        match url.scheme() {
            "https" => {}
            "http" if plain_http || is_loopback(&url) => {}
            "http" => return Err(UrlError::PlainHttp),
            _ => return Err(UrlError::Scheme),
        }
        if url.host().is_none() {
            return Err(UrlError::NotAUrl(text.to_owned()));
        }
        let mut url = url;
        let path = url.path().trim_end_matches('/').to_owned();
        url.set_path(&path);
        Ok(Self(url))
    }

    /// The URL without a trailing slash: the credential store's account and
    /// what `config.json` keeps.
    pub fn key(&self) -> &str {
        self.0.as_str().trim_end_matches('/')
    }

    /// One of the server's paths, such as `/oauth/token`.
    pub fn join(&self, path: &str) -> String {
        format!("{}{path}", self.key())
    }

    pub fn url(&self) -> &Url {
        &self.0
    }

    /// Whether this is plain `http://`.
    pub fn is_plain(&self) -> bool {
        self.0.scheme() == "http"
    }
}

impl std::fmt::Display for ServerUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.key())
    }
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => ip == Ipv4Addr::LOCALHOST,
        Some(Host::Ipv6(ip)) => ip == Ipv6Addr::LOCALHOST,
        Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// `http://` to any host: only an `e2e` build with
/// `SEKISHO_E2E_PLAIN_HTTP=1`.
fn plain_http_allowed() -> bool {
    cfg!(feature = "e2e") && std::env::var("SEKISHO_E2E_PLAIN_HTTP").as_deref() == Ok("1")
}

/// Where the CLI keeps its files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dirs {
    /// `config.json`: `~/.config/sekisho`, `%APPDATA%\sekisho` on Windows.
    pub config: PathBuf,
    /// Machine-local files such as `refresh.lock`: `~/.cache/sekisho`,
    /// `%LOCALAPPDATA%\sekisho` on Windows, never the roaming profile.
    pub local: PathBuf,
}

impl Dirs {
    /// Both folders at one path, as `SEKISHO_CONFIG_DIR` makes them.
    pub fn at(path: PathBuf) -> Self {
        Self {
            config: path.clone(),
            local: path,
        }
    }

    /// `SEKISHO_CONFIG_DIR` when set, else the platform's folders.
    pub fn from_vars(vars: &Vars) -> Result<Self, CliError> {
        if let Some(dir) = vars.get("SEKISHO_CONFIG_DIR") {
            return Ok(Self::at(PathBuf::from(dir)));
        }
        let base = etcetera::choose_base_strategy()
            .map_err(|_| CliError::error("can't find your home folder; set SEKISHO_CONFIG_DIR"))?;
        Ok(Self {
            config: base.config_dir().join("sekisho"),
            local: base.cache_dir().join("sekisho"),
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.json")
    }

    /// The lock a refresh runs under (Global Constraints).
    pub fn refresh_lock(&self) -> PathBuf {
        self.local.join("refresh.lock")
    }
}

/// `config.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl Config {
    /// The file, or an empty config when there is none.
    pub fn load(dirs: &Dirs) -> Result<Self, CliError> {
        let path = dirs.config_file();
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| CliError::error(format!("{} isn't valid: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(CliError::error(format!(
                "can't read {}: {e}",
                path.display()
            ))),
        }
    }

    /// Writes the file atomically, readable by its owner only on Unix.
    pub fn save(&self, dirs: &Dirs) -> Result<(), CliError> {
        let path = dirs.config_file();
        let text = serde_json::to_vec_pretty(self).expect("config serialises");
        write_private(&path, &text)
            .map_err(|e| CliError::error(format!("can't write {}: {e}", path.display())))
    }

    /// The server: `SEKISHO_URL` first, then `config.json`. Reads no file
    /// when `SEKISHO_URL` is set.
    pub fn server(vars: &Vars, dirs: &Dirs) -> Result<ServerUrl, CliError> {
        let text = match vars.get("SEKISHO_URL") {
            Some(url) => url.to_owned(),
            None => Self::load(dirs)?.server.ok_or_else(|| {
                CliError::not_signed_in("no Sekisho server is set")
                    .with_hint("run `sekisho login <server URL>`, or set SEKISHO_URL")
            })?,
        };
        ServerUrl::parse(&text).map_err(|e| CliError::usage(e.to_string()))
    }
}

/// Writes `bytes` to `path` through a temporary file in the same folder and
/// a rename, so a reader sees the old file or the new one.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e2e_plain_http_reaches_any_host_only_when_asked() {
        assert_eq!(
            ServerUrl::parse_with("http://sekisho:8080", false),
            Err(UrlError::PlainHttp)
        );
        assert!(ServerUrl::parse_with("http://sekisho:8080", true).is_ok());
    }

    /// The refresh lock stays out of the roaming profile, which folder
    /// redirection can put on a network share.
    #[cfg(windows)]
    #[test]
    fn windows_keeps_the_lock_off_the_roaming_profile() {
        let dirs = Dirs::from_vars(&Vars::default()).unwrap();
        let local = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap());
        let roaming = PathBuf::from(std::env::var("APPDATA").unwrap());
        assert_eq!(
            dirs.refresh_lock(),
            local.join("sekisho").join("refresh.lock")
        );
        assert_eq!(
            dirs.config_file(),
            roaming.join("sekisho").join("config.json")
        );
    }

    #[test]
    fn a_path_is_kept_without_its_slash() {
        let url = ServerUrl::parse("https://s.example.com/sekisho/").unwrap();
        assert_eq!(url.key(), "https://s.example.com/sekisho");
        assert_eq!(
            url.join("/oauth/token"),
            "https://s.example.com/sekisho/oauth/token"
        );
    }

    #[test]
    fn config_round_trips_and_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().join("nested"));
        assert_eq!(Config::load(&dirs).unwrap(), Config::default());
        let config = Config {
            server: Some("https://s.example.com".into()),
            email: Some("ada@example.com".into()),
        };
        config.save(&dirs).unwrap();
        assert_eq!(Config::load(&dirs).unwrap(), config);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dirs.config_file())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn sekisho_url_comes_first() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::at(dir.path().to_owned());
        Config {
            server: Some("https://from-file.example.com".into()),
            email: None,
        }
        .save(&dirs)
        .unwrap();
        let vars = Vars::from_pairs(&[("SEKISHO_URL", "https://from-env.example.com")]);
        assert_eq!(
            Config::server(&vars, &dirs).unwrap().key(),
            "https://from-env.example.com"
        );
        assert_eq!(
            Config::server(&Vars::default(), &dirs).unwrap().key(),
            "https://from-file.example.com"
        );
        let empty = Dirs::at(dir.path().join("none"));
        assert_eq!(
            Config::server(&Vars::default(), &empty).unwrap_err().exit,
            crate::output::Exit::NotSignedIn
        );
    }
}
