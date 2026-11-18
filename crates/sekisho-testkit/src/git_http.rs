//! Git's smart HTTP for tests: `git http-backend` run as a CGI under
//! `/git/`, serving bare repositories in a folder, and [`Repo`] to commit
//! to them. Needs `git` on the PATH.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Response, StatusCode};
use axum::routing::any;
use tokio::io::AsyncWriteExt;

/// Serves the bare repositories under `root` at `/git/<name>.git`.
pub fn serve(root: PathBuf) -> Router {
    Router::new()
        .route("/git/{*path}", any(backend))
        .with_state(root)
}

async fn backend(State(root): State<PathBuf>, request: Request) -> Response<Body> {
    let (parts, body) = request.into_parts();
    let body: Bytes = axum::body::to_bytes(body, 64 << 20)
        .await
        .unwrap_or_default();
    let path_info = parts
        .uri
        .path()
        .strip_prefix("/git")
        .unwrap_or("/")
        .to_owned();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned()
    };
    let mut child = tokio::process::Command::new("git")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", &root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", path_info)
        .env("QUERY_STRING", parts.uri.query().unwrap_or(""))
        .env("REQUEST_METHOD", parts.method.as_str())
        .env("CONTENT_TYPE", header("content-type"))
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("HTTP_GIT_PROTOCOL", header("git-protocol"))
        .env("HTTP_CONTENT_ENCODING", header("content-encoding"))
        .env("REMOTE_ADDR", "127.0.0.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("run git http-backend");
    let mut stdin = child.stdin.take().expect("stdin");
    let written = tokio::spawn(async move {
        let _ = stdin.write_all(&body).await;
    });
    let output = child
        .wait_with_output()
        .await
        .expect("git http-backend output");
    let _ = written.await;
    cgi_response(&output.stdout)
}

/// Turns a CGI answer (headers, a blank line, the body) into a response,
/// its `Status:` header into the status.
fn cgi_response(out: &[u8]) -> Response<Body> {
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| (i, 4))
        .or_else(|| out.windows(2).position(|w| w == b"\n\n").map(|i| (i, 2)));
    let Some((end, gap)) = split else {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("no CGI headers"))
            .unwrap();
    };
    let head = String::from_utf8_lossy(&out[..end]);
    let mut status = StatusCode::OK;
    let mut headers = HeaderMap::new();
    for line in head.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("status") {
            let code = value.split_whitespace().next().unwrap_or("500");
            status = code
                .parse()
                .ok()
                .and_then(|c| StatusCode::from_u16(c).ok())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        } else if let (Ok(n), Ok(v)) = (
            HeaderName::try_from(name.trim()),
            HeaderValue::from_str(value),
        ) {
            headers.append(n, v);
        }
    }
    let mut response = Response::new(Body::from(out[end + gap..].to_vec()));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// A bare repository served by [`serve`], and a work tree that commits to
/// its `main`.
pub struct Repo {
    bare: PathBuf,
    work: PathBuf,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

impl Repo {
    /// Makes `<root>/<name>.git`, whose default branch is `main`.
    pub fn create(root: &Path, name: &str) -> Repo {
        let bare = root.join(format!("{name}.git"));
        let work = root.join(format!("{name}.work"));
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        git(&bare, &["init", "-q", "--bare", "-b", "main"]);
        git(&work, &["init", "-q", "-b", "main"]);
        git(&work, &["config", "user.email", "tests@sekisho.test"]);
        git(&work, &["config", "user.name", "Sekisho tests"]);
        git(&work, &["config", "core.autocrlf", "false"]);
        Repo { bare, work }
    }

    /// Writes `files` (other files stay), commits them on `main`, pushes,
    /// and answers the commit.
    pub fn commit(&self, files: &[(&str, &[u8])], message: &str) -> String {
        // Once something is committed, take what the bare one has.
        let committed = Command::new("git")
            .arg("-C")
            .arg(&self.work)
            .args(["rev-parse", "--verify", "-q", "HEAD"])
            .output()
            .is_ok_and(|o| o.status.success());
        if committed {
            self.sync_from_bare();
        }
        for (path, bytes) in files {
            let path = self.work.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        git(&self.work, &["add", "-A"]);
        git(
            &self.work,
            &["commit", "-q", "--allow-empty", "-m", message],
        );
        let bare = self.bare.to_string_lossy().into_owned();
        git(
            &self.work,
            &["push", "-q", "-f", &bare, "HEAD:refs/heads/main"],
        );
        git(&self.work, &["rev-parse", "HEAD"])
    }

    /// Removes `paths`, commits on `main` and pushes: answers the commit.
    /// The work tree first takes `main` as the bare repository has it, so
    /// commits made there (a merged pull request) stay.
    pub fn remove(&self, paths: &[&str], message: &str) -> String {
        self.sync_from_bare();
        for path in paths {
            git(&self.work, &["rm", "-q", path]);
        }
        git(&self.work, &["commit", "-q", "-m", message]);
        let bare = self.bare.to_string_lossy().into_owned();
        git(
            &self.work,
            &["push", "-q", "-f", &bare, "HEAD:refs/heads/main"],
        );
        git(&self.work, &["rev-parse", "HEAD"])
    }

    /// Resets the work tree to the bare repository's `main`.
    fn sync_from_bare(&self) {
        let bare = self.bare.to_string_lossy().into_owned();
        git(&self.work, &["fetch", "-q", &bare, "main"]);
        git(&self.work, &["reset", "-q", "--hard", "FETCH_HEAD"]);
    }

    /// Makes branch `name` at the latest commit.
    pub fn branch(&self, name: &str) {
        let bare = self.bare.to_string_lossy().into_owned();
        git(
            &self.work,
            &[
                "push",
                "-q",
                "-f",
                &bare,
                &format!("HEAD:refs/heads/{name}"),
            ],
        );
    }
}
