//! Plan 1d's acceptance: providers chosen by model, open-weight hosts that
//! declare their APIs, the Gemini route, and Sekisho's refusal of a model an
//! environment or a client's API can't reach. Every provider points at the
//! mock model; each test checks what reached it (the path, the provider's
//! credential, no Sekisho token anywhere) and what was charged, in exact
//! micro-USD through a budget of the test's own.
//!
//! Prices are USD per million tokens, so a price of 1 is 1 micro-USD per
//! token. The mock reports 1,000 prompt tokens and one answer token per
//! chunk.

use std::time::Duration;

use reqwest::StatusCode;
use sekisho_e2e::*;
use serde_json::{Value, json};
use tokio::time::sleep;

/// An OpenAI-compatible host serving Chat Completions only (Scaleway's
/// shape), its model at input $1, output $2 and cache reads $0.10.
const CHAT_HOST_KEY: &str = "e2e-chat-host-key";
const QWEN: &str = "qwen-mock";
/// A second chat-only host, for two providers of one format.
const CHAT_HOST_B_KEY: &str = "e2e-chat-host-b-key";
const MISTRAL: &str = "mistral-mock";
/// A host serving Messages under its own path (DeepSeek's shape).
const MESSAGES_HOST_KEY: &str = "e2e-messages-host-key";
const KIMI: &str = "kimi-mock";
/// A host serving the Responses API.
const RESPONSES_HOST_KEY: &str = "e2e-responses-host-key";
const GPT_OSS: &str = "gptoss-mock";
/// Gemini, at $3 / $15. The key looks like a Google API key (`AIza…`):
/// agentgateway v1.7.0 moves only those to `x-goog-api-key` on the native
/// endpoints, and leaves anything else (an OAuth token) as a bearer token.
/// It is short, so it never matches the pattern secret scanners look for.
const GEMINI_KEY: &str = "AIza-e2e-mock";
const GEMINI: &str = "gemini-google-mock";
/// One Bedrock provider with access keys, its Claude at $3 / $15. The key
/// ID is shorter than a real one, so secret scanners never match it.
const BEDROCK_ACCESS_KEY: &str = "AKIAE2EHOSTSKEY01";
const BEDROCK_CLAUDE: &str = "us.anthropic.claude-hosts-v1:0";
/// One Azure OpenAI provider with a key, its deployment at $3 / $15.
const AZURE_KEY: &str = "e2e-hosts-azure-key";
const AZURE_GPT: &str = "gpt-hosts-deployment";
/// The `togetherai` preset, and a model agentgateway's catalog prices there
/// at $0.50 / $3.
const TOGETHER_KEY: &str = "e2e-together-key";
const TOGETHER_MODEL: &str = "Qwen/Qwen3.6-Plus";

/// The cost of one mock answer of `chunks` tokens, at `input` and `output`
/// micro-USD per token.
fn cost(input: i64, output: i64, chunks: i64) -> i64 {
    1_000 * input + chunks * output
}

/// Every provider and model these tests use, unless an earlier run made
/// them, and `data-team` (so the bare route keeps serving the `mock`
/// provider).
async fn ensure_hosts() {
    seed().await;
    let chat = json!([{ "api": "chat_completions" }]);
    for (name, key, apis) in [
        ("host-chat", CHAT_HOST_KEY, chat.clone()),
        ("host-chat-b", CHAT_HOST_B_KEY, chat),
        (
            "host-messages",
            MESSAGES_HOST_KEY,
            json!([{ "api": "chat_completions" }, { "api": "messages", "path": "/anthropic/v1/messages" }]),
        ),
        (
            "host-responses",
            RESPONSES_HOST_KEY,
            json!([{ "api": "chat_completions" }, { "api": "responses" }]),
        ),
    ] {
        ensure(
            "/api/v1/providers",
            json!({
                "name": name, "kind": "openai_compatible", "base_url": "http://mock-model:8000/v1",
                "apis": apis, "api_key": key, "allow_private_network": true
            }),
        )
        .await;
    }
    for (model, provider) in [
        (QWEN, "host-chat"),
        (MISTRAL, "host-chat-b"),
        (KIMI, "host-messages"),
        (GPT_OSS, "host-responses"),
    ] {
        ensure(
            "/api/v1/models",
            json!({
                "name": model, "provider": provider,
                "input_price": "1", "output_price": "2", "cache_read_price": "0.1"
            }),
        )
        .await;
    }
    for provider in [
        json!({ "name": "mock-anthropic", "kind": "anthropic", "api_key": ANTHROPIC_KEY }),
        json!({ "name": "host-google", "kind": "gemini", "api_key": GEMINI_KEY }),
        json!({
            "name": "host-bedrock", "kind": "bedrock", "region": "us-east-1",
            "aws_access_key_id": BEDROCK_ACCESS_KEY, "aws_secret_access_key": "e2e-hosts-secret"
        }),
        json!({
            "name": "host-azure", "kind": "azure",
            "endpoint": "https://e2e-hosts.openai.azure.com", "api_key": AZURE_KEY
        }),
    ] {
        let mut provider = provider;
        provider["base_url"] = json!("http://mock-model:8000");
        provider["allow_private_network"] = json!(true);
        ensure("/api/v1/providers", provider).await;
    }
    for (model, provider) in [
        (CLAUDE_MODEL, "mock-anthropic"),
        (GEMINI, "host-google"),
        (BEDROCK_CLAUDE, "host-bedrock"),
        (AZURE_GPT, "host-azure"),
    ] {
        ensure(
            "/api/v1/models",
            json!({ "name": model, "provider": provider, "input_price": "3", "output_price": "15" }),
        )
        .await;
    }
    ensure(
        "/api/v1/providers",
        json!({
            "name": "host-together", "preset": "togetherai",
            "base_url": "http://mock-model:8000/v1", "allow_private_network": true,
            "api_key": TOGETHER_KEY
        }),
    )
    .await;
    // No prices: agentgateway's catalog has the model under togetherai.
    ensure(
        "/api/v1/models",
        json!({ "name": TOGETHER_MODEL, "provider": "host-together" }),
    )
    .await;
}

/// Sends `request` and returns its status and body, as JSON if it is.
async fn send(request: reqwest::RequestBuilder) -> (StatusCode, Value, String) {
    let resp = request.send().await.unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    let json = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, json, text)
}

/// Sends `request`, expects 200, and returns the body.
async fn ok(request: reqwest::RequestBuilder) -> Value {
    let (status, json, text) = send(request).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    json
}

fn chat(env: &str, token: &str, model: &str, chunks: u32) -> reqwest::RequestBuilder {
    chat_request(
        &http(),
        &format!("/env/{env}/openai"),
        Some(token),
        model,
        chunks,
        0,
        false,
    )
}

/// A Gemini request as Gemini CLI sends it, with its key also in
/// `x-goog-api-key`, to `<model>:<action><query>`.
fn gemini_request(
    env: &str,
    token: &str,
    call: &str,
    query: &str,
    chunks: u32,
) -> reqwest::RequestBuilder {
    http()
        .post(format!(
            "{GATEWAY}/env/{env}/gemini/v1beta/models/{call}{query}"
        ))
        .bearer_auth(token)
        .header("x-goog-api-key", token)
        .header("x-mock-chunks", chunks.to_string())
        .header("x-mock-interval-ms", "0")
        .json(&json!({ "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }] }))
}

/// Asserts `r` was `POST <path>` with `authorization: Bearer <key>` and no
/// client credential header.
fn assert_bearer(r: &Value, path: &str, key: &str) {
    assert_eq!(
        (&r["method"], &r["path"]),
        (&json!("POST"), &json!(path)),
        "{r}"
    );
    assert_eq!(header(r, "authorization"), format!("Bearer {key}"), "{r}");
    for client_header in ["x-api-key", "api-key", "x-goog-api-key"] {
        assert!(
            r["headers"].get(client_header).is_none(),
            "{client_header}: {r}"
        );
    }
}

#[tokio::test]
#[ignore]
async fn a_chat_only_host_serves_every_tool() {
    ensure_hosts().await;
    let (env, _, token, budget) = cloud_env("chat-host", &[QWEN], "openai").await;
    let before = mock_requests().await.len();
    let each = cost(1, 2, 2);

    let answer = ok(messages_request(&env, &token, QWEN, 2, 0, false)).await;
    assert_eq!(answer["type"], "message", "{answer}");
    wait_charged(&budget, each).await;
    let text = read_stream(messages_request(&env, &token, QWEN, 2, 0, true)).await;
    assert_eq!(deltas(&text), ["t0 ", "t1 "], "{text}");
    wait_charged(&budget, 2 * each).await;
    let answer = ok(responses_request(&env, &token, QWEN, 2, 0, false)).await;
    assert_eq!(answer["object"], "response", "{answer}");
    wait_charged(&budget, 3 * each).await;
    let text = read_stream(responses_request(&env, &token, QWEN, 2, 0, true)).await;
    assert_eq!(deltas(&text), ["t0 ", "t1 "], "{text}");
    wait_charged(&budget, 4 * each).await;
    ok(chat(&env, &token, QWEN, 2)).await;
    wait_charged(&budget, 5 * each).await;

    let seen = requests_since(before).await;
    assert_eq!(seen.len(), 5, "{seen:?}");
    for r in &seen {
        assert_bearer(r, "/v1/chat/completions", CHAT_HOST_KEY);
    }
}

#[tokio::test]
#[ignore]
async fn a_host_that_speaks_messages_gets_them_unchanged() {
    ensure_hosts().await;
    let (env, _, token, budget) = cloud_env("messages-host", &[KIMI], "openai").await;
    let before = mock_requests().await.len();
    let answer = ok(messages_request(&env, &token, KIMI, 1, 0, false)).await;
    assert_eq!(answer["type"], "message", "{answer}");
    ok(responses_request(&env, &token, KIMI, 1, 0, false)).await;
    wait_charged(&budget, 2 * cost(1, 2, 1)).await;

    let seen = requests_since(before).await;
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_bearer(&seen[0], "/anthropic/v1/messages", MESSAGES_HOST_KEY);
    assert_bearer(&seen[1], "/v1/chat/completions", MESSAGES_HOST_KEY);
}

#[tokio::test]
#[ignore]
async fn a_host_that_speaks_responses_gets_them_unchanged() {
    ensure_hosts().await;
    let (env, _, token, budget) = cloud_env("responses-host", &[GPT_OSS], "openai").await;
    let before = mock_requests().await.len();
    let answer = ok(responses_request(&env, &token, GPT_OSS, 1, 0, false)).await;
    assert_eq!(answer["object"], "response", "{answer}");
    let text = read_stream(responses_request(&env, &token, GPT_OSS, 2, 0, true)).await;
    assert_eq!(deltas(&text), ["t0 ", "t1 "], "{text}");
    wait_charged(&budget, cost(1, 2, 1) + cost(1, 2, 2)).await;

    let seen = requests_since(before).await;
    assert_eq!(seen.len(), 2, "{seen:?}");
    for r in &seen {
        assert_bearer(r, "/v1/responses", RESPONSES_HOST_KEY);
    }
}

#[tokio::test]
#[ignore]
async fn claude_and_an_open_weight_model_share_an_environment() {
    ensure_hosts().await;
    let (env, _, token, budget) = cloud_env("mixed", &[CLAUDE_MODEL, QWEN], "openai").await;
    let before = mock_requests().await.len();
    ok(messages_request(&env, &token, CLAUDE_MODEL, 1, 0, false)).await;
    ok(messages_request(&env, &token, QWEN, 1, 0, false)).await;
    wait_charged(&budget, cost(3, 15, 1) + cost(1, 2, 1)).await;

    let seen = requests_since(before).await;
    assert_eq!(seen.len(), 2, "{seen:?}");
    let claude = &seen[0];
    assert_eq!(claude["path"], "/v1/messages", "{claude}");
    assert_eq!(header(claude, "x-api-key"), ANTHROPIC_KEY, "{claude}");
    assert!(claude["headers"].get("authorization").is_none(), "{claude}");
    assert_bearer(&seen[1], "/v1/chat/completions", CHAT_HOST_KEY);
}

#[tokio::test]
#[ignore]
async fn two_providers_of_one_format_share_an_environment() {
    ensure_hosts().await;
    let (env, _, token, budget) = cloud_env("two-hosts", &[QWEN, MISTRAL], "openai").await;
    let before = mock_requests().await.len();
    ok(chat(&env, &token, QWEN, 1)).await;
    ok(chat(&env, &token, MISTRAL, 1)).await;
    wait_charged(&budget, 2 * cost(1, 2, 1)).await;

    let seen = requests_since(before).await;
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_bearer(&seen[0], "/v1/chat/completions", CHAT_HOST_KEY);
    assert_bearer(&seen[1], "/v1/chat/completions", CHAT_HOST_B_KEY);
}

#[tokio::test]
#[ignore]
async fn cached_tokens_are_charged_by_each_hosts_rules() {
    ensure_hosts().await;
    let (env, _, token, budget) = cloud_env("cached", &[QWEN, KIMI, GPT_OSS], "openai").await;
    // 400 uncached input at $1, 600 cache reads at $0.10, 20 output at $2.
    let each = 400 + 60 + 40;
    let cases = [
        (
            "Messages to a chat-only host",
            messages_request(&env, &token, QWEN, 20, 0, false),
        ),
        (
            "Messages passed through",
            messages_request(&env, &token, KIMI, 20, 0, false),
        ),
        (
            "Responses passed through",
            responses_request(&env, &token, GPT_OSS, 20, 0, false),
        ),
        (
            "Responses to a chat-only host",
            responses_request(&env, &token, QWEN, 20, 0, false),
        ),
    ];
    for (i, (what, request)) in cases.into_iter().enumerate() {
        ok(request.header("x-mock-cached", "600")).await;
        println!("{what}");
        wait_charged(&budget, (i as i64 + 1) * each).await;
    }
}

#[tokio::test]
#[ignore]
async fn a_cut_off_translated_stream_is_charged_an_estimate() {
    ensure_hosts().await;
    let (env, _, token, budget) = cloud_env("cut-translated", &[QWEN], "openai").await;
    let request =
        messages_request(&env, &token, QWEN, 20, 100, true).header("x-mock-cached", "600");
    let read = read_then_hang_up(request, 5).await;
    assert_eq!(read.len(), 5);
    // At least the five answer tokens the tool got; at most the whole answer
    // with its cache reads at their own price: never the cached tokens as
    // uncached input on top.
    assert_between(charged(&budget).await, 5 * 2, 400 + 60 + 40);
}

#[tokio::test]
#[ignore]
async fn the_gemini_route_reaches_gemini() {
    ensure_hosts().await;
    let (env, _, token, budget) = cloud_env("gemini", &[GEMINI], "openai").await;
    let before = mock_requests().await.len();
    let answer = ok(gemini_request(
        &env,
        &token,
        &format!("{GEMINI}:generateContent"),
        "",
        2,
    ))
    .await;
    assert_eq!(
        answer["candidates"][0]["content"]["parts"][0]["text"], "t0 t1 ",
        "{answer}"
    );
    wait_charged(&budget, cost(3, 15, 2)).await;
    let text = read_stream(gemini_request(
        &env,
        &token,
        &format!("{GEMINI}:streamGenerateContent"),
        "?alt=sse",
        2,
    ))
    .await;
    assert!(text.contains("t1 "), "{text}");
    wait_charged(&budget, 2 * cost(3, 15, 2)).await;
    let counted = ok(gemini_request(
        &env,
        &token,
        &format!("{GEMINI}:countTokens"),
        "",
        1,
    ))
    .await;
    assert_eq!(counted["totalTokens"], 1000, "{counted}");
    sleep(Duration::from_secs(6)).await;
    assert_eq!(
        micro_usd(&spent(&budget).await),
        2 * cost(3, 15, 2),
        "countTokens was charged"
    );

    let seen = requests_since(before).await;
    let paths: Vec<&str> = seen.iter().map(|r| r["path"].as_str().unwrap()).collect();
    assert_eq!(
        paths,
        [
            format!("/v1beta/models/{GEMINI}:generateContent"),
            format!("/v1beta/models/{GEMINI}:streamGenerateContent?alt=sse"),
            format!("/v1beta/models/{GEMINI}:countTokens"),
        ],
        "{seen:?}"
    );
    for r in &seen {
        assert_eq!(header(r, "x-goog-api-key"), GEMINI_KEY, "{r}");
        assert!(r["headers"].get("authorization").is_none(), "{r}");
    }
}

#[tokio::test]
#[ignore]
async fn claude_code_reaches_a_gemini_model() {
    ensure_hosts().await;
    let (env, _, token, budget) = cloud_env("claude-gemini", &[GEMINI], "openai").await;
    let before = mock_requests().await.len();
    let answer = ok(messages_request(&env, &token, GEMINI, 1, 0, false)).await;
    assert_eq!(answer["type"], "message", "{answer}");
    wait_charged(&budget, cost(3, 15, 1)).await;
    let r = the_request_since(before).await;
    assert_bearer(&r, "/v1beta/openai/chat/completions", GEMINI_KEY);
}

#[tokio::test]
#[ignore]
async fn codex_gets_a_clear_answer_for_claude() {
    ensure_hosts().await;
    let (env, _, token, _) = cloud_env("codex-claude", &[CLAUDE_MODEL], "openai").await;
    let before = mock_requests().await.len();
    let (status, body, text) =
        send(responses_request(&env, &token, CLAUDE_MODEL, 1, 0, false)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert_eq!(body["error"], "api_not_served", "{text}");
    assert_eq!(
        body["message"],
        format!(
            "{CLAUDE_MODEL} can't be reached through the Responses API; \
             use the Anthropic Messages API or the Chat Completions API"
        )
    );
    assert!(requests_since(before).await.is_empty());
}

#[tokio::test]
#[ignore]
async fn an_unlisted_model_is_refused_by_sekisho() {
    ensure_hosts().await;
    let (env_a, _, token, _) = cloud_env("unlisted-a", &[QWEN], "openai").await;
    cloud_env("unlisted-b", &[MISTRAL], "openai").await;
    let before = mock_requests().await.len();
    // env-b's model, and a name Sekisho has nowhere, of the same length.
    let (s1, _, elsewhere) = send(chat(&env_a, &token, MISTRAL, 1)).await;
    let (s2, body, nowhere) = send(chat(&env_a, &token, "mistral-moxk", 1)).await;
    assert_eq!((s1, s2), (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND));
    assert_eq!(body["error"], "model_not_found", "{nowhere}");
    assert_eq!(elsewhere.replace(MISTRAL, "mistral-moxk"), nowhere);
    assert!(requests_since(before).await.is_empty());
}

#[tokio::test]
#[ignore]
async fn spoofed_headers_cannot_cross_environments() {
    ensure_hosts().await;
    let (env_a, _, token_a, budget_a) = cloud_env("spoof-a", &[QWEN], "openai").await;
    let (env_b, _, _, budget_b) = cloud_env("spoof-b", &[MISTRAL], "openai").await;
    let before = mock_requests().await.len();
    let spoofed = |model: &str| {
        chat(&env_a, &token_a, model, 1)
            .header("x-sekisho-env", &env_b)
            .header("x-sekisho-model", MISTRAL)
    };
    let (status, body, text) = send(spoofed(MISTRAL)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{text}");
    assert_eq!(body["error"], "model_not_found", "{text}");
    assert!(requests_since(before).await.is_empty());

    // env-a's own model with the same headers: env-a's provider and budget.
    ok(spoofed(QWEN)).await;
    wait_charged(&budget_a, cost(1, 2, 1)).await;
    let r = the_request_since(before).await;
    assert_bearer(&r, "/v1/chat/completions", CHAT_HOST_KEY);
    // The environment header the gateway sets is removed before the provider.
    // A client's own x-sekisho-model passes on like any other header it sends.
    assert!(r["headers"].get("x-sekisho-env").is_none(), "{r}");
    sleep(Duration::from_secs(2)).await;
    assert_eq!(micro_usd(&spent(&budget_b).await), 0);
}

#[tokio::test]
#[ignore]
async fn a_query_never_carries_a_token_to_a_provider() {
    ensure_hosts().await;
    let (env, _, token, _) = cloud_env("query", &[GEMINI, QWEN], "openai").await;
    let before = mock_requests().await.len();
    let gemini_body = json!({ "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }] });
    let generate = format!("/env/{env}/gemini/v1beta/models/{GEMINI}:generateContent");
    let chat_path = format!("/env/{env}/openai/v1/chat/completions");
    for (path, body) in [
        (format!("{generate}?key={token}"), &gemini_body),
        (format!("{generate}?%6Bey={token}"), &gemini_body),
        (format!("{generate}?alt=sse&key={token}"), &gemini_body),
        (format!("{generate}?foo=1"), &gemini_body),
        (format!("{chat_path}?key={token}"), &chat_body(QWEN)),
        (format!("{chat_path}?foo=1"), &chat_body(QWEN)),
    ] {
        let status = raw_status("POST", &path, &token, body).await;
        assert_eq!(status, 403, "{}", path.replace(&token, "<token>"));
    }
    assert!(requests_since(before).await.is_empty());

    // The two queries coding tools send are served.
    let messages = json!({
        "model": QWEN, "max_tokens": 16, "messages": [{ "role": "user", "content": "hi" }]
    });
    let served = [
        (
            format!("/env/{env}/anthropic/v1/messages?beta=true"),
            &messages,
        ),
        (
            format!("/env/{env}/gemini/v1beta/models/{GEMINI}:streamGenerateContent?alt=sse"),
            &gemini_body,
        ),
    ];
    for (path, body) in served {
        assert_eq!(raw_status("POST", &path, &token, body).await, 200, "{path}");
    }
    assert_eq!(requests_since(before).await.len(), 2);
}

#[tokio::test]
#[ignore]
async fn count_tokens_is_never_charged() {
    ensure_hosts().await;
    let (env, _, token, budget) = cloud_env("count", &[CLAUDE_MODEL, QWEN], "openai").await;
    let before = mock_requests().await.len();
    for model in [CLAUDE_MODEL, QWEN] {
        let request = http()
            .post(format!(
                "{GATEWAY}/env/{env}/anthropic/v1/messages/count_tokens"
            ))
            .bearer_auth(&token)
            .header("x-api-key", &token)
            .header("anthropic-version", "2023-06-01")
            .json(&json!({ "model": model, "messages": [{ "role": "user", "content": "hi" }] }));
        let answer = ok(request).await;
        assert!(
            answer["input_tokens"].as_u64().is_some(),
            "{model}: {answer}"
        );
    }
    sleep(Duration::from_secs(6)).await;
    assert_eq!(
        micro_usd(&spent(&budget).await),
        0,
        "count_tokens was charged"
    );
    // Anthropic counts its own; agentgateway counts for the chat-only host.
    let seen = requests_since(before).await;
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0]["path"], "/v1/messages/count_tokens", "{}", seen[0]);
    assert_eq!(header(&seen[0], "x-api-key"), ANTHROPIC_KEY, "{}", seen[0]);
}

#[tokio::test]
#[ignore]
async fn one_bedrock_provider_serves_every_tool() {
    ensure_hosts().await;
    let (env, _, token, budget) = cloud_env("bedrock-all", &[BEDROCK_CLAUDE], "openai").await;
    let before = mock_requests().await.len();
    let answer = ok(messages_request(&env, &token, BEDROCK_CLAUDE, 1, 0, false)).await;
    assert_eq!(answer["type"], "message", "{answer}");
    let answer = ok(responses_request(&env, &token, BEDROCK_CLAUDE, 1, 0, false)).await;
    assert_eq!(answer["object"], "response", "{answer}");
    ok(chat(&env, &token, BEDROCK_CLAUDE, 1)).await;
    wait_charged(&budget, 3 * cost(3, 15, 1)).await;

    let seen = requests_since(before).await;
    assert_eq!(seen.len(), 3, "{seen:?}");
    for r in &seen {
        assert_eq!(
            r["path"],
            format!("/model/{BEDROCK_CLAUDE}/converse"),
            "{r}"
        );
        let auth = header(r, "authorization");
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 ")
                && auth.contains(&format!("Credential={BEDROCK_ACCESS_KEY}/")),
            "{r}"
        );
    }
}

#[tokio::test]
#[ignore]
async fn one_azure_provider_serves_gpt_through_both_openai_apis() {
    ensure_hosts().await;
    let (env, _, token, budget) = cloud_env("azure-both", &[AZURE_GPT], "openai").await;
    let before = mock_requests().await.len();
    ok(chat(&env, &token, AZURE_GPT, 1)).await;
    let answer = ok(responses_request(&env, &token, AZURE_GPT, 1, 0, false)).await;
    assert_eq!(answer["object"], "response", "{answer}");
    wait_charged(&budget, 2 * cost(3, 15, 1)).await;

    let seen = requests_since(before).await;
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_bearer(&seen[0], "/openai/v1/chat/completions", AZURE_KEY);
    assert_bearer(&seen[1], "/openai/v1/responses", AZURE_KEY);
}

#[tokio::test]
#[ignore]
async fn a_preset_hosts_model_is_charged_the_catalog_price() {
    ensure_hosts().await;
    let (env, _, token, budget) = cloud_env("preset", &[TOGETHER_MODEL], "openai").await;
    let before = mock_requests().await.len();
    ok(chat(&env, &token, TOGETHER_MODEL, 20)).await;
    // agentgateway's catalog: $0.50 input, $3 output per million.
    wait_charged(&budget, 1_000 / 2 + 20 * 3).await;
    let r = the_request_since(before).await;
    assert_bearer(&r, "/v1/chat/completions", TOGETHER_KEY);
}
