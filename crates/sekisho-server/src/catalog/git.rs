//! The server's `git`, hardened as ACH does and more (D165): only `https`
//! (plain `http` where allowed), no redirects, no hooks, no symlinks, no
//! submodules, no system or global config, no prompts, no proxy; the host
//! pinned to the addresses Sekisho checked (`http.curloptResolve`); the
//! token in an `http.extraHeader` set through the environment, never in
//! the arguments or the URL; a checkout stopped past its size limit.
//!
//! `http.curloptResolve` needs git 2.37 or later; an older git ignores it.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine;
use sekisho_catalog::source::{GitRef, SourceUrl};
use tokio::io::AsyncReadExt;

/// A checkout may grow to this size (D165).
pub const CHECKOUT_LIMIT: u64 = 100 << 20;
/// A git error is cut to this many bytes.
const ERROR_LIMIT: usize = 512;
/// How often the checkout's size is measured.
const WATCH_EVERY: Duration = Duration::from_millis(500);
/// The username a token goes with when the source names none.
pub const DEFAULT_USERNAME: &str = "x-access-token";
/// The environment kept from the server's: what `git` needs to start and
/// to find its helpers. Nothing else, so no proxy, `GIT_*` or SSL setting
/// of the server's reaches it.
const KEPT_ENV: [&str; 8] = [
    "PATH",
    "SYSTEMROOT",
    "SystemRoot",
    "SYSTEMDRIVE",
    "WINDIR",
    "TEMP",
    "TMP",
    "TMPDIR",
];

#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";
#[cfg(not(windows))]
const NULL_DEVICE: &str = "/dev/null";

/// What to read, and how.
pub struct GitSource<'a> {
    pub url: &'a SourceUrl,
    /// `None` reads the remote's `HEAD`.
    pub git_ref: Option<&'a GitRef>,
    pub username: &'a str,
    pub token: Option<&'a str>,
    /// The host's checked addresses; git connects to these alone.
    pub pins: &'a [SocketAddr],
}

/// Why git failed: its own message, cut short, with no secret in it.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct GitError(String);

/// A command line and its whole environment.
pub(crate) struct Invocation {
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
}

#[derive(Clone, Debug)]
pub struct Git {
    program: PathBuf,
    allow_plain_http: bool,
    limit: u64,
}

impl Git {
    pub fn new(program: PathBuf, allow_plain_http: bool) -> Self {
        Self {
            program,
            allow_plain_http,
            limit: CHECKOUT_LIMIT,
        }
    }

    /// Another size limit, for tests.
    pub fn with_limit(mut self, bytes: u64) -> Self {
        self.limit = bytes;
        self
    }

    /// The `Authorization: Basic` value for the source's token.
    fn basic(src: &GitSource<'_>) -> Option<String> {
        let token = src.token?;
        let engine = base64::engine::general_purpose::STANDARD;
        Some(engine.encode(format!("{}:{token}", src.username)))
    }

    /// The hardened command line for `args` and its environment. `home` is
    /// an empty folder of the call's own.
    pub(crate) fn invocation(
        &self,
        src: &GitSource<'_>,
        home: &Path,
        dir: Option<&Path>,
        args: &[&str],
    ) -> Invocation {
        let mut config: Vec<String> = vec![
            "protocol.allow=never".into(),
            "protocol.https.allow=always".into(),
        ];
        if self.allow_plain_http && src.url.as_url().scheme() == "http" {
            config.push("protocol.http.allow=always".into());
        }
        config.extend([
            "http.followRedirects=false".into(),
            "http.sslVerify=true".into(),
            format!("core.hooksPath={}", home.join("no-hooks").display()),
            "core.fsmonitor=false".into(),
            "core.symlinks=false".into(),
            "credential.helper=".into(),
            "submodule.recurse=false".into(),
            "init.templateDir=".into(),
        ]);
        if let url::Host::Domain(host) = src.url.host() {
            // host:port:addr1,addr2, IPv6 addresses in brackets.
            let addresses: Vec<String> = src
                .pins
                .iter()
                .map(|a| match a {
                    SocketAddr::V4(v4) => v4.ip().to_string(),
                    SocketAddr::V6(v6) => format!("[{}]", v6.ip()),
                })
                .collect();
            config.push(format!(
                "http.curloptResolve={host}:{}:{}",
                src.url.port(),
                addresses.join(",")
            ));
        }
        let mut out: Vec<OsString> = Vec::new();
        for setting in config {
            out.push("-c".into());
            out.push(setting.into());
        }
        if let Some(dir) = dir {
            out.push("-C".into());
            out.push(dir.as_os_str().to_owned());
        }
        out.extend(args.iter().map(OsString::from));

        let mut env: Vec<(OsString, OsString)> = KEPT_ENV
            .iter()
            .filter_map(|k| std::env::var_os(k).map(|v| (OsString::from(k), v)))
            .collect();
        for (key, value) in [
            ("HOME", home.as_os_str().to_owned()),
            ("XDG_CONFIG_HOME", home.as_os_str().to_owned()),
            ("GIT_TERMINAL_PROMPT", "0".into()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_CONFIG_GLOBAL", NULL_DEVICE.into()),
            ("GIT_HTTP_LOW_SPEED_LIMIT", "1000".into()),
            ("GIT_HTTP_LOW_SPEED_TIME", "60".into()),
        ] {
            env.push((key.into(), value));
        }
        if let Some(basic) = Self::basic(src) {
            env.push(("GIT_CONFIG_COUNT".into(), "1".into()));
            env.push(("GIT_CONFIG_KEY_0".into(), "http.extraHeader".into()));
            env.push((
                "GIT_CONFIG_VALUE_0".into(),
                format!("Authorization: Basic {basic}").into(),
            ));
        }
        Invocation { args: out, env }
    }

    /// Removes the token and its Basic value from git's words, and cuts
    /// them short.
    fn clean(&self, src: &GitSource<'_>, text: &str) -> String {
        let mut text = text.trim().to_owned();
        if let Some(basic) = Self::basic(src) {
            text = text.replace(&basic, "[redacted]");
        }
        if let Some(token) = src.token.filter(|t| !t.is_empty()) {
            text = text.replace(token, "[redacted]");
        }
        if text.len() > ERROR_LIMIT {
            let mut end = ERROR_LIMIT;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        text
    }

    /// Runs one command; with `watch`, stops it once that folder passes
    /// the size limit. Answers its output.
    async fn run(
        &self,
        src: &GitSource<'_>,
        dir: Option<&Path>,
        args: &[&str],
        watch: Option<&Path>,
    ) -> Result<String, GitError> {
        let home = tempfile::tempdir()
            .map_err(|e| GitError(format!("can't make a folder for git: {e}")))?;
        let call = self.invocation(src, home.path(), dir, args);
        let mut child = tokio::process::Command::new(&self.program)
            .args(&call.args)
            .env_clear()
            .envs(call.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| GitError(format!("can't run git: {e}")))?;
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");
        let out = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf).await;
            buf
        });
        let err = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            buf
        });
        let status = match watch {
            None => child.wait().await,
            Some(folder) => loop {
                tokio::select! {
                    status = child.wait() => break status,
                    () = tokio::time::sleep(WATCH_EVERY) => {
                        let folder = folder.to_path_buf();
                        let size = tokio::task::spawn_blocking(move || folder_size(&folder))
                            .await
                            .unwrap_or(0);
                        if size > self.limit {
                            let _ = child.kill().await;
                            return Err(GitError(format!(
                                "the repository is larger than {} MiB",
                                self.limit >> 20
                            )));
                        }
                    }
                }
            },
        }
        .map_err(|e| GitError(format!("git didn't finish: {e}")))?;
        let stdout = out.await.unwrap_or_default();
        let stderr = err.await.unwrap_or_default();
        if !status.success() {
            let said = String::from_utf8_lossy(&stderr);
            let said = self.clean(src, &said);
            return Err(GitError(if said.is_empty() {
                format!("git {} failed", args.first().unwrap_or(&""))
            } else {
                said
            }));
        }
        // A last measure: the command may have finished between two.
        if let Some(folder) = watch {
            let folder = folder.to_path_buf();
            let size = tokio::task::spawn_blocking(move || folder_size(&folder))
                .await
                .unwrap_or(0);
            if size > self.limit {
                return Err(GitError(format!(
                    "the repository is larger than {} MiB",
                    self.limit >> 20
                )));
            }
        }
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    }

    /// The commit the source's ref names, and that ref in full.
    async fn resolve(&self, src: &GitSource<'_>) -> Result<(String, String), GitError> {
        let url = src.url.as_str();
        let wanted: Vec<String> = match src.git_ref {
            None => vec!["HEAD".to_owned()],
            Some(r) if r.as_str().starts_with("refs/") => vec![r.as_str().to_owned()],
            Some(r) => vec![format!("refs/heads/{r}"), format!("refs/tags/{r}")],
        };
        let mut args: Vec<&str> = vec!["ls-remote", "--", url];
        args.extend(wanted.iter().map(String::as_str));
        let listing = self.run(src, None, &args, None).await?;
        let mut found: Vec<(String, String)> = Vec::new();
        for line in listing.lines() {
            if let Some((sha, name)) = line.split_once('\t') {
                found.push((name.trim().to_owned(), sha.trim().to_owned()));
            }
        }
        let look = |name: &str| {
            found
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, s)| s.clone())
        };
        for name in &wanted {
            // An annotated tag's commit is on its peeled line.
            let sha = look(&format!("{name}^{{}}")).or_else(|| look(name));
            if let Some(sha) = sha {
                if sha.len() == 40 && sha.bytes().all(|c| c.is_ascii_hexdigit()) {
                    return Ok((sha.to_lowercase(), name.clone()));
                }
                return Err(GitError(
                    "git answered something that isn't a commit".to_owned(),
                ));
            }
        }
        Err(GitError(match src.git_ref {
            Some(r) => format!("the ref {r} isn't in the repository"),
            None => "the repository has no HEAD".to_owned(),
        }))
    }

    /// The commit the source's ref names now, in 40 hex digits.
    pub async fn ls_remote(&self, src: &GitSource<'_>) -> Result<String, GitError> {
        self.resolve(src).await.map(|(sha, _)| sha)
    }

    /// Fetches the source's ref into the empty folder `dest`, one commit
    /// deep, and checks it out; answers the commit.
    pub async fn checkout(&self, src: &GitSource<'_>, dest: &Path) -> Result<String, GitError> {
        let (_, refname) = self.resolve(src).await?;
        let url = src.url.as_str();
        self.run(src, Some(dest), &["init", "-q"], Some(dest))
            .await?;
        self.run(
            src,
            Some(dest),
            &["fetch", "-q", "--depth=1", "--no-tags", "--", url, &refname],
            Some(dest),
        )
        .await?;
        self.run(
            src,
            Some(dest),
            &["checkout", "-q", "--detach", "FETCH_HEAD"],
            Some(dest),
        )
        .await?;
        let commit = self
            .run(src, Some(dest), &["rev-parse", "FETCH_HEAD^{commit}"], None)
            .await?;
        Ok(commit.trim().to_lowercase())
    }
}

/// The bytes under `folder`, symlinks not followed.
fn folder_size(folder: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![folder.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(items) = std::fs::read_dir(&dir) else {
            continue;
        };
        for item in items.flatten() {
            let Ok(kind) = item.file_type() else { continue };
            if kind.is_dir() {
                stack.push(item.path());
            } else if let Ok(meta) = item.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_git_call_is_hardened() {
        let url = SourceUrl::parse("https://git.example.com/team/skills.git", false).unwrap();
        let reference = GitRef::parse("main").unwrap();
        let pins: Vec<SocketAddr> = vec![
            "192.0.2.10:443".parse().unwrap(),
            "[2001:db8::1]:443".parse().unwrap(),
        ];
        let token = "s3cr3t-token";
        let src = GitSource {
            url: &url,
            git_ref: Some(&reference),
            username: "robot",
            token: Some(token),
            pins: &pins,
        };
        let home = Path::new("/tmp/sekisho-git-home");
        let git = Git::new("git".into(), false);
        let call = git.invocation(
            &src,
            home,
            None,
            &["ls-remote", "--", url.as_str(), "refs/heads/main"],
        );
        let args: Vec<String> = call
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        // The pins come first, each after -c.
        let pinned: Vec<&str> = args
            .iter()
            .skip(1)
            .step_by(2)
            .take(11)
            .map(String::as_str)
            .collect();
        assert!(
            args.iter().step_by(2).take(11).all(|a| a == "-c"),
            "{args:?}"
        );
        for setting in [
            "protocol.allow=never",
            "protocol.https.allow=always",
            "http.followRedirects=false",
            "http.sslVerify=true",
            "core.fsmonitor=false",
            "core.symlinks=false",
            "credential.helper=",
            "submodule.recurse=false",
            "http.curloptResolve=git.example.com:443:192.0.2.10,[2001:db8::1]",
        ] {
            assert!(pinned.contains(&setting), "{setting} missing: {args:?}");
        }
        assert!(
            !pinned.iter().any(|s| s.starts_with("protocol.http.")),
            "{args:?}"
        );
        // The URL follows `--`, so it can't be read as an option.
        let dashes = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(args[dashes + 1], url.as_str());
        assert!(!args.iter().any(|a| a.contains(token)), "{args:?}");

        let env: std::collections::HashMap<String, String> = call
            .env
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert_eq!(env["GIT_CONFIG_COUNT"], "1");
        assert_eq!(env["GIT_CONFIG_KEY_0"], "http.extraHeader");
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("robot:{token}"));
        assert_eq!(
            env["GIT_CONFIG_VALUE_0"],
            format!("Authorization: Basic {basic}")
        );
        assert_eq!(env["GIT_TERMINAL_PROMPT"], "0");
        assert_eq!(env["GIT_CONFIG_NOSYSTEM"], "1");
        assert_eq!(env["GIT_CONFIG_GLOBAL"], NULL_DEVICE);
        assert_eq!(env["HOME"], home.to_string_lossy());
        for key in env.keys() {
            assert!(
                KEPT_ENV.contains(&key.as_str())
                    || key.starts_with("GIT_")
                    || key == "HOME"
                    || key == "XDG_CONFIG_HOME",
                "{key} leaks from the server's environment"
            );
        }
        assert!(!env.values().any(|v| v.contains(token)));

        // Plain http only where allowed, and only for an http source.
        let plain = SourceUrl::parse("http://127.0.0.1:8080/r.git", true).unwrap();
        let src = GitSource {
            url: &plain,
            git_ref: None,
            username: DEFAULT_USERNAME,
            token: None,
            pins: &[],
        };
        let allowed = Git::new("git".into(), true).invocation(&src, home, None, &["ls-remote"]);
        assert!(
            allowed
                .args
                .iter()
                .any(|a| a == "protocol.http.allow=always")
        );
        assert!(
            !allowed
                .args
                .iter()
                .any(|a| a.to_string_lossy().starts_with("http.curloptResolve"))
        );
        assert!(!allowed.env.iter().any(|(k, _)| k == "GIT_CONFIG_COUNT"));
    }

    #[test]
    fn git_errors_lose_the_token_and_are_cut() {
        let url = SourceUrl::parse("https://git.example.com/r.git", false).unwrap();
        let src = GitSource {
            url: &url,
            git_ref: None,
            username: "u",
            token: Some("tok-123"),
            pins: &[],
        };
        let git = Git::new("git".into(), false);
        let basic = base64::engine::general_purpose::STANDARD.encode("u:tok-123");
        let said = git.clean(
            &src,
            &format!("fatal: tok-123 and {basic} {}", "x".repeat(600)),
        );
        assert!(
            !said.contains("tok-123") && !said.contains(&basic),
            "{said}"
        );
        assert!(said.len() <= ERROR_LIMIT);
    }
}
