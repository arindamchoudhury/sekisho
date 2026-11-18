//! A mock of the six GitHub REST calls Git mode's proposals make, over the
//! bare repositories [`crate::git_http`] serves: `/repos/{owner}/{repo}`
//! is `<root>/<repo>.git` (the owner is ignored). It records every request
//! and refuses one without `X-GitHub-Api-Version: 2026-03-10`. Tests merge
//! a pull request with `POST /_github/{owner}/{repo}/pulls/{n}/merge`.
//! Needs `git` on the PATH.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use serde_json::{Value, json};

/// The API version Sekisho sends (D175).
pub const API_VERSION: &str = "2026-03-10";

/// One request the mock got.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub api_version: Option<String>,
    pub authorization: Option<String>,
    pub accept: Option<String>,
    pub body: Value,
}

/// One pull request opened on the mock.
#[derive(Clone, Debug)]
pub struct Pull {
    pub number: u64,
    pub repo: String,
    pub title: String,
    pub head: String,
    pub base: String,
    pub body: String,
}

/// What the mock saw and holds; clones share it.
#[derive(Clone, Default)]
pub struct GitHub {
    root: PathBuf,
    pub requests: Arc<Mutex<Vec<Recorded>>>,
    pub pulls: Arc<Mutex<Vec<Pull>>>,
}

impl GitHub {
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    pub fn pulls(&self) -> Vec<Pull> {
        self.pulls.lock().unwrap().clone()
    }
}

/// The mock over the bare repositories in `root`, and its record.
pub fn router(root: PathBuf) -> (Router, GitHub) {
    let state = GitHub {
        root,
        ..GitHub::default()
    };
    let router = Router::new()
        .route("/repos/{owner}/{repo}", get(repository))
        .route(
            "/repos/{owner}/{repo}/git/ref/heads/{*branch}",
            get(get_ref),
        )
        .route("/repos/{owner}/{repo}/git/refs", post(create_ref))
        .route(
            "/repos/{owner}/{repo}/contents/{*path}",
            get(get_contents).put(put_contents).delete(delete_contents),
        )
        .route("/repos/{owner}/{repo}/pulls", post(create_pull))
        .route("/_github/{owner}/{repo}/pulls/{number}/merge", post(merge))
        .layer(axum::middleware::from_fn_with_state(state.clone(), record))
        .with_state(state.clone());
    (router, state)
}

async fn record(
    State(gh): State<GitHub>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 16 << 20)
        .await
        .unwrap_or_default();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let api_version = header("x-github-api-version");
    gh.requests.lock().unwrap().push(Recorded {
        method: parts.method.to_string(),
        path: parts.uri.path().to_owned(),
        api_version: api_version.clone(),
        authorization: header("authorization"),
        accept: header("accept"),
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    });
    if !parts.uri.path().starts_with("/_github/") && api_version.as_deref() != Some(API_VERSION) {
        return error(
            StatusCode::BAD_REQUEST,
            "X-GitHub-Api-Version is missing or unknown",
        );
    }
    next.run(axum::extract::Request::from_parts(parts, bytes.into()))
        .await
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({ "message": message }))).into_response()
}

fn bare(gh: &GitHub, repo: &str) -> PathBuf {
    gh.root.join(format!("{repo}.git"))
}

/// Runs git on a bare repository: its trimmed output, or `None` on failure.
fn git(dir: &Path, args: &[&str], env: &[(&str, &str)], input: Option<&[u8]>) -> Option<String> {
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "GitHub mock")
        .env("GIT_AUTHOR_EMAIL", "mock@github.test")
        .env("GIT_COMMITTER_NAME", "GitHub mock")
        .env("GIT_COMMITTER_EMAIL", "mock@github.test")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (k, v) in env {
        command.env(k, v);
    }
    let mut child = command.spawn().ok()?;
    if let Some(input) = input {
        use std::io::Write;
        child.stdin.take()?.write_all(input).ok()?;
    }
    let out = child.wait_with_output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

async fn repository(
    State(gh): State<GitHub>,
    UrlPath((_, repo)): UrlPath<(String, String)>,
) -> Response {
    let dir = bare(&gh, &repo);
    match git(&dir, &["symbolic-ref", "--short", "HEAD"], &[], None) {
        Some(branch) => axum::Json(json!({ "default_branch": branch })).into_response(),
        None => error(StatusCode::NOT_FOUND, "Not Found"),
    }
}

async fn get_ref(
    State(gh): State<GitHub>,
    UrlPath((_, repo, branch)): UrlPath<(String, String, String)>,
) -> Response {
    let dir = bare(&gh, &repo);
    match git(
        &dir,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ],
        &[],
        None,
    ) {
        Some(sha) => axum::Json(json!({
            "ref": format!("refs/heads/{branch}"),
            "object": { "sha": sha, "type": "commit" }
        }))
        .into_response(),
        None => error(StatusCode::NOT_FOUND, "Not Found"),
    }
}

async fn create_ref(
    State(gh): State<GitHub>,
    UrlPath((_, repo)): UrlPath<(String, String)>,
    body: Bytes,
) -> Response {
    let dir = bare(&gh, &repo);
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let (Some(name), Some(sha)) = (body["ref"].as_str(), body["sha"].as_str()) else {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "ref and sha are required");
    };
    if git(&dir, &["rev-parse", "--verify", "-q", name], &[], None).is_some() {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "Reference already exists");
    }
    match git(&dir, &["update-ref", name, sha], &[], None) {
        Some(_) => (
            StatusCode::CREATED,
            axum::Json(json!({ "ref": name, "object": { "sha": sha } })),
        )
            .into_response(),
        None => error(StatusCode::UNPROCESSABLE_ENTITY, "Object does not exist"),
    }
}

/// The blob at `path` on `branch`, if any.
fn blob(dir: &Path, branch: &str, path: &str) -> Option<String> {
    git(
        dir,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}:{path}"),
        ],
        &[],
        None,
    )
}

async fn get_contents(
    State(gh): State<GitHub>,
    UrlPath((_, repo, path)): UrlPath<(String, String, String)>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let dir = bare(&gh, &repo);
    let branch = query
        .get("ref")
        .cloned()
        .or_else(|| git(&dir, &["symbolic-ref", "--short", "HEAD"], &[], None))
        .unwrap_or_default();
    match blob(&dir, &branch, &path) {
        Some(sha) => {
            axum::Json(json!({ "type": "file", "path": path, "sha": sha })).into_response()
        }
        None => error(StatusCode::NOT_FOUND, "Not Found"),
    }
}

/// Commits a change to `path` on `branch`: new bytes, or `None` to delete.
fn commit_change(
    dir: &Path,
    branch: &str,
    path: &str,
    bytes: Option<&[u8]>,
    message: &str,
) -> Option<String> {
    let parent = git(
        dir,
        &[
            "rev-parse",
            "--verify",
            "-q",
            &format!("refs/heads/{branch}"),
        ],
        &[],
        None,
    )?;
    let index = dir.join(format!("mock-index-{}", std::process::id()));
    let index_text = index.to_string_lossy().into_owned();
    let env = [("GIT_INDEX_FILE", index_text.as_str())];
    let _ = std::fs::remove_file(&index);
    git(dir, &["read-tree", &parent], &env, None)?;
    match bytes {
        Some(bytes) => {
            let blob = git(dir, &["hash-object", "-w", "--stdin"], &[], Some(bytes))?;
            git(
                dir,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("100644,{blob},{path}"),
                ],
                &env,
                None,
            )?;
        }
        None => {
            // `--force-remove` needs a work tree; mode 0 removes in a bare one.
            let line = format!(
                "0 {}	{path}
",
                "0".repeat(40)
            );
            git(
                dir,
                &["update-index", "--index-info"],
                &env,
                Some(line.as_bytes()),
            )?;
        }
    }
    let tree = git(dir, &["write-tree"], &env, None)?;
    let _ = std::fs::remove_file(&index);
    let commit = git(
        dir,
        &["commit-tree", &tree, "-p", &parent, "-m", message],
        &[],
        None,
    )?;
    git(
        dir,
        &[
            "update-ref",
            &format!("refs/heads/{branch}"),
            &commit,
            &parent,
        ],
        &[],
        None,
    )?;
    Some(commit)
}

async fn change_contents(
    gh: GitHub,
    repo: String,
    path: String,
    body: Bytes,
    delete: bool,
) -> Response {
    let dir = bare(&gh, &repo);
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let (Some(message), Some(branch)) = (body["message"].as_str(), body["branch"].as_str()) else {
        return error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "message and branch are required",
        );
    };
    let current = blob(&dir, branch, &path);
    let given = body["sha"].as_str();
    match (&current, given) {
        (Some(c), Some(g)) if c != g => {
            return error(StatusCode::CONFLICT, "sha does not match");
        }
        (Some(_), None) => {
            return error(StatusCode::UNPROCESSABLE_ENTITY, "\"sha\" wasn't supplied.");
        }
        (None, _) if delete => return error(StatusCode::NOT_FOUND, "Not Found"),
        _ => {}
    }
    let bytes = if delete {
        None
    } else {
        let Some(content) = body["content"].as_str() else {
            return error(StatusCode::UNPROCESSABLE_ENTITY, "content is required");
        };
        match base64::engine::general_purpose::STANDARD.decode(content) {
            Ok(b) => Some(b),
            Err(_) => return error(StatusCode::UNPROCESSABLE_ENTITY, "content isn't base64"),
        }
    };
    match commit_change(&dir, branch, &path, bytes.as_deref(), message) {
        Some(commit) => {
            let status = if delete || current.is_some() {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            (status, axum::Json(json!({ "commit": { "sha": commit } }))).into_response()
        }
        None => error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the change couldn't be committed",
        ),
    }
}

async fn put_contents(
    State(gh): State<GitHub>,
    UrlPath((_, repo, path)): UrlPath<(String, String, String)>,
    body: Bytes,
) -> Response {
    change_contents(gh, repo, path, body, false).await
}

async fn delete_contents(
    State(gh): State<GitHub>,
    UrlPath((_, repo, path)): UrlPath<(String, String, String)>,
    body: Bytes,
) -> Response {
    change_contents(gh, repo, path, body, true).await
}

async fn create_pull(
    State(gh): State<GitHub>,
    UrlPath((owner, repo)): UrlPath<(String, String)>,
    body: Bytes,
) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let text = |k: &str| body[k].as_str().unwrap_or_default().to_owned();
    let (head, base) = (text("head"), text("base"));
    let dir = bare(&gh, &repo);
    for branch in [&head, &base] {
        if git(
            &dir,
            &[
                "rev-parse",
                "--verify",
                "-q",
                &format!("refs/heads/{branch}"),
            ],
            &[],
            None,
        )
        .is_none()
        {
            return error(StatusCode::UNPROCESSABLE_ENTITY, "Validation Failed");
        }
    }
    let mut pulls = gh.pulls.lock().unwrap();
    let number = pulls.len() as u64 + 1;
    pulls.push(Pull {
        number,
        repo: repo.clone(),
        title: text("title"),
        head,
        base,
        body: text("body"),
    });
    (
        StatusCode::CREATED,
        axum::Json(json!({
            "number": number,
            "html_url": format!("https://github.test/{owner}/{repo}/pull/{number}"),
        })),
    )
        .into_response()
}

async fn merge(
    State(gh): State<GitHub>,
    UrlPath((_, repo, number)): UrlPath<(String, String, u64)>,
    _method: Method,
    _uri: Uri,
    _headers: HeaderMap,
) -> Response {
    let Some(pull) = gh
        .pulls()
        .into_iter()
        .find(|p| p.repo == repo && p.number == number)
    else {
        return error(StatusCode::NOT_FOUND, "Not Found");
    };
    let dir = bare(&gh, &repo);
    let rev = |b: &str| git(&dir, &["rev-parse", &format!("refs/heads/{b}")], &[], None);
    let (Some(base), Some(head)) = (rev(&pull.base), rev(&pull.head)) else {
        return error(StatusCode::NOT_FOUND, "Not Found");
    };
    let fast_forward = git(
        &dir,
        &["merge-base", "--is-ancestor", &base, &head],
        &[],
        None,
    )
    .is_some();
    let new = if fast_forward {
        head
    } else {
        let Some(commit) = git(
            &dir,
            &[
                "commit-tree",
                &format!("{head}^{{tree}}"),
                "-p",
                &base,
                "-p",
                &head,
                "-m",
                &format!("Merge pull request #{number}"),
            ],
            &[],
            None,
        ) else {
            return error(StatusCode::CONFLICT, "can't merge");
        };
        commit
    };
    match git(
        &dir,
        &["update-ref", &format!("refs/heads/{}", pull.base), &new],
        &[],
        None,
    ) {
        Some(_) => axum::Json(json!({ "merged": true, "sha": new })).into_response(),
        None => error(StatusCode::CONFLICT, "can't merge"),
    }
}
