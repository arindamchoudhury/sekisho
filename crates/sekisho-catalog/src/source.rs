//! A catalog source's settings: the repository's URL, the ref to follow
//! and the folder to read. Each reaches `git`'s command line or a path, so
//! none may look like an option or leave the checkout.

use std::fmt;
use std::path::{Path, PathBuf};

/// Why a setting was refused. It never quotes a URL, which may hold a
/// password.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid {what}: {reason}")]
pub struct SettingError {
    what: &'static str,
    reason: &'static str,
}

fn refuse<T>(what: &'static str, reason: &'static str) -> Result<T, SettingError> {
    Err(SettingError { what, reason })
}

const MAX_URL: usize = 2048;

/// The checks every outside URL in a catalog takes: `https://` (or
/// `http://` where allowed), a host, no user info or fragment, and nothing
/// a parser might read two ways. `query` says whether a query may follow.
fn parse_url(
    s: &str,
    what: &'static str,
    allow_plain_http: bool,
    query: bool,
) -> Result<url::Url, SettingError> {
    if s.is_empty() || s.len() > MAX_URL {
        return refuse(what, "give an https:// URL of at most 2048 characters");
    }
    if s.starts_with('-') {
        return refuse(what, "it can't start with -");
    }
    if s.chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return refuse(
            what,
            "it can't hold spaces, control characters or backslashes",
        );
    }
    // The URL parser forgives `https:///host`; git might not read it alike.
    let after_scheme = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"));
    if after_scheme.is_some_and(|rest| rest.starts_with('/')) {
        return refuse(what, "it needs a host after the scheme");
    }
    let Ok(url) = url::Url::parse(s) else {
        return refuse(what, "it isn't a URL");
    };
    match url.scheme() {
        "https" => {}
        "http" if allow_plain_http => {}
        "http" => return refuse(what, "use https://"),
        _ => return refuse(what, "only https:// URLs are allowed"),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return refuse(what, "credentials don't belong in the URL");
    }
    if url.fragment().is_some() {
        return refuse(what, "it can't have a fragment");
    }
    if !query && url.query().is_some() {
        return refuse(what, "it can't have a query");
    }
    if url.host_str().is_none_or(str::is_empty) {
        return refuse(what, "it needs a host");
    }
    Ok(url)
}

/// A remote MCP server's or an A2A agent's address in a catalog: as
/// [`SourceUrl`], but a query is allowed. Answers the parser's normal form.
pub fn endpoint_url(
    s: &str,
    what: &'static str,
    allow_plain_http: bool,
) -> Result<String, SettingError> {
    parse_url(s, what, allow_plain_http, true).map(String::from)
}

/// A repository's address: `https://` (or `http://` where allowed), with a
/// host, and no user info, query or fragment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceUrl(url::Url);

impl SourceUrl {
    /// `allow_plain_http` is for tests and the e2e build only.
    pub fn parse(s: &str, allow_plain_http: bool) -> Result<Self, SettingError> {
        parse_url(s, "source URL", allow_plain_http, false).map(Self)
    }

    pub fn host(&self) -> url::Host<String> {
        self.0.host().expect("checked at parse").to_owned()
    }

    pub fn port(&self) -> u16 {
        self.0
            .port_or_known_default()
            .expect("http and https have one")
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn as_url(&self) -> &url::Url {
        &self.0
    }
}

impl fmt::Display for SourceUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `[A-Za-z0-9._/-]`, the characters a ref and a folder may hold.
fn is_setting_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"._/-".contains(&c)
}

/// A branch, tag or other ref to follow.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GitRef(String);

impl GitRef {
    pub fn parse(s: &str) -> Result<Self, SettingError> {
        const WHAT: &str = "source ref";
        if !(1..=255).contains(&s.len()) || !s.bytes().all(is_setting_char) {
            return refuse(WHAT, "use 1-255 letters, digits or the characters . _ / -");
        }
        if s.starts_with(['-', '/']) {
            return refuse(WHAT, "it can't start with - or /");
        }
        if s.contains("..") || s.contains("//") || s.contains("@{") {
            return refuse(WHAT, "it can't hold .., // or @{");
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GitRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The folder of the repository a source reads; empty is the root.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Folder(String);

impl Folder {
    pub fn parse(s: &str) -> Result<Self, SettingError> {
        const WHAT: &str = "source folder";
        if s.is_empty() {
            return Ok(Self::default());
        }
        if s.len() > 255 || !s.bytes().all(is_setting_char) {
            return refuse(
                WHAT,
                "use at most 255 letters, digits or the characters . _ / -",
            );
        }
        if s.split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
        {
            return refuse(
                WHAT,
                "give a path inside the repository, without /, . or .. segments at either end or between",
            );
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The folder inside `root`, a checkout.
    pub fn join(&self, root: &Path) -> PathBuf {
        let mut path = root.to_path_buf();
        if !self.0.is_empty() {
            path.extend(self.0.split('/'));
        }
        path
    }
}

/// Hosts whose repositories Obot reads without `.git`: `<owner>/<repo>`,
/// then an optional branch.
const KNOWN_GIT_HOSTS: [&str; 3] = ["github.com", "gitlab.com", "bitbucket.org"];

/// An Obot catalog repository URL, read as Obot reads one
/// (`pkg/git/git.go` `parseGitURL`, Obot `5e471f101`), D176.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObotRepository {
    /// What git clones: the path up to and including `<repo>.git`.
    pub url: SourceUrl,
    /// The branch after the repository, if any. Without one Sekisho
    /// follows the repository's default branch; Obot assumes `main`.
    pub git_ref: Option<GitRef>,
    /// The repository's name, `.git` removed.
    pub repo: String,
}

impl ObotRepository {
    /// Reads the forms Obot accepts: `https://github.com/<org>/<repo>` (and
    /// GitLab's and Bitbucket's) with an optional branch after it, any
    /// host's `…/<repo>.git` with an optional branch after it, and either
    /// without `https://`, which is then assumed.
    pub fn parse(s: &str, allow_plain_http: bool) -> Result<Self, SettingError> {
        const WHAT: &str = "Obot catalog URL";
        let with_scheme;
        let s = if s.contains("://") {
            s
        } else {
            with_scheme = format!("https://{s}");
            &with_scheme
        };
        let url = parse_url(s, WHAT, allow_plain_http, false)?;
        let segments: Vec<&str> = url.path().trim_matches('/').split('/').collect();
        if segments.iter().any(|seg| seg.is_empty()) {
            return refuse(WHAT, "give the repository's path, such as /<org>/<repo>");
        }
        let host = url.host_str().unwrap_or_default();
        let (repo_path, rest) = match segments.iter().position(|seg| seg.ends_with(".git")) {
            Some(i) => (segments[..=i].join("/"), &segments[i + 1..]),
            None if KNOWN_GIT_HOSTS.contains(&host) && segments.len() >= 2 => (
                format!("{}/{}.git", segments[0], segments[1]),
                &segments[2..],
            ),
            None => {
                return refuse(
                    WHAT,
                    "give the repository's URL ending in .git, with an optional branch after it",
                );
            }
        };
        let repo = repo_path
            .rsplit('/')
            .next()
            .and_then(|r| r.strip_suffix(".git"))
            .unwrap_or_default()
            .to_owned();
        if repo.is_empty() {
            return refuse(WHAT, "give the repository's name before .git");
        }
        let mut clone = url.clone();
        clone.set_path(&format!("/{repo_path}"));
        let url = SourceUrl::parse(clone.as_str(), allow_plain_http)?;
        let git_ref = if rest.is_empty() {
            None
        } else {
            Some(GitRef::parse(&rest.join("/"))?)
        };
        Ok(Self { url, git_ref, repo })
    }

    /// The source name an import gives it: `obot-<repo>` as a slug, and
    /// `obot-<repo>-<n>` for `n` of 2 or more, when that is taken.
    pub fn source_name(&self, n: usize) -> crate::names::SourceName {
        let mut slug = String::new();
        for c in self.repo.chars() {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                slug.push(c);
            } else if !slug.is_empty() && !slug.ends_with('-') {
                slug.push('-');
            }
        }
        let suffix = if n >= 2 {
            format!("-{n}")
        } else {
            String::new()
        };
        let mut name = format!("obot-{slug}");
        name.truncate(63 - suffix.len());
        let mut name = name.trim_end_matches('-').to_owned();
        name.push_str(&suffix);
        crate::names::SourceName::parse(&name).expect("a slug of at most 63 characters")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obot(s: &str) -> (String, Option<String>, String) {
        let r = ObotRepository::parse(s, false).unwrap();
        (
            r.url.as_str().to_owned(),
            r.git_ref.map(|g| g.as_str().to_owned()),
            r.repo,
        )
    }

    #[test]
    fn obot_urls_are_read_as_obot_reads_them() {
        let gh = "https://github.com/obot-platform/mcp-catalog.git".to_owned();
        assert_eq!(
            obot("https://github.com/obot-platform/mcp-catalog"),
            (gh.clone(), None, "mcp-catalog".into())
        );
        assert_eq!(
            obot("https://github.com/obot-platform/mcp-catalog/"),
            (gh.clone(), None, "mcp-catalog".into())
        );
        assert_eq!(
            obot("github.com/obot-platform/mcp-catalog.git"),
            (gh.clone(), None, "mcp-catalog".into())
        );
        assert_eq!(
            obot("https://github.com/obot-platform/mcp-catalog/release"),
            (gh, Some("release".into()), "mcp-catalog".into())
        );
        assert_eq!(
            obot("https://gitlab.com/group/sub/repo.git/feature/catalog"),
            (
                "https://gitlab.com/group/sub/repo.git".into(),
                Some("feature/catalog".into()),
                "repo".into()
            )
        );
        assert_eq!(
            obot("https://git.example.com/a/b.git/dev"),
            (
                "https://git.example.com/a/b.git".into(),
                Some("dev".into()),
                "b".into()
            )
        );
        assert_eq!(
            obot("https://git.example.com:8443/b.git"),
            (
                "https://git.example.com:8443/b.git".into(),
                None,
                "b".into()
            )
        );
    }

    #[test]
    fn other_obot_urls_are_refused() {
        for bad in [
            "ssh://git@github.com/a/b.git",
            "git@github.com:a/b.git",
            "http://github.com/a/b",
            "https://git.example.com/a/b",
            "https://github.com/a",
            "https://github.com/a//b",
            "https://github.com/a/b?ref=x",
            "https://user:pw@github.com/a/b",
            "https://github.com/a/b/--upload-pack=x",
            "https://github.com/a/b/x..y",
            "https://git.example.com/.git",
        ] {
            assert!(ObotRepository::parse(bad, false).is_err(), "{bad}");
        }
        let err = ObotRepository::parse("https://user:secret@github.com/a/b", false)
            .unwrap_err()
            .to_string();
        assert!(!err.contains("secret"), "{err}");
    }

    #[test]
    fn imported_sources_are_named_after_the_repository() {
        let r = ObotRepository::parse("https://github.com/o/MCP_Catalog.git", false).unwrap();
        assert_eq!(r.source_name(1).as_str(), "obot-mcp-catalog");
        assert_eq!(r.source_name(2).as_str(), "obot-mcp-catalog-2");
        let long =
            ObotRepository::parse(&format!("https://h.example/{}.git", "x".repeat(80)), false)
                .unwrap();
        assert_eq!(long.source_name(1).as_str().len(), 63);
        assert_eq!(long.source_name(12).as_str().len(), 63);
        assert!(long.source_name(12).as_str().ends_with("-12"));
        let odd = ObotRepository::parse("https://h.example/___.git", false).unwrap();
        assert_eq!(odd.source_name(1).as_str(), "obot");
    }

    #[test]
    fn urls_keep_the_parsers_normal_form() {
        let u = SourceUrl::parse("https://GitHub.com/a/b", false).unwrap();
        assert_eq!(u.as_str(), "https://github.com/a/b");
        assert_eq!(
            u.host(),
            url::Host::<String>::Domain("github.com".to_owned())
        );
        let ip = SourceUrl::parse("https://[::1]:8443/r", false).unwrap();
        assert_eq!(
            ip.host(),
            url::Host::<String>::Ipv6(std::net::Ipv6Addr::LOCALHOST)
        );
        assert_eq!(ip.port(), 8443);
    }

    #[test]
    fn endpoints_may_have_a_query_but_no_fragment() {
        let u = endpoint_url("https://h/mcp?team=a", "remote URL", false).unwrap();
        assert_eq!(u, "https://h/mcp?team=a");
        assert!(endpoint_url("https://h/mcp#x", "remote URL", false).is_err());
        assert!(SourceUrl::parse("https://h/r?x", false).is_err());
        let err = endpoint_url("http://h/mcp", "remote URL", false).unwrap_err();
        assert_eq!(err.to_string(), "invalid remote URL: use https://");
    }

    #[test]
    fn errors_never_quote_the_value() {
        let err = GitRef::parse("--upload-pack=evil").unwrap_err().to_string();
        assert!(!err.contains("evil"), "{err}");
    }
}
