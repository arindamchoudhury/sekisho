//! Git mode's pull requests through GitHub's REST API (D175): version
//! `2026-03-10`, on github.com or GitHub Enterprise Server. Each call goes
//! through [`Outbound`], so GitHub's addresses are checked and pinned like
//! any outside fetch. The token is sent to GitHub alone, and no error
//! carries it or a URL.

use base64::Engine;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use http::{HeaderName, Method};
use sekisho_catalog::source::SourceUrl;
use serde_json::{Value, json};
use url::Url;

use super::outbound::Outbound;

/// The REST API version Sekisho speaks.
pub const API_VERSION: &str = "2026-03-10";
/// GitHub's answers are small; this is generous.
const ANSWER_LIMIT: usize = 1 << 20;

/// A repository on GitHub: `owner/name`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoPath {
    pub owner: String,
    pub name: String,
}

impl RepoPath {
    /// The last two segments of the repository's URL, `.git` removed.
    pub fn from_url(url: &SourceUrl) -> Option<Self> {
        let segments: Vec<&str> = url
            .as_url()
            .path_segments()?
            .filter(|s| !s.is_empty())
            .collect();
        let [.., owner, name] = segments.as_slice() else {
            return None;
        };
        let name = name.strip_suffix(".git").unwrap_or(name);
        (!owner.is_empty() && !name.is_empty()).then(|| Self {
            owner: (*owner).to_owned(),
            name: name.to_owned(),
        })
    }
}

/// A pull request GitHub opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
}

/// A change to one file, proposed on a branch of its own.
pub struct Proposal<'a> {
    /// The environment, which names the branch.
    pub environment: &'a str,
    /// Relative to the repository's root.
    pub path: &'a str,
    /// The file's new text; `None` deletes it.
    pub content: Option<String>,
    pub title: &'a str,
    pub body: &'a str,
}

pub struct GitHub {
    /// `https://api.github.com`, or GitHub Enterprise Server's `/api/v3`.
    pub api: Url,
    pub token: String,
    pub out: Outbound,
    /// GitHub Enterprise Server on a private network.
    pub allow_private: bool,
}

impl GitHub {
    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        what: &str,
    ) -> Result<(u16, Value), String> {
        let url = Url::parse(&format!(
            "{}/{path}",
            self.api.as_str().trim_end_matches('/')
        ))
        .map_err(|_| format!("{what}: the API URL can't be joined"))?;
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            HeaderName::from_static("x-github-api-version"),
            HeaderValue::from_static(API_VERSION),
        );
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", self.token))
            .map_err(|_| "the GitHub token can't be sent in a header".to_owned())?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);
        let bytes = match body {
            Some(b) => {
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                Some(serde_json::to_vec(&b).expect("JSON serialises"))
            }
            None => None,
        };
        let answer = self
            .out
            .send(
                method,
                &url,
                self.allow_private,
                headers,
                bytes,
                ANSWER_LIMIT,
            )
            .await
            .map_err(|e| format!("{what}: {e}"))?;
        let json: Value = serde_json::from_slice(&answer.body).unwrap_or(Value::Null);
        Ok((answer.status, json))
    }

    /// Fails unless GitHub answered 2xx, quoting GitHub's message.
    fn ok(what: &str, (status, json): (u16, Value)) -> Result<Value, String> {
        if (200..300).contains(&status) {
            return Ok(json);
        }
        let message: String = json["message"]
            .as_str()
            .unwrap_or("no message")
            .chars()
            .take(200)
            .collect();
        Err(format!("{what}: GitHub answered {status}: {message}"))
    }

    /// Opens a pull request that makes `proposal`'s change on a new branch
    /// `sekisho/<environment>-<8 hex>` off `base` (the repository's default
    /// branch when `None`).
    pub async fn propose(
        &self,
        repo: &RepoPath,
        base: Option<&str>,
        proposal: Proposal<'_>,
    ) -> Result<PullRequest, String> {
        let r = format!("repos/{}/{}", repo.owner, repo.name);
        let base = match base {
            Some(b) => b.to_owned(),
            None => {
                let info = Self::ok(
                    "reading the repository",
                    self.call(Method::GET, &r, None, "reading the repository")
                        .await?,
                )?;
                info["default_branch"]
                    .as_str()
                    .ok_or("the repository has no default branch")?
                    .to_owned()
            }
        };
        let what = "reading the base branch";
        let head = Self::ok(
            what,
            self.call(
                Method::GET,
                &format!("{r}/git/ref/heads/{base}"),
                None,
                what,
            )
            .await?,
        )?;
        let base_sha = head["object"]["sha"]
            .as_str()
            .ok_or("the base branch has no commit")?
            .to_owned();
        // The file as the base has it: its blob, or none when it's new.
        let what = "reading the file";
        let (status, json) = self
            .call(
                Method::GET,
                &format!("{r}/contents/{}?ref={base}", proposal.path),
                None,
                what,
            )
            .await?;
        let sha = match status {
            404 => None,
            _ => Some(
                Self::ok(what, (status, json))?["sha"]
                    .as_str()
                    .ok_or("the file has no sha")?
                    .to_owned(),
            ),
        };
        if proposal.content.is_none() && sha.is_none() {
            return Err(format!("{} isn't in the repository", proposal.path));
        }

        let suffix: String = sekisho_core::keys::random_bytes::<4>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let branch = format!("sekisho/{}-{suffix}", proposal.environment);
        let what = "making the branch";
        Self::ok(
            what,
            self.call(
                Method::POST,
                &format!("{r}/git/refs"),
                Some(json!({ "ref": format!("refs/heads/{branch}"), "sha": base_sha })),
                what,
            )
            .await?,
        )?;

        let what = "committing the change";
        let path = format!("{r}/contents/{}", proposal.path);
        let answer = match &proposal.content {
            Some(text) => {
                let mut body = json!({
                    "message": proposal.title,
                    "content": base64::engine::general_purpose::STANDARD.encode(text),
                    "branch": branch,
                });
                if let Some(sha) = &sha {
                    body["sha"] = json!(sha);
                }
                self.call(Method::PUT, &path, Some(body), what).await?
            }
            None => {
                let body = json!({ "message": proposal.title, "sha": sha, "branch": branch });
                self.call(Method::DELETE, &path, Some(body), what).await?
            }
        };
        Self::ok(what, answer)?;

        let what = "opening the pull request";
        let pull = Self::ok(
            what,
            self.call(
                Method::POST,
                &format!("{r}/pulls"),
                Some(json!({
                    "title": proposal.title,
                    "head": branch,
                    "base": base,
                    "body": proposal.body,
                })),
                what,
            )
            .await?,
        )?;
        Ok(PullRequest {
            number: pull["number"]
                .as_u64()
                .ok_or("the pull request has no number")?,
            url: pull["html_url"].as_str().unwrap_or_default().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repositories_are_the_urls_last_two_segments() {
        let url = |s: &str| SourceUrl::parse(s, true).unwrap();
        assert_eq!(
            RepoPath::from_url(&url("https://github.com/acme/envs.git")),
            Some(RepoPath {
                owner: "acme".into(),
                name: "envs".into()
            })
        );
        assert_eq!(
            RepoPath::from_url(&url("https://ghe.example.com/team/sub/envs")).map(|r| r.owner),
            Some("sub".into())
        );
        assert_eq!(
            RepoPath::from_url(&url("https://github.com/envs.git")),
            None
        );
    }
}
