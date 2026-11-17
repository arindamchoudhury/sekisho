//! Exact versions (D171): an npm or Python package names one version, and a
//! container image is pinned to a digest at sync.

use std::fmt;

/// Why a package or image was refused. The message is shown to admins.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid {what} {value:?}: {reason}")]
pub struct PinError {
    what: &'static str,
    value: String,
    reason: &'static str,
}

fn refuse<T>(what: &'static str, value: &str, reason: &'static str) -> Result<T, PinError> {
    Err(PinError {
        what,
        value: value.to_owned(),
        reason,
    })
}

/// Package specs that name something other than a registry package.
fn is_url_git_or_path(s: &str) -> bool {
    s.contains("://")
        || s.contains(char::is_whitespace)
        || [
            "git+",
            "git:",
            "file:",
            "github:",
            "gitlab:",
            "bitbucket:",
            "link:",
            "npm:",
            ".",
            "/",
            "~",
            "\\",
        ]
        .iter()
        .any(|p| s.starts_with(p))
}

// ---------------------------------------------------------------- npm

const NPM: &str = "npm package";

/// An npm package at one exact version: `name@1.2.3` or
/// `@scope/name@1.2.3`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NpmPackage {
    name: String,
    version: semver::Version,
}

/// npm's rule for new names: lowercase, URL-safe, not starting with `.`
/// or `_`, at most 214 characters with the scope.
fn is_npm_part(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(['.', '_'])
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"-._~".contains(&c))
}

fn is_npm_name(s: &str) -> bool {
    if s.len() > 214 {
        return false;
    }
    match s.strip_prefix('@') {
        Some(scoped) => scoped
            .split_once('/')
            .is_some_and(|(scope, name)| is_npm_part(scope) && is_npm_part(name)),
        None => is_npm_part(s),
    }
}

impl NpmPackage {
    pub fn parse_exact(s: &str) -> Result<Self, PinError> {
        if is_url_git_or_path(s) {
            return refuse(
                NPM,
                s,
                "name a package from the registry, not a URL, Git or a path",
            );
        }
        // The version follows the last `@`; a scope's `@` is the first character.
        let Some((name, version)) = s.rsplit_once('@').filter(|(name, _)| !name.is_empty()) else {
            return refuse(NPM, s, "name one exact version, as name@1.2.3");
        };
        if !is_npm_name(name) {
            return refuse(NPM, s, "the name isn't a valid npm package name");
        }
        let Ok(version) = semver::Version::parse(version) else {
            return refuse(
                NPM,
                s,
                "name one exact version, as name@1.2.3, not a range or tag",
            );
        };
        if !version.build.is_empty() {
            // npm ignores build metadata, so it would name a version inexactly.
            return refuse(NPM, s, "build metadata (+…) isn't part of an npm version");
        }
        Ok(Self {
            name: name.to_owned(),
            version,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn version(&self) -> &semver::Version {
        &self.version
    }
}

impl fmt::Display for NpmPackage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.name, self.version)
    }
}

// ---------------------------------------------------------------- Python

const PYTHON: &str = "Python package";

/// A Python package at one exact version: `name==1.0` (also written
/// `name@1.0`, as `uvx` accepts).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PythonPackage {
    name: String,
    version: String,
}

/// PEP 508's name: `^([A-Z0-9]|[A-Z0-9][A-Z0-9._-]*[A-Z0-9])$`, any case.
fn is_python_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(c))
}

/// `^[0-9][0-9A-Za-z.!+_-]*$`: one version, never a wildcard.
fn is_python_version(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_digit()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b".!+_-".contains(c))
}

impl PythonPackage {
    pub fn parse_exact(s: &str) -> Result<Self, PinError> {
        if is_url_git_or_path(s) {
            return refuse(
                PYTHON,
                s,
                "name a package from the index, not a URL, Git or a path",
            );
        }
        let Some((name, version)) = s.split_once("==").or_else(|| s.split_once('@')) else {
            return refuse(PYTHON, s, "name one exact version, as name==1.0");
        };
        if !is_python_name(name) {
            return refuse(
                PYTHON,
                s,
                "the name isn't a valid Python package name (extras aren't allowed)",
            );
        }
        if !is_python_version(version) {
            return refuse(
                PYTHON,
                s,
                "name one exact version, as name==1.0, not a range, wildcard or URL",
            );
        }
        Ok(Self {
            name: name.to_owned(),
            version: version.to_owned(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn version(&self) -> &str {
        &self.version
    }
}

impl fmt::Display for PythonPackage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}=={}", self.name, self.version)
    }
}

// ---------------------------------------------------------------- images

const IMAGE: &str = "container image";
const DOCKER_HUB: &str = "registry-1.docker.io";
const MAX_NAME: usize = 255;
const MAX_TAG: usize = 128;

/// A container image reference in Docker's grammar
/// (`distribution/reference`), normalised: Docker Hub images name
/// `registry-1.docker.io`, and one-segment ones `library/`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ImageRef {
    registry: String,
    repository: String,
    tag: Option<String>,
    digest: Option<String>,
}

/// `sha256:` and 64 lowercase hex digits.
fn is_digest(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

/// `[\w][\w.-]{0,127}`
fn is_tag(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=MAX_TAG).contains(&b.len())
        && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(c))
}

/// A path component: `[a-z0-9]+((\.|_|__|-+)[a-z0-9]+)*`.
fn is_path_component(s: &str) -> bool {
    let b = s.as_bytes();
    let alnum = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    if b.is_empty() || !alnum(b[0]) || !alnum(b[b.len() - 1]) {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        if alnum(b[i]) {
            i += 1;
            continue;
        }
        // A separator run: `.`, `_`, `__` or one or more `-`.
        let start = i;
        while i < b.len() && !alnum(b[i]) {
            i += 1;
        }
        let sep = &b[start..i];
        let ok = sep == b"." || sep == b"_" || sep == b"__" || sep.iter().all(|c| *c == b'-');
        if !ok {
            return false;
        }
    }
    true
}

/// A registry host with an optional port: DNS labels of letters, digits
/// and hyphens, dot-separated.
fn is_registry(s: &str) -> bool {
    let (host, port) = match s.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (s, None),
    };
    let label_ok = |l: &str| {
        let b = l.as_bytes();
        !b.is_empty()
            && b[0].is_ascii_alphanumeric()
            && b[b.len() - 1].is_ascii_alphanumeric()
            && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-')
    };
    host.split('.').all(label_ok)
        && port
            .is_none_or(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|c| c.is_ascii_digit()))
}

impl ImageRef {
    pub fn parse(s: &str) -> Result<Self, PinError> {
        let (rest, digest) = match s.split_once('@') {
            Some((rest, digest)) => {
                if !is_digest(digest) {
                    return refuse(IMAGE, s, "a digest is sha256: and 64 lowercase hex digits");
                }
                (rest, Some(digest.to_owned()))
            }
            None => (s, None),
        };
        // A tag follows the last `:` after the last `/`.
        let last_slash = rest.rfind('/').map_or(0, |i| i + 1);
        let (name, tag) = match rest[last_slash..].rfind(':') {
            Some(i) => {
                let (name, tag) = rest.split_at(last_slash + i);
                let tag = &tag[1..];
                if !is_tag(tag) {
                    return refuse(
                        IMAGE,
                        s,
                        "a tag has 1-128 letters, digits, _ . or -, not starting with . or -",
                    );
                }
                (name, Some(tag.to_owned()))
            }
            None => (rest, None),
        };
        if name.len() > MAX_NAME {
            return refuse(IMAGE, s, "the name is longer than 255 characters");
        }
        let (registry, path) = match name.split_once('/') {
            Some((first, path)) if first.contains(['.', ':']) || first == "localhost" => {
                (first, path)
            }
            _ => ("docker.io", name),
        };
        if !is_registry(registry) {
            return refuse(IMAGE, s, "the registry isn't a valid host name");
        }
        if !path.split('/').all(is_path_component) {
            return refuse(
                IMAGE,
                s,
                "the repository has lowercase letters, digits and . _ - separators",
            );
        }
        let hub = matches!(registry, "docker.io" | "index.docker.io" | DOCKER_HUB);
        let (registry, repository) = if hub {
            let repository = if path.contains('/') {
                path.to_owned()
            } else {
                format!("library/{path}")
            };
            (DOCKER_HUB.to_owned(), repository)
        } else {
            (registry.to_owned(), path.to_owned())
        };
        Ok(Self {
            registry,
            repository,
            tag,
            digest,
        })
    }

    pub fn registry(&self) -> &str {
        &self.registry
    }

    pub fn repository(&self) -> &str {
        &self.repository
    }

    pub fn tag(&self) -> Option<&str> {
        self.tag.as_deref()
    }

    pub fn digest(&self) -> Option<&str> {
        self.digest.as_deref()
    }

    /// The same image pinned to `digest` (`sha256:<64 hex>`), as a sync
    /// resolves a tag.
    pub fn with_digest(&self, digest: String) -> Result<ImageRef, PinError> {
        if !is_digest(&digest) {
            return refuse(
                IMAGE,
                &digest,
                "a digest is sha256: and 64 lowercase hex digits",
            );
        }
        Ok(Self {
            digest: Some(digest),
            ..self.clone()
        })
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.registry, self.repository)?;
        if let Some(tag) = &self.tag {
            write!(f, ":{tag}")?;
        }
        if let Some(digest) = &self.digest {
            write!(f, "@{digest}")?;
        }
        Ok(())
    }
}

/// Each is stored as its canonical text and checked again when read.
macro_rules! serde_as_text {
    ($ty:ident, $parse:ident) => {
        impl serde::Serialize for $ty {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }

        impl<'de> serde::Deserialize<'de> for $ty {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = <String as serde::Deserialize>::deserialize(d)?;
                Self::$parse(&s).map_err(serde::de::Error::custom)
            }
        }
    };
}

serde_as_text!(NpmPackage, parse_exact);
serde_as_text!(PythonPackage, parse_exact);
serde_as_text!(ImageRef, parse);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_components_take_dockers_separators() {
        for good in ["a", "a.b", "a_b", "a__b", "a-b", "a---b", "a1.b2_c3"] {
            assert!(is_path_component(good), "{good}");
        }
        for bad in ["", "a___b", "a._b", "a-.b", ".a", "a_", "A", "a..b"] {
            assert!(!is_path_component(bad), "{bad}");
        }
    }

    #[test]
    fn registries_are_hosts_with_an_optional_port() {
        for good in ["ghcr.io", "localhost:5000", "my-registry.example:443", "r1"] {
            assert!(is_registry(good), "{good}");
        }
        for bad in [
            "",
            "-a.io",
            "a-.io",
            "a..io",
            "a.io:",
            "a.io:123456",
            "a.io:x",
            "a_b.io",
        ] {
            assert!(!is_registry(bad), "{bad}");
        }
    }

    #[test]
    fn pins_are_stored_as_their_canonical_text() {
        let image = ImageRef::parse("nginx:1.27").unwrap();
        let json = serde_json::to_string(&image).unwrap();
        assert_eq!(json, "\"registry-1.docker.io/library/nginx:1.27\"");
        assert_eq!(serde_json::from_str::<ImageRef>(&json).unwrap(), image);
        let py = PythonPackage::parse_exact("a@1.0").unwrap();
        assert_eq!(serde_json::to_string(&py).unwrap(), "\"a==1.0\"");
        assert!(serde_json::from_str::<NpmPackage>("\"pkg@latest\"").is_err());
    }

    #[test]
    fn errors_name_the_value_and_rule() {
        let err = NpmPackage::parse_exact("pkg@latest")
            .unwrap_err()
            .to_string();
        assert!(err.contains("pkg@latest") && err.contains("exact"), "{err}");
    }
}
