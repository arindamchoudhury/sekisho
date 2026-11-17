//! A container image's tag resolved to its digest at sync (D171), as
//! `docker pull` asks: `HEAD /v2/<repo>/manifests/<tag>` for any manifest
//! type, then the registry's token flow when it answers 401 (Docker's
//! `WWW-Authenticate: Bearer realm=…,service=…,scope=…`), with the
//! registry's credential when Sekisho holds one.

use base64::Engine;
use http::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, WWW_AUTHENTICATE};
use sekisho_catalog::pins::ImageRef;
use url::Url;

use super::outbound::Outbound;

/// Every manifest type a tag may name: OCI's index and manifest, Docker's
/// list and v2 manifest.
const MANIFESTS: &str = "application/vnd.oci.image.index.v1+json, \
     application/vnd.oci.image.manifest.v1+json, \
     application/vnd.docker.distribution.manifest.list.v2+json, \
     application/vnd.docker.distribution.manifest.v2+json";
/// A token answer is small.
const TOKEN_LIMIT: usize = 64 << 10;

/// A `WWW-Authenticate` challenge: its scheme, lowercased, and parameters.
#[derive(Debug, PartialEq, Eq)]
struct Challenge {
    scheme: String,
    params: Vec<(String, String)>,
}

impl Challenge {
    fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Reads `Scheme k="v",k2=v2`; quoted values may hold commas.
fn parse_challenge(header: &str) -> Option<Challenge> {
    let header = header.trim();
    let (scheme, rest) = header.split_once(' ').unwrap_or((header, ""));
    if scheme.is_empty() {
        return None;
    }
    let mut params = Vec::new();
    let mut chars = rest.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| *c == ',' || c.is_whitespace()) {
            chars.next();
        }
        let key: String = chars.by_ref().take_while(|c| *c != '=').collect::<String>();
        let key = key.trim().to_owned();
        if key.is_empty() {
            break;
        }
        let mut value = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => value.extend(chars.next()),
                    '"' => break,
                    c => value.push(c),
                }
            }
        } else {
            while let Some(c) = chars.peek() {
                if *c == ',' {
                    break;
                }
                value.push(*c);
                chars.next();
            }
            value = value.trim().to_owned();
        }
        params.push((key, value));
    }
    Some(Challenge {
        scheme: scheme.to_lowercase(),
        params,
    })
}

fn basic(user: &str, password: &str) -> String {
    let engine = base64::engine::general_purpose::STANDARD;
    format!("Basic {}", engine.encode(format!("{user}:{password}")))
}

/// The `Authorization` value the challenge asks for.
async fn authorize(
    out: &Outbound,
    challenge: &Challenge,
    creds: Option<&(String, String)>,
    allow_private: bool,
) -> Result<String, String> {
    match challenge.scheme.as_str() {
        "basic" => {
            let (user, password) =
                creds.ok_or("the registry wants a credential, and Sekisho holds none for it")?;
            Ok(basic(user, password))
        }
        "bearer" => {
            let realm = challenge
                .param("realm")
                .ok_or("the registry's challenge names no realm")?;
            let mut url =
                Url::parse(realm).map_err(|_| "the registry's token realm isn't a URL")?;
            {
                let mut query = url.query_pairs_mut();
                for name in ["service", "scope"] {
                    if let Some(value) = challenge.param(name) {
                        query.append_pair(name, value);
                    }
                }
            }
            let mut headers = HeaderMap::new();
            if let Some((user, password)) = creds {
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&basic(user, password))
                        .map_err(|_| "the credential can't be sent")?,
                );
            }
            let answer = out
                .get(&url, allow_private, headers, TOKEN_LIMIT)
                .await
                .map_err(|e| format!("the token service: {e}"))?;
            if answer.status != 200 {
                return Err(format!("the token service answered HTTP {}", answer.status));
            }
            let json: serde_json::Value = serde_json::from_slice(&answer.body)
                .map_err(|_| "the token service didn't answer JSON")?;
            let token = json
                .get("token")
                .or_else(|| json.get("access_token"))
                .and_then(serde_json::Value::as_str)
                .filter(|t| !t.is_empty())
                .ok_or("the token service answered no token")?;
            Ok(format!("Bearer {token}"))
        }
        other => Err(format!(
            "the registry wants {other} authentication, which Sekisho doesn't speak"
        )),
    }
}

fn is_digest(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

/// The digest `image`'s tag names now (`latest` when it has none); an
/// image that names a digest already keeps it.
pub async fn resolve_digest(
    out: &Outbound,
    image: &ImageRef,
    creds: Option<(String, String)>,
    allow_private: bool,
) -> Result<String, String> {
    if let Some(digest) = image.digest() {
        return Ok(digest.to_owned());
    }
    let tag = image.tag().unwrap_or("latest");
    // Registries are https; builds that allow plain http (tests, e2e)
    // reach them over http.
    let scheme = if out.allow_plain_http {
        "http"
    } else {
        "https"
    };
    let url = Url::parse(&format!(
        "{scheme}://{}/v2/{}/manifests/{tag}",
        image.registry(),
        image.repository()
    ))
    .map_err(|_| "the image's registry isn't a host Sekisho can reach")?;
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static(MANIFESTS));
    let mut answer = out.head(&url, allow_private, headers.clone()).await?;
    if answer.status == 401 {
        let challenge = answer
            .headers
            .get(WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_challenge)
            .ok_or("the registry answered 401 without a challenge")?;
        let value = authorize(out, &challenge, creds.as_ref(), allow_private).await?;
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&value).map_err(|_| "the registry's token can't be sent")?,
        );
        answer = out.head(&url, allow_private, headers).await?;
    }
    match answer.status {
        200 => {
            let digest = answer
                .headers
                .get("docker-content-digest")
                .and_then(|v| v.to_str().ok())
                .ok_or("the registry answered no Docker-Content-Digest")?;
            if is_digest(digest) {
                Ok(digest.to_owned())
            } else {
                Err("the registry answered a digest that isn't sha256".to_owned())
            }
        }
        404 => Err("the registry has no such tag".to_owned()),
        401 | 403 => Err(format!(
            "the registry refused access (HTTP {})",
            answer.status
        )),
        status => Err(format!("the registry answered HTTP {status}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenges_are_read_as_registries_write_them() {
        let c = parse_challenge(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/nginx:pull,push""#,
        )
        .unwrap();
        assert_eq!(c.scheme, "bearer");
        assert_eq!(c.param("realm"), Some("https://auth.docker.io/token"));
        assert_eq!(c.param("Service"), Some("registry.docker.io"));
        assert_eq!(c.param("scope"), Some("repository:library/nginx:pull,push"));
        let b = parse_challenge(r#"Basic realm="Registry Realm""#).unwrap();
        assert_eq!(b.scheme, "basic");
        assert_eq!(b.param("realm"), Some("Registry Realm"));
        let bare = parse_challenge("Bearer realm=https://h/t, service=s").unwrap();
        assert_eq!(bare.param("realm"), Some("https://h/t"));
        assert_eq!(bare.param("service"), Some("s"));
        assert!(parse_challenge("").is_none());
    }

    #[test]
    fn digests_are_sha256() {
        assert!(is_digest(&format!("sha256:{}", "a".repeat(64))));
        assert!(!is_digest(&format!("sha512:{}", "a".repeat(64))));
        assert!(!is_digest(&format!("sha256:{}", "A".repeat(64))));
    }
}
