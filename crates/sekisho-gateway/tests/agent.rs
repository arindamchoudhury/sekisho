use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use sekisho_gateway::agent::{AgentConfig, Url, run_agent, write_atomic};
use serde_json::json;
use tokio::time::{Instant, sleep};

const TOKEN: &str = "gateway-token-0123456789";
const V1: &str = "binds: [] # v1\n";
const V2: &str = "binds: [] # v2\n";

/// One scripted answer of the fake config server.
enum Scripted {
    Config(i64, &'static str),
    Status(u16),
}

#[derive(Clone, Default)]
struct Fake {
    script: Arc<Mutex<VecDeque<Scripted>>>,
    /// The `after` of every authorised request, in order.
    seen: Arc<Mutex<Vec<i64>>>,
    /// Requests refused for a wrong token.
    refused: Arc<Mutex<usize>>,
}

async fn config(
    State(fake): State<Fake>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    if headers.get("authorization").and_then(|v| v.to_str().ok())
        != Some(&format!("Bearer {TOKEN}"))
    {
        *fake.refused.lock().unwrap() += 1;
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let after = query
        .as_deref()
        .and_then(|q| q.strip_prefix("after="))
        .and_then(|v| v.parse().ok())
        .expect("after=<version>");
    fake.seen.lock().unwrap().push(after);
    let next = fake.script.lock().unwrap().pop_front();
    match next {
        Some(Scripted::Config(version, yaml)) => {
            axum::Json(json!({ "version": version, "yaml": yaml })).into_response()
        }
        Some(Scripted::Status(status)) => StatusCode::from_u16(status).unwrap().into_response(),
        None => {
            sleep(Duration::from_millis(100)).await; // a short long poll
            StatusCode::NO_CONTENT.into_response()
        }
    }
}

async fn fake_server(script: Vec<Scripted>) -> (Fake, Url) {
    let fake = Fake {
        script: Arc::new(Mutex::new(script.into())),
        ..Default::default()
    };
    let app = Router::new()
        .route("/internal/gateway/config", axum::routing::get(config))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (fake, Url::parse(&format!("http://{addr}/")).unwrap())
}

fn agent(server_url: Url, config_path: &Path, token: &str) -> AgentConfig {
    AgentConfig {
        server_url,
        gateway_token: token.into(),
        config_path: config_path.to_path_buf(),
        retry: Duration::from_millis(50),
    }
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

async fn eventually(within: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < within,
            "{what} not seen within {within:?}"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

fn config_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("config.yaml")
}

#[tokio::test]
async fn writes_new_versions() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_path(&dir);
    let (fake, url) = fake_server(vec![Scripted::Config(1, V1), Scripted::Config(2, V2)]).await;
    tokio::spawn(run_agent(agent(url, &path, TOKEN)));
    eventually(Duration::from_secs(2), "v2 on disk", || {
        read(&path).as_deref() == Some(V2)
    })
    .await;
    // The poll after v2 may still be on its way.
    eventually(Duration::from_secs(2), "the poll after v2", || {
        fake.seen.lock().unwrap().len() >= 3
    })
    .await;
    let seen = fake.seen.lock().unwrap().clone();
    assert_eq!(
        seen[..3],
        [0, 1, 2],
        "each poll asks for versions after the last applied"
    );
}

#[tokio::test]
async fn keeps_last_good_on_server_error_and_bad_yaml() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_path(&dir);
    let (fake, url) = fake_server(vec![
        Scripted::Config(1, V1),
        Scripted::Status(500),
        Scripted::Config(2, "::not yaml"),
        Scripted::Config(3, "binds: 3\n"), // parses, but agentgateway's schema refuses it
    ])
    .await;
    tokio::spawn(run_agent(agent(url, &path, TOKEN)));
    eventually(Duration::from_secs(3), "the whole script served", || {
        fake.seen.lock().unwrap().len() >= 5
    })
    .await;
    assert_eq!(read(&path).as_deref(), Some(V1));
    let seen = fake.seen.lock().unwrap().clone();
    // After the 500 it asks again for the same version; bad versions are skipped, not re-fetched.
    assert_eq!(seen[..5], [0, 1, 1, 2, 3]);
}

#[tokio::test]
async fn unreachable_server_keeps_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_path(&dir);
    std::fs::write(&path, V1).unwrap();
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/", closed.local_addr().unwrap())).unwrap();
    drop(closed);
    let task = tokio::spawn(run_agent(agent(url, &path, TOKEN)));
    sleep(Duration::from_millis(500)).await;
    assert!(!task.is_finished(), "the agent keeps retrying");
    assert_eq!(read(&path).as_deref(), Some(V1));
    task.abort();
}

#[tokio::test]
async fn wrong_token_writes_nothing_and_retries() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_path(&dir);
    let (fake, url) = fake_server(vec![Scripted::Config(1, V1)]).await;
    let task = tokio::spawn(run_agent(agent(url, &path, "wrong-token-0123456789")));
    eventually(Duration::from_secs(2), "repeated attempts", || {
        *fake.refused.lock().unwrap() >= 3
    })
    .await;
    assert_eq!(read(&path), None);
    assert!(!task.is_finished());
    task.abort();
}

/// A server that answers "unchanged" at once (no long poll) must not make
/// the agent poll in a tight loop.
#[tokio::test]
async fn an_instant_unchanged_answer_does_not_spin() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_path(&dir);
    let script = (0..10_000).map(|_| Scripted::Status(204)).collect();
    let (fake, url) = fake_server(script).await;
    tokio::spawn(run_agent(agent(url, &path, TOKEN))); // retry is 50 ms
    sleep(Duration::from_millis(500)).await;
    let polls = fake.seen.lock().unwrap().len();
    assert!((2..=15).contains(&polls), "{polls} polls in 500 ms");
}

/// After the server's database is re-created its versions start again, lower
/// than the agent's. The agent takes whatever the server says is newest.
#[tokio::test]
async fn follows_a_server_whose_versions_went_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_path(&dir);
    let (fake, url) = fake_server(vec![Scripted::Config(57, V1), Scripted::Config(2, V2)]).await;
    tokio::spawn(run_agent(agent(url, &path, TOKEN)));
    eventually(Duration::from_secs(2), "v2 on disk", || {
        read(&path).as_deref() == Some(V2)
    })
    .await;
    eventually(Duration::from_secs(2), "the poll after v2", || {
        fake.seen.lock().unwrap().len() >= 3
    })
    .await;
    assert_eq!(fake.seen.lock().unwrap()[..3], [0, 57, 2]);
}

#[tokio::test]
async fn https_is_refused_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let url = Url::parse("https://sekisho.example.com/").unwrap();
    let err = run_agent(agent(url, &config_path(&dir), TOKEN))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("https"), "{err}");
}

#[test]
fn write_atomic_replaces_whole_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_path(&dir);
    std::fs::write(&path, "a much longer previous config\n".repeat(10)).unwrap();
    write_atomic(&path, "short\n").unwrap();
    assert_eq!(read(&path).as_deref(), Some("short\n"));
    let names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["config.yaml"], "no temp files remain");
}

#[test]
fn write_atomic_creates_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_path(&dir);
    write_atomic(&path, V1).unwrap();
    assert_eq!(read(&path).as_deref(), Some(V1));
}

#[cfg(unix)]
#[test]
fn write_atomic_makes_the_file_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = config_path(&dir);
    write_atomic(&path, V1).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
