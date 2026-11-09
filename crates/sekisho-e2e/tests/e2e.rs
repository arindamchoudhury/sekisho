//! Plan 1's acceptance tests, against the stack in `e2e/docker-compose.yml`.
//! Ignored by default; run with the stack up:
//!   cargo test -p sekisho-e2e -- --ignored --test-threads=1

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tokio::time::{Instant, sleep};

const ADMIN: &str = "http://127.0.0.1:18080";
const GATEWAY: &str = "http://127.0.0.1:3000";
const MOCK: &str = "http://127.0.0.1:18000";
const BOOTSTRAP: &str = "e2e-bootstrap-token-0123456789";
const MOCK_PROVIDER_KEY: &str = "mock-provider-key-e2e";
const MODEL: &str = "mock-model";

fn http() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap()
}

async fn admin(method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = http()
        .request(method.parse().unwrap(), format!("{ADMIN}{path}"))
        .bearer_auth(BOOTSTRAP);
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req
        .send()
        .await
        .expect("admin API reachable (is the stack up?)");
    let status = resp.status();
    let text = resp.text().await.unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// A name no earlier run used.
fn unique(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{}", nanos % 1_000_000_000_000)
}

/// The mock provider and model, and `data-team`. Safe to call again.
async fn seed() {
    let (s, _) = admin(
        "POST",
        "/api/v1/providers",
        Some(json!({ "name": "mock", "kind": "openai_compatible", "base_url": "http://mock-model:8000/v1", "api_key": MOCK_PROVIDER_KEY })),
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

async fn put_env(name: &str) {
    let (s, body) = admin(
        "PUT",
        &format!("/api/v1/environments/{name}"),
        Some(json!({ "models": [MODEL] })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

/// A fresh key for `env`: (id, token).
async fn new_key(env: &str) -> (String, String) {
    let (s, body) = admin(
        "POST",
        &format!("/api/v1/environments/{env}/keys"),
        Some(json!({ "owner": "e2e@example.com", "name": unique("key") })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    (
        body["id"].as_str().unwrap().into(),
        body["token"].as_str().unwrap().into(),
    )
}

/// An environment budget of `usd` this month; returns its id.
async fn env_budget(env: &str, usd: &str) -> String {
    let (s, body) = admin(
        "POST",
        "/api/v1/budgets",
        Some(json!({ "name": unique("budget"), "scope": "environment", "environment": env, "limit": usd, "window": "month" })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().into()
}

async fn spent(budget: &str) -> String {
    let (_, body) = admin("GET", &format!("/api/v1/budgets/{budget}/status"), None).await;
    body["spent"].as_str().unwrap_or("?").into()
}

async fn wait_spent(budget: &str, usd: &str) {
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

fn chat_request(
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
async fn chat(env: &str, token: &str, chunks: u32) -> (StatusCode, Value) {
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

/// Waits until `env`'s route is live and its key is accepted. The probe asks
/// for a model the route doesn't allow, so it never reaches the model and
/// costs nothing.
async fn wait_route(env: &str, token: &str) {
    let start = Instant::now();
    loop {
        let resp = chat_request(
            &http(),
            &format!("/env/{env}/openai"),
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
            "route for {env} not live: {status} {body}"
        );
        sleep(Duration::from_millis(100)).await;
    }
}

/// An environment of its own, live at the gateway, with a key.
async fn fresh_env(prefix: &str) -> (String, String, String) {
    seed().await;
    let env = unique(prefix);
    put_env(&env).await;
    let (id, token) = new_key(&env).await;
    wait_route(&env, &token).await;
    (env, id, token)
}

fn compose(args: &[&str]) {
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

fn median(mut values: Vec<Duration>) -> Duration {
    values.sort();
    values[values.len() / 2]
}

#[tokio::test]
#[ignore]
async fn new_environment_live_under_5s() {
    seed().await;
    let env = unique("ml-team");
    let started = Instant::now();
    put_env(&env).await;
    let (_, token) = new_key(&env).await;
    loop {
        let (status, _) = chat(&env, &token, 1).await;
        if status == StatusCode::OK {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{env} not live within 5 s (last {status})"
        );
        sleep(Duration::from_millis(50)).await;
    }
    println!("{env} live after {:?}", started.elapsed());
}

#[tokio::test]
#[ignore]
async fn revoke_cuts_running_stream_under_2s() {
    let (env, id, token) = fresh_env("revoke").await;
    let mut resp = chat_request(
        &http(),
        &format!("/env/{env}/openai"),
        Some(&token),
        MODEL,
        15,
        1000,
        true,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut text = String::new();
    let mut revoked_at = None;
    let mut last_chunk_at = Instant::now();
    // The stream ends cleanly or with an error once it is cut; both end the loop.
    while let Ok(Some(bytes)) = resp.chunk().await {
        last_chunk_at = Instant::now();
        if let Some(at) = revoked_at {
            assert!(
                last_chunk_at.duration_since(at) <= Duration::from_secs(2),
                "a chunk arrived {:?} after revocation",
                last_chunk_at.duration_since(at)
            );
        }
        text.push_str(&String::from_utf8_lossy(&bytes));
        if revoked_at.is_none() && text.matches("\"content\"").count() >= 3 {
            let (s, _) = admin("POST", &format!("/api/v1/keys/{id}/revoke"), None).await;
            assert_eq!(s, StatusCode::NO_CONTENT);
            revoked_at = Some(Instant::now());
        }
    }
    let revoked_at = revoked_at.expect("three chunks before the revocation");
    let ended = last_chunk_at.max(Instant::now()).duration_since(revoked_at);
    println!(
        "stream ended {ended:?} after revocation; {} content chunks",
        text.matches("\"content\"").count()
    );
    assert!(
        ended <= Duration::from_secs(2) + Duration::from_millis(500),
        "stream ran {ended:?} past revocation"
    );
    assert!(!text.contains("[DONE]"), "the stream was not cut");
    let (status, body) = chat(&env, &token, 1).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["reason"], "revoked", "{body}");
}

#[tokio::test]
#[ignore]
async fn budget_refuses_next_request_with_message() {
    seed().await;
    let env = unique("budget-team");
    put_env(&env).await;
    // The budget comes first, so the free probe below is the only other request.
    let budget = env_budget(&env, "0.01").await;
    let (_, token) = new_key(&env).await;
    wait_route(&env, &token).await;
    // 100-chunk requests cost $0.0045 each (1000 in at $3/M, 100 out at $15/M).
    for (n, total) in [(1, "0.0045"), (2, "0.009"), (3, "0.0135")] {
        let (status, body) = chat(&env, &token, 100).await;
        assert_eq!(status, StatusCode::OK, "request {n}: {body}");
        wait_spent(&budget, total).await;
    }
    let (status, body) = chat(&env, &token, 100).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(body["error"], "budget_exhausted");
    let message = body["message"].as_str().unwrap();
    assert!(
        message.starts_with("Budget '") && message.contains("is used up until"),
        "{message}"
    );
}

#[tokio::test]
#[ignore]
async fn spend_equals_catalog_price() {
    seed().await;
    let env = unique("spend-team");
    put_env(&env).await;
    let budget = env_budget(&env, "1").await;
    let (_, token) = new_key(&env).await;
    wait_route(&env, &token).await;
    let (status, body) = chat(&env, &token, 100).await; // 1000 in, 100 out
    assert_eq!(status, StatusCode::OK, "{body}");
    wait_spent(&budget, "0.0045").await; // 4500 micro-USD
}

#[tokio::test]
#[ignore]
async fn hooks_add_under_5ms_median() {
    seed().await;
    let (_, token) = new_key("data-team").await;
    wait_route("data-team", &token).await;
    let client = http();
    let time = async |path: &str, token: Option<&str>| {
        let started = Instant::now();
        let resp = chat_request(&client, path, token, MODEL, 1, 0, false)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        resp.bytes().await.unwrap();
        started.elapsed()
    };
    for _ in 0..5 {
        time("/bare/openai", None).await; // warm up connections
        time("/env/data-team/openai", Some(&token)).await;
    }
    let (mut bare, mut hooked) = (Vec::new(), Vec::new());
    for _ in 0..40 {
        bare.push(time("/bare/openai", None).await);
        hooked.push(time("/env/data-team/openai", Some(&token)).await);
    }
    let (bare, hooked) = (median(bare), median(hooked));
    println!("median without hooks {bare:?}, with hooks {hooked:?}");
    assert!(
        hooked.saturating_sub(bare) < Duration::from_millis(5),
        "hooks add {:?}",
        hooked.saturating_sub(bare)
    );
}

#[tokio::test]
#[ignore]
async fn sekisho_token_never_reaches_provider() {
    seed().await;
    let (_, token) = new_key("data-team").await;
    wait_route("data-team", &token).await;
    // send with both `Authorization: Bearer <sek_ token>` and `x-api-key: <sek_ token>`, as Claude Code does
    let resp = chat_request(
        &http(),
        "/env/data-team/openai",
        Some(&token),
        MODEL,
        1,
        0,
        false,
    )
    .header("x-api-key", &token)
    .header("api-key", &token)
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let h: serde_json::Map<String, Value> = http()
        .get(format!("{MOCK}/_last_headers"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        h.values().all(|v| !v.as_str().unwrap().contains("sek_")),
        "{h:?}"
    );
    assert!(
        h.values()
            .any(|v| v.as_str().unwrap().contains(MOCK_PROVIDER_KEY)),
        "{h:?}"
    );
}

#[tokio::test]
#[ignore]
async fn unrelated_config_change_keeps_stream() {
    seed().await;
    let (_, token) = new_key("data-team").await;
    wait_route("data-team", &token).await;
    let mut resp = chat_request(
        &http(),
        "/env/data-team/openai",
        Some(&token),
        MODEL,
        10,
        300,
        true,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut text = String::new();
    let mut changed = false;
    while let Some(bytes) = resp.chunk().await.unwrap() {
        text.push_str(&String::from_utf8_lossy(&bytes));
        if !changed && text.contains("\"content\"") {
            // Two new config versions while the stream runs.
            put_env(&unique("ml-team")).await;
            put_env(&unique("ml-team")).await;
            changed = true;
        }
    }
    assert!(changed);
    assert_eq!(text.matches("\"content\"").count(), 10, "{text}");
    assert!(text.contains("[DONE]"));
}

#[tokio::test]
#[ignore]
async fn sekisho_down_requests_refused() {
    seed().await;
    let (_, token) = new_key("data-team").await;
    wait_route("data-team", &token).await;
    compose(&["stop", "sekisho"]);
    let (status, body) = chat("data-team", &token, 1).await;
    compose(&["start", "sekisho"]);
    assert_ne!(
        status,
        StatusCode::OK,
        "agentgateway served a request without its hooks: {body}"
    );
    // Leave the stack ready: wait until requests pass again.
    let start = Instant::now();
    while chat("data-team", &token, 1).await.0 != StatusCode::OK {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "sekisho did not come back"
        );
        sleep(Duration::from_millis(250)).await;
    }
}
