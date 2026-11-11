//! The acceptance tests of plans 1, 1b and 1c, against the stack in
//! `e2e/docker-compose.yml`.
//! Ignored by default; run with the stack up:
//!   cargo test -p sekisho-e2e -- --ignored --test-threads=1

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::{Client, StatusCode};
use sekisho_hooks::tokens::count_text;
use serde_json::{Value, json};
use tokio::time::{Instant, sleep};

const ADMIN: &str = "http://127.0.0.1:18080";
const GATEWAY: &str = "http://127.0.0.1:3000";
const MOCK: &str = "http://127.0.0.1:18000";
const BOOTSTRAP: &str = "e2e-bootstrap-token-0123456789";
const MOCK_PROVIDER_KEY: &str = "mock-provider-key-e2e";
const MODEL: &str = "mock-model";
/// A second model on the mock provider, for environments that must differ.
const MODEL_B: &str = "mock-model-b";
/// Built-in provider kinds pointed at the mock: their keys and models.
const ANTHROPIC_KEY: &str = "mock-anthropic-key-e2e";
const OPENAI_KEY: &str = "mock-openai-key-e2e";
const CLAUDE_MODEL: &str = "claude-mock";
const GPT_MODEL: &str = "gpt-mock";

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

async fn put_env(name: &str) {
    put_env_models(name, &[MODEL]).await;
}

async fn put_env_models(name: &str, models: &[&str]) {
    let (s, body) = admin(
        "PUT",
        &format!("/api/v1/environments/{name}"),
        Some(json!({ "models": models })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
}

/// Creates a provider or model unless an earlier run did.
async fn ensure(path: &str, body: Value) {
    let (s, answer) = admin("POST", path, Some(body)).await;
    assert!(
        s == StatusCode::CREATED || s == StatusCode::CONFLICT,
        "{path}: {s} {answer}"
    );
}

/// Every request the mock model has received, oldest first.
async fn mock_requests() -> Vec<Value> {
    http()
        .get(format!("{MOCK}/_requests"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn chat_body(model: &str) -> Value {
    json!({ "model": model, "messages": [{ "role": "user", "content": "hi" }] })
}

/// Sends one HTTP/1.1 request to the gateway byte for byte, so the path
/// reaches agentgateway exactly as written (reqwest resolves `%2e%2e`), and
/// returns the status.
async fn raw_status(method: &str, path: &str, token: &str, body: &Value) -> u16 {
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

/// Waits until `env`'s OpenAI route is live and its key is accepted.
async fn wait_route(env: &str, token: &str) {
    wait_route_at(env, token, "openai").await;
}

/// Waits until `env`'s route for `format` is live and its key is accepted.
/// The probe asks for a model the route doesn't allow, so it never reaches
/// the model and costs nothing.
async fn wait_route_at(env: &str, token: &str, format: &str) {
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

/// Changing the environment a stream belongs to (here: allowing a second
/// model) re-renders that stream's own route. The change goes live while the
/// stream runs, and the stream still finishes.
#[tokio::test]
#[ignore]
async fn a_change_to_the_streams_own_environment_keeps_it() {
    let (env, _, token) = fresh_env("own-change").await;
    ensure(
        "/api/v1/models",
        json!({ "name": MODEL_B, "provider": "mock", "input_price": "3", "output_price": "15" }),
    )
    .await;
    let route = format!("/env/{env}/openai");
    let mut resp = chat_request(&http(), &route, Some(&token), MODEL, 10, 300, true)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut text = String::new();
    let mut changed = false;
    while let Some(bytes) = resp.chunk().await.unwrap() {
        text.push_str(&String::from_utf8_lossy(&bytes));
        if !changed && text.contains("\"content\"") {
            put_env_models(&env, &[MODEL, MODEL_B]).await;
            let start = Instant::now();
            loop {
                let status = chat_request(&http(), &route, Some(&token), MODEL_B, 1, 0, false)
                    .send()
                    .await
                    .unwrap()
                    .status();
                if status == StatusCode::OK {
                    break;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(2),
                    "{env}'s change not live within 2 s (last {status})"
                );
                sleep(Duration::from_millis(50)).await;
            }
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

/// A model the environment doesn't list is refused before the provider.
#[tokio::test]
#[ignore]
async fn unlisted_model_never_reaches_the_provider() {
    let (env, _, token) = fresh_env("listed").await;
    let before = mock_requests().await.len();
    let resp = chat_request(
        &http(),
        &format!("/env/{env}/openai"),
        Some(&token),
        "gpt-not-listed",
        1,
        0,
        false,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        mock_requests().await.len(),
        before,
        "the provider was called"
    );
}

/// Whatever path or method a client uses under a model route, the provider
/// sees only a POST to its model endpoint: a key can't reach the provider's
/// other endpoints (files, stored completions, fine-tuning) with its key.
#[tokio::test]
#[ignore]
async fn only_model_calls_reach_the_provider() {
    let (env, _, token) = fresh_env("paths").await;
    let before = mock_requests().await.len();
    for (method, path) in [
        ("GET", "/v1/models"),
        ("GET", "/v1/files"),
        ("POST", "/v1/files"),
        ("DELETE", "/v1/files/file-abc"),
        ("GET", "/v1/chat/completions"),
        ("POST", "/v1/fine_tuning/jobs"),
    ] {
        let status = raw_status(
            method,
            &format!("/env/{env}/openai{path}"),
            &token,
            &chat_body(MODEL),
        )
        .await;
        println!("{method} {path} -> {status}");
    }
    for r in &mock_requests().await[before..] {
        assert_eq!(
            (r["method"].as_str(), r["path"].as_str()),
            (Some("POST"), Some("/v1/chat/completions")),
            "{r}"
        );
    }
}

/// A key for one environment never gets another environment's models, however
/// the path is encoded. agentgateway v1.7.0 matches routes on the raw path;
/// Sekisho takes the environment from the matched route, never the path.
#[tokio::test]
#[ignore]
async fn encoded_paths_cannot_cross_environments() {
    let (a, _, token_a) = fresh_env("cross-a").await;
    ensure(
        "/api/v1/models",
        json!({ "name": MODEL_B, "provider": "mock", "input_price": "3", "output_price": "15" }),
    )
    .await;
    let b = unique("cross-b");
    put_env_models(&b, &[MODEL_B]).await;
    let (_, token_b) = new_key(&b).await;
    wait_route(&b, &token_b).await;
    let body = chat_body(MODEL_B);
    assert_eq!(
        raw_status(
            "POST",
            &format!("/env/{b}/openai/v1/chat/completions"),
            &token_b,
            &body
        )
        .await,
        200,
        "b's own key works"
    );
    for path in [
        format!("/env/{b}/openai/v1/chat/completions"),
        format!("/env/{a}/openai/%2e%2e/%2e%2e/{b}/openai/v1/chat/completions"),
        format!("/env/{a}/openai/%2E%2E/%2E%2E/{b}/openai/v1/chat/completions"),
        format!("/env/{a}/openai/..%2f..%2f{b}/openai/v1/chat/completions"),
        format!("/env/{a}/openai/../../{b}/openai/v1/chat/completions"),
        format!("/env/{a}/openai//../../{b}/openai/v1/chat/completions"),
    ] {
        let status = raw_status("POST", &path, &token_a, &body).await;
        println!("{path} -> {status}");
        assert_ne!(status, 200, "{path}");
    }
}

/// Built-in provider kinds (here Anthropic and OpenAI, pointed at the mock
/// by a base URL without a path) get only their own key, never a client
/// credential, and only on their model endpoint. Anthropic-format requests
/// stay in Anthropic format.
#[tokio::test]
#[ignore]
async fn built_in_providers_get_their_key_on_their_model_endpoint() {
    seed().await;
    for (name, kind, key) in [
        ("mock-anthropic", "anthropic", ANTHROPIC_KEY),
        ("mock-openai", "openai", OPENAI_KEY),
    ] {
        ensure(
            "/api/v1/providers",
            json!({
                "name": name, "kind": kind, "base_url": "http://mock-model:8000", "api_key": key,
                "allow_private_network": true
            }),
        )
        .await;
    }
    for (model, provider) in [(CLAUDE_MODEL, "mock-anthropic"), (GPT_MODEL, "mock-openai")] {
        ensure(
            "/api/v1/models",
            json!({ "name": model, "provider": provider, "input_price": "3", "output_price": "15" }),
        )
        .await;
    }
    let env = unique("builtin");
    put_env_models(&env, &[CLAUDE_MODEL, GPT_MODEL]).await;
    let (_, token) = new_key(&env).await;
    wait_route(&env, &token).await;
    let before = mock_requests().await.len();

    // Claude Code sends its token as both Authorization and x-api-key.
    let messages = json!({ "model": CLAUDE_MODEL, "max_tokens": 16, "messages": [{ "role": "user", "content": "hi" }] });
    let resp = http()
        .post(format!("{GATEWAY}/env/{env}/anthropic/v1/messages"))
        .bearer_auth(&token)
        .header("x-api-key", &token)
        .header("anthropic-version", "2023-06-01")
        .header("x-mock-chunks", "1")
        .json(&messages)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let answer: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        answer["type"], "message",
        "not an Anthropic answer: {answer}"
    );

    let resp = chat_request(
        &http(),
        &format!("/env/{env}/openai"),
        Some(&token),
        GPT_MODEL,
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

    // Other paths under the routes still go only to the model endpoints.
    for (route, body) in [("anthropic", &messages), ("openai", &chat_body(GPT_MODEL))] {
        let status = raw_status(
            "POST",
            &format!("/env/{env}/{route}/v1/files"),
            &token,
            body,
        )
        .await;
        println!("POST /{route}/v1/files -> {status}");
    }

    for r in &mock_requests().await[before..] {
        let headers = r["headers"].as_object().unwrap();
        assert!(
            headers
                .values()
                .all(|v| !v.as_str().unwrap().contains("sek_")),
            "{r}"
        );
        match (r["method"].as_str(), r["path"].as_str()) {
            (Some("POST"), Some("/v1/messages")) => {
                assert_eq!(headers.get("x-api-key"), Some(&json!(ANTHROPIC_KEY)), "{r}");
                assert_eq!(headers.get("authorization"), None, "{r}");
            }
            (Some("POST"), Some("/v1/chat/completions")) => {
                assert_eq!(
                    headers.get("authorization"),
                    Some(&json!(format!("Bearer {OPENAI_KEY}"))),
                    "{r}"
                );
                assert_eq!(headers.get("x-api-key"), None, "{r}");
            }
            _ => panic!("reached the provider: {r}"),
        }
    }
}

// Bedrock and Azure (plan 1b). Their providers point at the mock model; the
// e2e image is built with sekisho-server/e2e, which is the only build that
// lets cloud credentials go to a host other than AWS's or Azure's. The mock
// is on Docker's private network, hence allow_private_network.

/// Bedrock on the Anthropic route with AWS access keys.
const BEDROCK_CLAUDE: &str = "us.anthropic.claude-mock-v1:0";
const E2E_ACCESS_KEY: &str = "AKIAE2EACCESSKEY01";
/// Bedrock on the Anthropic route with the gateway's own AWS credentials
/// (set on the agentgateway service in e2e/docker-compose.yml).
const BEDROCK_CLOUD_CLAUDE: &str = "eu.anthropic.claude-mock-v1:0";
const GATEWAY_ACCESS_KEY: &str = "AKIAE2EGATEWAY00001";
/// Bedrock on the OpenAI route with a Bedrock API key.
const BEDROCK_OSS: &str = "gpt-oss-mock";
const BEDROCK_API_KEY: &str = "e2e-bedrock-api-key";
/// Azure OpenAI with an API key.
const AZURE_DEPLOYMENT: &str = "gpt-mock-deployment";
const AZURE_KEY: &str = "e2e-azure-key";

/// Creates a provider on the mock and its model (3 / 15 USD per million
/// tokens) unless an earlier run did.
async fn ensure_cloud_provider(provider: Value, model: &str) {
    let mut provider = provider;
    provider["base_url"] = json!("http://mock-model:8000");
    provider["allow_private_network"] = json!(true);
    let name = provider["name"].clone();
    ensure("/api/v1/providers", provider).await;
    ensure(
        "/api/v1/models",
        json!({ "name": model, "provider": name, "input_price": "3", "output_price": "15" }),
    )
    .await;
}

async fn ensure_bedrock_keys() {
    ensure_cloud_provider(
        json!({
            "name": "bedrock-keys", "kind": "bedrock", "format": "anthropic", "region": "us-east-1",
            "aws_access_key_id": E2E_ACCESS_KEY, "aws_secret_access_key": "e2e-aws-secret"
        }),
        BEDROCK_CLAUDE,
    )
    .await;
}

/// A fresh environment for `models` with a $1 budget and a key, live on its
/// `format` route: (env, key id, token, budget id). Names start with `via-`
/// so they sort after `data-team`: the bare route of plan 1's latency test
/// serves the first environment, by name, that has an OpenAI route.
async fn cloud_env(
    prefix: &str,
    models: &[&str],
    format: &str,
) -> (String, String, String, String) {
    let env = unique(&format!("via-{prefix}"));
    put_env_models(&env, models).await;
    let budget = env_budget(&env, "1").await;
    let (id, token) = new_key(&env).await;
    wait_route_at(&env, &token, format).await;
    (env, id, token, budget)
}

/// An Anthropic Messages request as Claude Code sends it: the Sekisho token
/// as both `Authorization` and `x-api-key`.
fn messages_request(
    env: &str,
    token: &str,
    model: &str,
    chunks: u32,
    interval_ms: u32,
    stream: bool,
) -> reqwest::RequestBuilder {
    http()
        .post(format!("{GATEWAY}/env/{env}/anthropic/v1/messages"))
        .bearer_auth(token)
        .header("x-api-key", token)
        .header("anthropic-version", "2023-06-01")
        .header("x-mock-chunks", chunks.to_string())
        .header("x-mock-interval-ms", interval_ms.to_string())
        .json(&json!({
            "model": model, "max_tokens": 64, "stream": stream,
            "messages": [{ "role": "user", "content": "hi" }]
        }))
}

/// The one request the mock received since `before`, checked to carry no
/// Sekisho token and no client credential header.
async fn the_request_since(before: usize) -> Value {
    let seen = mock_requests().await;
    let new = &seen[before..];
    assert_eq!(new.len(), 1, "{new:?}");
    let r = new[0].clone();
    let headers = r["headers"].as_object().unwrap();
    assert!(
        headers
            .values()
            .all(|v| !v.as_str().unwrap().contains("sek_")),
        "{r}"
    );
    for client_header in ["x-api-key", "api-key"] {
        assert!(headers.get(client_header).is_none(), "{client_header}: {r}");
    }
    r
}

fn header<'a>(r: &'a Value, name: &str) -> &'a str {
    r["headers"][name].as_str().unwrap_or_default()
}

#[tokio::test]
#[ignore]
async fn bedrock_claude_code_request_is_signed_with_access_keys() {
    ensure_bedrock_keys().await;
    let (env, _, token, _) = cloud_env("bedrock-keys", &[BEDROCK_CLAUDE], "anthropic").await;
    let before = mock_requests().await.len();
    let resp = messages_request(&env, &token, BEDROCK_CLAUDE, 1, 0, false)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let answer: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        answer["type"], "message",
        "not an Anthropic answer: {answer}"
    );

    let r = the_request_since(before).await;
    assert_eq!(r["method"], "POST", "{r}");
    assert_eq!(
        r["path"],
        format!("/model/{BEDROCK_CLAUDE}/converse"),
        "{r}"
    );
    let signature = header(&r, "authorization");
    assert!(
        signature.starts_with(&format!("AWS4-HMAC-SHA256 Credential={E2E_ACCESS_KEY}/"))
            && signature.contains("/us-east-1/bedrock/aws4_request"),
        "{r}"
    );
}

#[tokio::test]
#[ignore]
async fn bedrock_stream_is_translated_and_charged() {
    ensure_bedrock_keys().await;
    let (env, _, token, budget) = cloud_env("bedrock-stream", &[BEDROCK_CLAUDE], "anthropic").await;
    let resp = messages_request(&env, &token, BEDROCK_CLAUDE, 10, 50, true)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = resp.text().await.unwrap();
    assert!(text.contains("event: message_start"), "{text}");
    assert_eq!(
        text.matches("event: content_block_delta").count(),
        10,
        "{text}"
    );
    assert!(text.contains("event: message_stop"), "{text}");
    // 1000 input tokens at $3/M and 10 output at $15/M, at Sekisho's price.
    wait_spent(&budget, "0.00315").await;
}

#[tokio::test]
#[ignore]
async fn revoking_a_key_cuts_a_bedrock_stream_under_2s() {
    ensure_bedrock_keys().await;
    let (env, id, token, _) = cloud_env("bedrock-revoke", &[BEDROCK_CLAUDE], "anthropic").await;
    let mut resp = messages_request(&env, &token, BEDROCK_CLAUDE, 20, 300, true)
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
        if revoked_at.is_none() && text.contains("event: content_block_delta") {
            let (s, _) = admin("POST", &format!("/api/v1/keys/{id}/revoke"), None).await;
            assert_eq!(s, StatusCode::NO_CONTENT);
            revoked_at = Some(Instant::now());
        }
    }
    let revoked_at = revoked_at.expect("a delta before the revocation");
    let ended = last_chunk_at.max(Instant::now()).duration_since(revoked_at);
    println!(
        "Bedrock stream ended {ended:?} after revocation; {} deltas",
        text.matches("event: content_block_delta").count()
    );
    assert!(
        ended <= Duration::from_secs(2) + Duration::from_millis(500),
        "stream ran {ended:?} past revocation"
    );
    assert!(!text.contains("message_stop"), "the stream was not cut");
}

#[tokio::test]
#[ignore]
async fn bedrock_openai_route_uses_its_api_key() {
    ensure_cloud_provider(
        json!({
            "name": "bedrock-oss", "kind": "bedrock", "format": "openai", "region": "us-east-1",
            "api_key": BEDROCK_API_KEY
        }),
        BEDROCK_OSS,
    )
    .await;
    let (env, _, token, _) = cloud_env("bedrock-oss", &[BEDROCK_OSS], "openai").await;
    let before = mock_requests().await.len();
    let resp = chat_request(
        &http(),
        &format!("/env/{env}/openai"),
        Some(&token),
        BEDROCK_OSS,
        1,
        0,
        false,
    )
    .header("x-api-key", &token)
    .header("api-key", &token)
    .send()
    .await
    .unwrap();
    let status = resp.status();
    let answer: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["object"], "chat.completion", "{answer}");

    let r = the_request_since(before).await;
    assert_eq!(r["path"], format!("/model/{BEDROCK_OSS}/converse"), "{r}");
    assert_eq!(
        header(&r, "authorization"),
        format!("Bearer {BEDROCK_API_KEY}"),
        "{r}"
    );
}

#[tokio::test]
#[ignore]
async fn bedrock_cloud_identity_signs_with_the_gateways_credentials() {
    ensure_cloud_provider(
        json!({
            "name": "bedrock-cloud", "kind": "bedrock", "format": "anthropic", "region": "us-east-1",
            "cloud_identity": true
        }),
        BEDROCK_CLOUD_CLAUDE,
    )
    .await;
    let (env, _, token, _) = cloud_env("bedrock-cloud", &[BEDROCK_CLOUD_CLAUDE], "anthropic").await;
    let before = mock_requests().await.len();
    let resp = messages_request(&env, &token, BEDROCK_CLOUD_CLAUDE, 1, 0, false)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let answer: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{answer}");

    let r = the_request_since(before).await;
    assert_eq!(
        r["path"],
        format!("/model/{BEDROCK_CLOUD_CLAUDE}/converse"),
        "{r}"
    );
    let signature = header(&r, "authorization");
    assert!(
        signature.starts_with(&format!(
            "AWS4-HMAC-SHA256 Credential={GATEWAY_ACCESS_KEY}/"
        )) && signature.contains("/us-east-1/bedrock/aws4_request"),
        "{r}"
    );
}

#[tokio::test]
#[ignore]
async fn azure_openai_route_uses_its_api_key_and_is_charged() {
    ensure_cloud_provider(
        json!({
            "name": "azure-openai", "kind": "azure", "format": "openai",
            "endpoint": "https://e2e-resource.openai.azure.com", "api_key": AZURE_KEY
        }),
        AZURE_DEPLOYMENT,
    )
    .await;
    let (env, _, token, budget) = cloud_env("azure", &[AZURE_DEPLOYMENT], "openai").await;
    let before = mock_requests().await.len();
    let resp = chat_request(
        &http(),
        &format!("/env/{env}/openai"),
        Some(&token),
        AZURE_DEPLOYMENT,
        1,
        0,
        false,
    )
    .header("x-api-key", &token)
    .header("api-key", &token)
    .send()
    .await
    .unwrap();
    let status = resp.status();
    let answer: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{answer}");
    // The mock echoes the model it was asked for.
    assert_eq!(answer["model"], AZURE_DEPLOYMENT, "{answer}");

    let r = the_request_since(before).await;
    assert_eq!(
        (&r["method"], &r["path"]),
        (&json!("POST"), &json!("/openai/v1/chat/completions")),
        "{r}"
    );
    assert_eq!(
        header(&r, "authorization"),
        format!("Bearer {AZURE_KEY}"),
        "{r}"
    );
    // 1000 input tokens at $3/M and 1 output at $15/M.
    wait_spent(&budget, "0.003015").await;
}

// What a request costs (plan 1c). agentgateway reports a stream that ends
// early only when it starts (Task 7), so Sekisho charges it from what its
// stream hook relayed: the provider's own counts where its usage events got
// through, else agentgateway's count of the prompt plus the answer text,
// each o200k event counted on its own. The mock reports its usage (1,000
// prompt tokens and one per chunk) at the end of a stream.

/// agentgateway's `tokenize` count of `chat_body`'s one "hi" message.
const OPENAI_PROMPT: i64 = 8;
/// The mock's own prompt count, in its usage event.
const MOCK_PROMPT: i64 = 1_000;

/// `usd` as a budget's status shows it ("0.000249"), in micro-USD.
fn micro_usd(usd: &str) -> i64 {
    let (whole, fraction) = usd.split_once('.').unwrap_or((usd, ""));
    let fraction = format!("{fraction:0<6}");
    let whole: i64 = whole
        .parse()
        .unwrap_or_else(|_| panic!("not an amount: {usd}"));
    whole * 1_000_000 + fraction[..6].parse::<i64>().unwrap()
}

/// The answer text of each complete server-sent event in `sse`, in the
/// OpenAI chat or Anthropic Messages format.
fn deltas(sse: &str) -> Vec<String> {
    let complete = &sse[..sse.rfind('\n').map_or(0, |end| end + 1)];
    complete
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim_start()).ok())
        .filter_map(|event| {
            event["choices"][0]["delta"]["content"]
                .as_str()
                .or(event["delta"]["text"].as_str())
                .map(str::to_owned)
        })
        .collect()
}

/// Every delta of a full 20-chunk mock answer.
fn all_deltas() -> Vec<String> {
    (0..20).map(|i| format!("t{i} ")).collect()
}

/// The o200k tokens of `deltas`, each counted on its own as the stream hook
/// counts them.
fn o200k(deltas: &[String]) -> i64 {
    deltas.iter().map(|d| count_text(d) as i64).sum()
}

/// The cost at 3 / 15 USD per million tokens, in micro-USD.
fn estimate(prompt: i64, answer: i64) -> i64 {
    3 * prompt + 15 * answer
}

/// Streams `request` until `keep` answer deltas have arrived, then hangs up
/// as a client does on Esc. Returns the deltas read.
async fn read_then_hang_up(request: reqwest::RequestBuilder, keep: usize) -> Vec<String> {
    let mut resp = request.send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut text = String::new();
    while let Some(bytes) = resp.chunk().await.unwrap() {
        text.push_str(&String::from_utf8_lossy(&bytes));
        let read = deltas(&text);
        if read.len() >= keep {
            return read; // dropping the response closes the connection
        }
    }
    panic!("the stream ended before {keep} deltas: {text}");
}

/// Waits up to 10 s for `budget` to be charged, then a second more for any
/// second charge, and returns its spend in micro-USD.
async fn charged(budget: &str) -> i64 {
    let start = Instant::now();
    loop {
        if micro_usd(&spent(budget).await) > 0 {
            sleep(Duration::from_secs(1)).await;
            return micro_usd(&spent(budget).await);
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "nothing charged within 10 s"
        );
        sleep(Duration::from_millis(100)).await;
    }
}

fn assert_between(spent: i64, low: i64, high: i64) {
    println!("charged {spent} micro-USD; expected {low}..={high}");
    assert!(
        (low..=high).contains(&spent),
        "charged {spent} micro-USD, expected {low}..={high}"
    );
}

#[tokio::test]
#[ignore]
async fn a_cut_off_stream_is_charged_an_estimate() {
    seed().await;
    let (env, _, token, budget) = cloud_env("cut-off", &[MODEL], "openai").await;
    let request = chat_request(
        &http(),
        &format!("/env/{env}/openai"),
        Some(&token),
        MODEL,
        20,
        100,
        true,
    );
    read_then_hang_up(request, 5).await;
    // agentgateway keeps reading the provider after the client hangs up, so
    // the stream hook relays the whole answer, the provider's usage event
    // included: the charge is the provider's own count, 1,000 prompt and 20
    // answer tokens, though agentgateway never reported the request.
    let spent = charged(&budget).await;
    println!("charged {spent} micro-USD");
    assert_eq!(spent, estimate(MOCK_PROMPT, 20));
}

#[tokio::test]
#[ignore]
async fn a_stream_cut_by_revocation_is_charged_an_estimate() {
    seed().await;
    let (env, id, token, budget) = cloud_env("revoked", &[MODEL], "openai").await;
    let mut resp = chat_request(
        &http(),
        &format!("/env/{env}/openai"),
        Some(&token),
        MODEL,
        20,
        100,
        true,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut text = String::new();
    let mut revoked = false;
    // The stream ends cleanly or with an error once it is cut.
    while let Ok(Some(bytes)) = resp.chunk().await {
        text.push_str(&String::from_utf8_lossy(&bytes));
        if !revoked && deltas(&text).len() >= 3 {
            let (s, _) = admin("POST", &format!("/api/v1/keys/{id}/revoke"), None).await;
            assert_eq!(s, StatusCode::NO_CONTENT);
            revoked = true;
        }
    }
    assert!(revoked && !text.contains("[DONE]"), "not cut: {text}");
    let received = deltas(&text);
    println!("{} deltas before the cut", received.len());
    assert_between(
        charged(&budget).await,
        estimate(OPENAI_PROMPT, o200k(&received)),
        estimate(OPENAI_PROMPT, o200k(&all_deltas())),
    );
}

#[tokio::test]
#[ignore]
async fn a_cut_off_bedrock_stream_is_charged_an_estimate() {
    ensure_bedrock_keys().await;
    let (env, _, token, budget) = cloud_env("bedrock-cut", &[BEDROCK_CLAUDE], "anthropic").await;
    read_then_hang_up(
        messages_request(&env, &token, BEDROCK_CLAUDE, 20, 100, true),
        5,
    )
    .await;
    // As for OpenAI: Bedrock's usage, translated into the Anthropic stream's
    // last event, gets through after the hang-up.
    let spent = charged(&budget).await;
    println!("charged {spent} micro-USD");
    assert_eq!(spent, estimate(MOCK_PROMPT, 20));
}

#[tokio::test]
#[ignore]
async fn an_admin_price_wins_over_a_dated_model_name() {
    seed().await;
    ensure(
        "/api/v1/providers",
        json!({
            "name": "mock-openai", "kind": "openai", "base_url": "http://mock-model:8000",
            "api_key": OPENAI_KEY, "allow_private_network": true
        }),
    )
    .await;
    ensure(
        "/api/v1/models",
        json!({ "name": "gpt-4o", "provider": "mock-openai", "input_price": "1", "output_price": "1" }),
    )
    .await;
    let (env, _, token, budget) = cloud_env("dated", &["gpt-4o"], "openai").await;
    let resp = chat_request(
        &http(),
        &format!("/env/{env}/openai"),
        Some(&token),
        "gpt-4o",
        1,
        0,
        false,
    )
    .header("x-mock-model", "gpt-4o-2024-08-06")
    .send()
    .await
    .unwrap();
    let status = resp.status();
    let answer: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["model"], "gpt-4o-2024-08-06", "{answer}");
    // 1000 prompt and 1 answer token at the admin's 1 / 1 USD per million,
    // not the catalog's 2.5 / 10 for the dated name (0.00251).
    wait_spent(&budget, "0.001001").await;
}
