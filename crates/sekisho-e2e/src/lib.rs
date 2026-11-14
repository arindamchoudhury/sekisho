//! End-to-end acceptance tests against the stack in
//! `e2e/docker-compose.yml`: `tests/e2e.rs` (plans 1 to 1c) and
//! `tests/people.rs` (plan 2's sign-in and people). They are ignored by
//! default:
//!
//! ```text
//! docker compose -f e2e/docker-compose.yml up -d --build --wait
//! cargo test -p sekisho-e2e -- --ignored --test-threads=1
//! docker compose -f e2e/docker-compose.yml down -v
//! ```
//!
//! This library holds what both share: the stack's addresses, calls to
//! Sekisho's API as the admin, and a browser that signs people in through
//! Dex (the code flow and the device flow).

mod sign_in;

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep};

pub use sign_in::*;

pub const ADMIN: &str = "http://127.0.0.1:18080";
pub const GATEWAY: &str = "http://127.0.0.1:3000";
pub const MOCK: &str = "http://127.0.0.1:18000";
pub const BOOTSTRAP: &str = "e2e-bootstrap-token-0123456789";
pub const MOCK_PROVIDER_KEY: &str = "mock-provider-key-e2e";
pub const MODEL: &str = "mock-model";
/// A second model on the mock provider, for environments that must differ.
pub const MODEL_B: &str = "mock-model-b";
/// Built-in provider kinds pointed at the mock: their keys and models.
pub const ANTHROPIC_KEY: &str = "mock-anthropic-key-e2e";
pub const OPENAI_KEY: &str = "mock-openai-key-e2e";
pub const CLAUDE_MODEL: &str = "claude-mock";
pub const GPT_MODEL: &str = "gpt-mock";

pub fn http() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap()
}

/// A call to Sekisho's API as the admin (see [`admin_token`]). A 401 (the
/// admin's session ended, say by a restart's lost cache) signs in again once.
pub async fn admin(method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (status, answer) = by(&admin_token().await, method, path, body.clone()).await;
    if status != StatusCode::UNAUTHORIZED {
        return (status, answer);
    }
    *ADMIN_SESSION.lock().await = None;
    by(&admin_token().await, method, path, body).await
}

/// A call to Sekisho's API with this access token.
pub async fn by(token: &str, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = http()
        .request(method.parse().unwrap(), format!("{ADMIN}{path}"))
        .bearer_auth(token);
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req
        .send()
        .await
        .expect("Sekisho's API reachable (is the stack up?)");
    let status = resp.status();
    let text = resp.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// A name no earlier run used.
pub fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{}", nanos % 1_000_000_000_000)
}

/// The mock provider and model, and `data-team`. Safe to call again.
pub async fn seed() {
    let (s, _) = admin(
        "POST",
        "/api/v1/providers",
        // The mock is on Docker's private network.
        Some(json!({
            "name": "mock", "kind": "openai_compatible", "base_url": "http://mock-model:8000/v1",
            "api_key": MOCK_PROVIDER_KEY, "allow_private_network": true
        })),
    )
    .await;
    assert!(
        s == StatusCode::CREATED || s == StatusCode::CONFLICT,
        "provider: {s}"
    );
    let (s, _) = admin(
        "POST",
        "/api/v1/models",
        Some(
            json!({ "name": MODEL, "provider": "mock", "input_price": "3", "output_price": "15" }),
        ),
    )
    .await;
    assert!(
        s == StatusCode::CREATED || s == StatusCode::CONFLICT,
        "model: {s}"
    );
    put_env("data-team").await;
}

pub async fn put_env(name: &str) {
    put_env_models(name, &[MODEL]).await;
}

pub async fn put_env_models(name: &str, models: &[&str]) {
    let (s, body) = admin(
        "PUT",
        &format!("/api/v1/environments/{name}"),
        Some(json!({ "models": models, "access_groups": [GROUP] })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

/// Creates a provider or model unless an earlier run did.
pub async fn ensure(path: &str, body: Value) {
    let (s, answer) = admin("POST", path, Some(body)).await;
    assert!(
        s == StatusCode::CREATED || s == StatusCode::CONFLICT,
        "{path}: {s} {answer}"
    );
}

/// Every request the mock model has received, oldest first.
pub async fn mock_requests() -> Vec<Value> {
    http()
        .get(format!("{MOCK}/_requests"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

pub fn chat_body(model: &str) -> Value {
    json!({ "model": model, "messages": [{ "role": "user", "content": "hi" }] })
}

/// Sends one HTTP/1.1 request to the gateway byte for byte, so the path
/// reaches agentgateway exactly as written (reqwest resolves `%2e%2e`), and
/// returns the status.
pub async fn raw_status(method: &str, path: &str, token: &str, body: &Value) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let body = body.to_string();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nhost: 127.0.0.1:3000\r\nauthorization: Bearer {token}\r\n\
         content-type: application/json\r\nx-mock-chunks: 1\r\nx-mock-interval-ms: 0\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let exchange = async {
        let mut tcp = tokio::net::TcpStream::connect("127.0.0.1:3000")
            .await
            .unwrap();
        tcp.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tcp.read_to_end(&mut response).await.unwrap();
        response
    };
    let response = tokio::time::timeout(Duration::from_secs(30), exchange)
        .await
        .expect("an answer within 30 s");
    let text = String::from_utf8_lossy(&response);
    text.split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no status line: {text}"))
}

/// A fresh key of the admin's for `env`: (id, token). Keys are made by
/// their owners; the admin is in [`GROUP`], which every test environment
/// admits.
pub async fn new_key(env: &str) -> (String, String) {
    let (s, body) = admin(
        "POST",
        "/api/v1/me/keys",
        Some(json!({ "environment": env, "name": unique("key") })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    (
        body["id"].as_str().unwrap().into(),
        body["token"].as_str().unwrap().into(),
    )
}

/// An environment budget of `usd` this month; returns its id.
pub async fn env_budget(env: &str, usd: &str) -> String {
    let (s, body) = admin(
        "POST",
        "/api/v1/budgets",
        Some(json!({ "name": unique("budget"), "scope": "environment", "environment": env, "limit": usd, "window": "month" })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().into()
}

pub async fn spent(budget: &str) -> String {
    let (_, body) = admin("GET", &format!("/api/v1/budgets/{budget}/status"), None).await;
    body["spent"].as_str().unwrap_or("?").into()
}

pub async fn wait_spent(budget: &str, usd: &str) {
    let start = Instant::now();
    loop {
        let now = spent(budget).await;
        if now == usd {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "spent {now}, expected {usd}"
        );
        sleep(Duration::from_millis(100)).await;
    }
}

pub fn chat_request(
    client: &Client,
    path: &str,
    token: Option<&str>,
    model: &str,
    chunks: u32,
    interval_ms: u32,
    stream: bool,
) -> reqwest::RequestBuilder {
    let mut req = client
        .post(format!("{GATEWAY}{path}/v1/chat/completions"))
        .header("x-mock-chunks", chunks.to_string())
        .header("x-mock-interval-ms", interval_ms.to_string())
        .json(&json!({ "model": model, "stream": stream, "messages": [{ "role": "user", "content": "hi" }] }));
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    req
}

/// A non-streaming request to `env`'s OpenAI route.
pub async fn chat(env: &str, token: &str, chunks: u32) -> (StatusCode, Value) {
    let resp = chat_request(
        &http(),
        &format!("/env/{env}/openai"),
        Some(token),
        MODEL,
        chunks,
        0,
        false,
    )
    .send()
    .await
    .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    (
        status,
        serde_json::from_str(&text).unwrap_or(json!({ "raw": text })),
    )
}

/// Waits until `env`'s OpenAI route is live and its key is accepted.
pub async fn wait_route(env: &str, token: &str) {
    wait_route_at(env, token, "openai").await;
}

/// Waits until `env`'s route for `format` is live and its key is accepted.
/// The probe asks for a model the route doesn't allow, so it never reaches
/// the model and costs nothing.
pub async fn wait_route_at(env: &str, token: &str, format: &str) {
    let start = Instant::now();
    loop {
        let resp = chat_request(
            &http(),
            &format!("/env/{env}/{format}"),
            Some(token),
            "e2e-probe-not-allowed",
            0,
            0,
            false,
        )
        .send()
        .await
        .unwrap();
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        // 404: no route yet. A JSON "reason": Sekisho's hook doesn't know the key yet.
        if status != StatusCode::NOT_FOUND && !body.contains("\"reason\"") {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{format} route for {env} not live: {status} {body}"
        );
        sleep(Duration::from_millis(100)).await;
    }
}

/// An environment of its own, live at the gateway, with a key.
pub async fn fresh_env(prefix: &str) -> (String, String, String) {
    seed().await;
    let env = unique(prefix);
    put_env(&env).await;
    let (id, token) = new_key(&env).await;
    wait_route(&env, &token).await;
    (env, id, token)
}

pub fn compose(args: &[&str]) {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../e2e/docker-compose.yml");
    let status = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(&file)
        .args(args)
        .status()
        .expect("docker");
    assert!(status.success(), "docker compose {args:?}");
}

pub fn median(mut values: Vec<Duration>) -> Duration {
    values.sort();
    values[values.len() / 2]
}
