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

/// Waits until `env`'s models are live on its `format` route and its key is
/// accepted.
///
/// One `llm:` block serves every environment, so agentgateway answers 404
/// for any model of an environment it doesn't have yet, and a request for a
/// model the environment lists reaches the provider and is charged. So the
/// probe asks for a model nothing lists (refused before any provider,
/// costing nothing) only to learn that Sekisho's hook takes the key, and the
/// environment is looked up in the config the gateway agent wrote.
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
        // A JSON "reason": Sekisho's hook doesn't know the key yet.
        if !body.contains("\"reason\"") {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{format} route for {env}: the key isn't taken: {status} {body}"
        );
        sleep(Duration::from_millis(100)).await;
    }
    while !lists_env(&gateway_config(), env) {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "{env} not in the gateway's config"
        );
        sleep(Duration::from_millis(100)).await;
    }
    // agentgateway reloads a small file about 25 ms after a write. A config
    // of about 1,000 environments takes about half a second per load, so
    // `tests/scale.rs` waits for a served request instead.
    sleep(Duration::from_millis(500)).await;
}

/// Whether `yaml` has a model entry (`id: <env>/<model>`) for `env`.
fn lists_env(yaml: &str, env: &str) -> bool {
    let prefix = format!("{env}/");
    yaml.lines().any(|line| {
        line.trim_start()
            .strip_prefix("- id:")
            .is_some_and(|id| id.trim().trim_matches(['\'', '"']).starts_with(&prefix))
    })
}

/// The config file the gateway agent last wrote for agentgateway.
pub fn gateway_config() -> String {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../e2e/docker-compose.yml");
    let out = Command::new("docker")
        .arg("compose")
        .arg("-f")
        .arg(&file)
        .args(["exec", "-T", "gateway-agent", "cat", "/config/config.yaml"])
        .output()
        .expect("docker");
    assert!(
        out.status.success(),
        "reading the gateway config: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("the config is UTF-8")
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

// Requests as each coding tool sends them, what reached the mock, and what
// was charged: shared by `tests/e2e.rs` and `tests/hosts.rs`.

/// A fresh environment for `models` with a $1 budget and a key, live on its
/// `format` route: (env, key id, token, budget id). Names start with `via-`
/// so they sort after `data-team`: the bare route of plan 1's latency test
/// serves the provider of the first model entry of an OpenAI kind, by
/// environment name.
pub async fn cloud_env(
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
pub fn messages_request(
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

/// An OpenAI Responses request as Codex sends it.
pub fn responses_request(
    env: &str,
    token: &str,
    model: &str,
    chunks: u32,
    interval_ms: u32,
    stream: bool,
) -> reqwest::RequestBuilder {
    http()
        .post(format!("{GATEWAY}/env/{env}/openai/v1/responses"))
        .bearer_auth(token)
        .header("x-mock-chunks", chunks.to_string())
        .header("x-mock-interval-ms", interval_ms.to_string())
        .json(&json!({ "model": model, "input": "hi", "stream": stream }))
}

/// The requests the mock received since `before`, each checked to carry no
/// Sekisho token in any header or in its path.
pub async fn requests_since(before: usize) -> Vec<Value> {
    let seen = mock_requests().await;
    let new = seen[before..].to_vec();
    for r in &new {
        assert!(!r["path"].as_str().unwrap().contains("sek_"), "{r}");
        let headers = r["headers"].as_object().unwrap();
        assert!(
            headers
                .values()
                .all(|v| !v.as_str().unwrap().contains("sek_")),
            "{r}"
        );
    }
    new
}

/// The one request the mock received since `before`, checked to carry no
/// Sekisho token and no client credential header.
pub async fn the_request_since(before: usize) -> Value {
    let new = requests_since(before).await;
    assert_eq!(new.len(), 1, "{new:?}");
    let r = new[0].clone();
    for client_header in ["x-api-key", "api-key"] {
        assert!(
            r["headers"].get(client_header).is_none(),
            "{client_header}: {r}"
        );
    }
    r
}

pub fn header<'a>(r: &'a Value, name: &str) -> &'a str {
    r["headers"][name].as_str().unwrap_or_default()
}

/// `usd` as a budget's status shows it ("0.000249"), in micro-USD.
pub fn micro_usd(usd: &str) -> i64 {
    let (whole, fraction) = usd.split_once('.').unwrap_or((usd, ""));
    let fraction = format!("{fraction:0<6}");
    let whole: i64 = whole
        .parse()
        .unwrap_or_else(|_| panic!("not an amount: {usd}"));
    whole * 1_000_000 + fraction[..6].parse::<i64>().unwrap()
}

/// The answer text of each complete server-sent event in `sse`, in the
/// OpenAI chat, Anthropic Messages or OpenAI Responses format.
pub fn deltas(sse: &str) -> Vec<String> {
    let complete = &sse[..sse.rfind('\n').map_or(0, |end| end + 1)];
    complete
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim_start()).ok())
        .filter_map(|event| {
            event["choices"][0]["delta"]["content"]
                .as_str()
                .or(event["delta"]["text"].as_str())
                .or((event["type"] == "response.output_text.delta")
                    .then(|| event["delta"].as_str())
                    .flatten())
                .map(str::to_owned)
        })
        .collect()
}

/// Streams `request` until `keep` answer deltas have arrived, then hangs up
/// as a client does on Esc. Returns the deltas read.
pub async fn read_then_hang_up(request: reqwest::RequestBuilder, keep: usize) -> Vec<String> {
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

/// Reads a whole streamed answer and returns its text.
pub async fn read_stream(request: reqwest::RequestBuilder) -> String {
    let resp = request.send().await.unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{text}");
    text
}

/// Waits up to 10 s for `budget` to be charged, then a second more for any
/// second charge, and returns its spend in micro-USD.
pub async fn charged(budget: &str) -> i64 {
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

/// Waits up to 10 s for `budget`'s spend to reach exactly `micro` micro-USD.
pub async fn wait_charged(budget: &str, micro: i64) {
    let start = Instant::now();
    loop {
        let now = micro_usd(&spent(budget).await);
        if now == micro {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "charged {now} micro-USD, expected {micro}"
        );
        sleep(Duration::from_millis(100)).await;
    }
}

pub fn assert_between(spent: i64, low: i64, high: i64) {
    println!("charged {spent} micro-USD; expected {low}..={high}");
    assert!(
        (low..=high).contains(&spent),
        "charged {spent} micro-USD, expected {low}..={high}"
    );
}

// ------------------------------------------------------------ catalogs (plan 3)

/// The mock as Sekisho reaches it inside the stack: Git, the registry,
/// agent cards and GitHub.
pub const MOCK_INTERNAL: &str = "http://mock-model:8000";

/// A repository on the mock, as Sekisho clones it.
pub fn git_url(repo: &str) -> String {
    format!("{MOCK_INTERNAL}/git/{repo}.git")
}

async fn mock_call(method: reqwest::Method, path: &str, body: Value) -> Value {
    let r = http()
        .request(method, format!("{MOCK}{path}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{path}: {}", r.status());
    r.json().await.unwrap_or(Value::Null)
}

/// Commits `files` (path, text) on the mock repository's `main`: the commit.
pub async fn git_commit(repo: &str, files: &[(&str, &str)]) -> String {
    let files: serde_json::Map<String, Value> = files
        .iter()
        .map(|(p, t)| ((*p).to_owned(), Value::String((*t).to_owned())))
        .collect();
    let body = mock_call(
        reqwest::Method::POST,
        &format!("/_git/{repo}/commit"),
        serde_json::json!({ "files": files }),
    )
    .await;
    body["commit"].as_str().unwrap().to_owned()
}

/// Removes `paths` on the mock repository's `main`: the commit.
pub async fn git_remove(repo: &str, paths: &[&str]) -> String {
    let body = mock_call(
        reqwest::Method::POST,
        &format!("/_git/{repo}/commit"),
        serde_json::json!({ "remove": paths }),
    )
    .await;
    body["commit"].as_str().unwrap().to_owned()
}

/// Points the mock registry's `repo:tag` at `digest`.
pub async fn set_image(repo: &str, tag: &str, digest: &str) {
    mock_call(
        reqwest::Method::PUT,
        &format!("/_registry/{repo}/{tag}"),
        serde_json::json!({ "digest": digest }),
    )
    .await;
}

/// Serves `card` as the mock agent's v1.0 card.
pub async fn set_card(agent: &str, card: Value) {
    mock_call(reqwest::Method::PUT, &format!("/_cards/{agent}"), card).await;
}

/// Adds a source of `kind` reading the mock repository `repo`, and waits
/// for its first sync: its report.
pub async fn add_source(name: &str, kind: &str, repo: &str) -> Value {
    let (s, body) = admin(
        "POST",
        "/api/v1/catalog-sources",
        Some(serde_json::json!({
            "name": name, "kind": kind, "url": git_url(repo), "allow_private_network": true
        })),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    wait_sync(name, None).await
}

/// The latest sync's report once it isn't `after` (a sync ID).
pub async fn wait_sync(source: &str, after: Option<&str>) -> Value {
    let path = format!("/api/v1/catalog-sources/{source}/syncs/latest");
    for _ in 0..600 {
        let (s, body) = admin("GET", &path, None).await;
        if s == StatusCode::OK && body["id"].as_str() != after {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no new sync of {source}");
}

/// Syncs `source` now and waits for the report.
pub async fn sync_now(source: &str) -> Value {
    let path = format!("/api/v1/catalog-sources/{source}/syncs/latest");
    let (_, before) = admin("GET", &path, None).await;
    let before = before["id"].as_str().map(str::to_owned);
    loop {
        let (s, body) = admin(
            "POST",
            &format!("/api/v1/catalog-sources/{source}/sync"),
            None,
        )
        .await;
        match s {
            StatusCode::ACCEPTED => break,
            StatusCode::CONFLICT => tokio::time::sleep(Duration::from_millis(200)).await,
            _ => panic!("sync of {source}: {s} {body}"),
        }
    }
    wait_sync(source, before.as_deref()).await
}
