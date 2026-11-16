//! `sekisho-mock-model`: an OpenAI-compatible model for the end-to-end tests.
//!
//! `POST /v1/chat/completions` answers with `x-mock-chunks` chunks (default
//! 20), one every `x-mock-interval-ms` (default 1000), and always reports
//! usage: 1000 prompt tokens and one completion token per chunk. With
//! `"stream": true` it streams server-sent events ending in `[DONE]`;
//! otherwise it answers one JSON body with the same usage. The answer names
//! the requested model, or `x-mock-model` when given (as providers report a
//! dated model name).
//!
//! `POST /v1/messages` answers like Anthropic's Messages API, with the same
//! chunks, interval and usage. With `"stream": true` it streams Anthropic's
//! events: `message_start` (with the prompt's usage), one
//! `content_block_delta` per chunk, then `message_delta` (with the output
//! count) and `message_stop`. `x-mock-error-after: n` ends the stream after
//! `n` deltas with an `overloaded_error` event, as a provider that fails
//! mid-answer does.
//!
//! `POST /anthropic/v1/messages` (a host serving Messages under its own
//! path) answers like `/v1/messages`, and `POST /v1/messages/count_tokens`
//! answers `{"input_tokens": 1000}`.
//!
//! `POST /v1/responses` answers like OpenAI's Responses API, with the same
//! chunks, interval and usage: one JSON body, or with `"stream": true`
//! `response.created`, one `response.output_text.delta` per chunk, then
//! `response.completed` with the usage.
//!
//! Gemini: `POST /v1beta/models/{model}:{action}` answers
//! `generateContent` (one JSON body), `streamGenerateContent` (server-sent
//! events with `alt=sse`, `usageMetadata` in each chunk and `finishReason`
//! on the last) and `countTokens` (`{"totalTokens": 1000}`). Gemini's
//! OpenAI-compatible endpoint, `POST /v1beta/openai/chat/completions`,
//! answers like `/v1/chat/completions`.
//!
//! `x-mock-cached: n` makes `n` of the 1000 prompt tokens cache reads, in
//! each format's own convention: Chat Completions
//! `prompt_tokens_details.cached_tokens` within `prompt_tokens`; Messages
//! `cache_read_input_tokens` beside `input_tokens = 1000 - n`; Responses
//! `input_tokens_details.cached_tokens` within `input_tokens`; Gemini
//! `cachedContentTokenCount` within `promptTokenCount`.
//!
//! Azure's `v1` paths, `POST /openai/v1/chat/completions` and
//! `POST /openai/v1/responses`, answer like `/v1/chat/completions` and
//! `/v1/responses`.
//!
//! Bedrock's Converse API: `POST /model/{model}/converse` answers one JSON
//! body, and `POST /model/{model}/converse-stream` streams AWS event-stream
//! messages (`messageStart`, one `contentBlockDelta` per chunk,
//! `contentBlockStop`, `messageStop`, then `metadata` with the usage), with
//! the same chunks, interval and usage.
//!
//! Every other path answers 200 `{"mock": "unrouted"}`, so a request that
//! should never have reached the provider is visible. Every request except
//! `/_…` and `/healthz` is recorded: `GET /_requests` lists them (method, raw
//! path and headers) and `GET /_last_headers` returns the last one's headers,
//! so a test can check what reached the provider. Listens on `MOCK_LISTEN`
//! (default `0.0.0.0:8000`) with `TCP_NODELAY` on.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::serve::ListenerExt;
use axum::{Json, Router};
use sekisho_testkit::eventstream;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Every request received, oldest first.
type Seen = Arc<Mutex<Vec<Value>>>;

#[tokio::main]
async fn main() {
    let addr = std::env::var("MOCK_LISTEN").unwrap_or_else(|_| "0.0.0.0:8000".into());
    let seen: Seen = Arc::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(completions))
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/anthropic/v1/messages", post(messages))
        .route("/v1/responses", post(responses))
        .route("/v1beta/models/{call}", post(gemini))
        .route("/v1beta/openai/chat/completions", post(completions))
        .route("/openai/v1/chat/completions", post(completions))
        .route("/openai/v1/responses", post(responses))
        .route("/model/{model}/converse", post(converse))
        .route("/model/{model}/converse-stream", post(converse_stream))
        .route("/_requests", get(requests))
        .route("/_last_headers", get(last_headers))
        .route("/healthz", get(|| async { "ok" }))
        .fallback(unrouted)
        .layer(middleware::from_fn_with_state(seen.clone(), record))
        .with_state(seen);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("cannot listen on {addr}: {e}"))
        .tap_io(|tcp| {
            // Without it, small SSE writes wait for ACKs (the spike's 40 ms).
            let _ = tcp.set_nodelay(true);
        });
    println!("sekisho-mock-model listening on {addr}");
    axum::serve(listener, app).await.expect("serve");
}

fn header_number(headers: &HeaderMap, name: &str, default: u64) -> u64 {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

async fn record(State(seen): State<Seen>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if !path.starts_with("/_") && path != "/healthz" {
        let headers: BTreeMap<String, String> = req
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let path_and_query = req.uri().path_and_query().map_or(path, |p| p.as_str());
        seen.lock().unwrap().push(json!({
            "method": req.method().as_str(),
            "path": path_and_query,
            "headers": headers,
        }));
    }
    next.run(req).await
}

async fn requests(State(seen): State<Seen>) -> Json<Vec<Value>> {
    Json(seen.lock().unwrap().clone())
}

async fn last_headers(State(seen): State<Seen>) -> Json<Value> {
    let seen = seen.lock().unwrap();
    Json(seen.last().map_or(json!({}), |r| r["headers"].clone()))
}

async fn unrouted() -> Json<Value> {
    Json(json!({ "mock": "unrouted" }))
}

/// The prompt's tokens that are cache reads (`x-mock-cached`), at most all
/// 1000.
fn cached(headers: &HeaderMap) -> u64 {
    header_number(headers, "x-mock-cached", 0).min(PROMPT)
}

/// The prompt's tokens in every answer.
const PROMPT: u64 = 1000;

/// A server-sent event stream fed by a task.
fn sse(rx: mpsc::Receiver<Result<Bytes, Infallible>>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response()
}

async fn count_tokens() -> Json<Value> {
    Json(json!({ "input_tokens": PROMPT }))
}

/// Responses' usage for the prompt, `cached` of it read from the cache, and
/// one output token per chunk.
fn responses_usage(cached: u64, chunks: u64) -> Value {
    json!({
        "input_tokens": PROMPT,
        "input_tokens_details": { "cached_tokens": cached },
        "output_tokens": chunks,
        "output_tokens_details": { "reasoning_tokens": 0 },
        "total_tokens": PROMPT + chunks,
    })
}

/// A Responses API `response` object.
fn response_object(model: &str, status: &str, text: Option<&str>, usage: Value) -> Value {
    let output = match text {
        Some(text) => json!([{
            "type": "message", "id": "msg_mock", "status": "completed", "role": "assistant",
            "content": [{ "type": "output_text", "text": text, "annotations": [] }],
        }]),
        None => json!([]),
    };
    json!({
        "id": "resp_mock", "object": "response", "created_at": 0, "status": status,
        "model": model, "output": output, "usage": usage,
        "error": null, "incomplete_details": null,
    })
}

async fn responses(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let chunks = header_number(&headers, "x-mock-chunks", 20);
    let cached = cached(&headers);
    let model = body["model"].as_str().unwrap_or("mock-model").to_owned();
    if body["stream"].as_bool() != Some(true) {
        let text: String = (0..chunks).map(|i| format!("t{i} ")).collect();
        let usage = responses_usage(cached, chunks);
        return Json(response_object(&model, "completed", Some(&text), usage)).into_response();
    }

    let interval = Duration::from_millis(header_number(&headers, "x-mock-interval-ms", 1000));
    let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(4);
    tokio::spawn(async move {
        let mut sequence = 0u64;
        let mut event = |name: &str, mut value: Value| {
            value["type"] = json!(name);
            value["sequence_number"] = json!(sequence);
            sequence += 1;
            Ok(Bytes::from(format!("event: {name}\ndata: {value}\n\n")))
        };
        let created = response_object(&model, "in_progress", None, Value::Null);
        if tx
            .send(event("response.created", json!({ "response": created })))
            .await
            .is_err()
        {
            return;
        }
        let mut text = String::new();
        for i in 0..chunks {
            tokio::time::sleep(interval).await;
            let delta = format!("t{i} ");
            text.push_str(&delta);
            let value = json!({
                "item_id": "msg_mock", "output_index": 0, "content_index": 0,
                "delta": delta, "logprobs": [],
            });
            if tx
                .send(event("response.output_text.delta", value))
                .await
                .is_err()
            {
                return; // the client went away
            }
        }
        let usage = responses_usage(cached, chunks);
        let done = response_object(&model, "completed", Some(&text), usage);
        let _ = tx
            .send(event("response.completed", json!({ "response": done })))
            .await;
    });
    sse(rx)
}

/// Gemini's usage for the prompt, `cached` of it from the cache, and
/// `output` answer tokens.
fn gemini_usage(cached: u64, output: u64) -> Value {
    let mut usage = json!({
        "promptTokenCount": PROMPT,
        "candidatesTokenCount": output,
        "totalTokenCount": PROMPT + output,
    });
    if cached > 0 {
        usage["cachedContentTokenCount"] = json!(cached);
    }
    usage
}

/// One Gemini answer chunk: `text`, and `finishReason` when `last`.
fn gemini_chunk(model: &str, text: &str, last: bool, usage: Value) -> Value {
    let mut candidate = json!({
        "content": { "role": "model", "parts": [{ "text": text }] },
        "index": 0,
    });
    if last {
        candidate["finishReason"] = json!("STOP");
    }
    json!({ "candidates": [candidate], "usageMetadata": usage, "modelVersion": model })
}

/// `{model}:{action}`: `generateContent`, `streamGenerateContent` or
/// `countTokens`.
async fn gemini(
    axum::extract::Path(call): axum::extract::Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some((model, action)) = call.split_once(':') else {
        return unrouted().await.into_response();
    };
    let model = model.to_owned();
    let chunks = header_number(&headers, "x-mock-chunks", 20);
    let cached = cached(&headers);
    match action {
        "countTokens" => Json(json!({ "totalTokens": PROMPT })).into_response(),
        "generateContent" => {
            let text: String = (0..chunks).map(|i| format!("t{i} ")).collect();
            Json(gemini_chunk(
                &model,
                &text,
                true,
                gemini_usage(cached, chunks),
            ))
            .into_response()
        }
        "streamGenerateContent" => {
            let interval =
                Duration::from_millis(header_number(&headers, "x-mock-interval-ms", 1000));
            let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(4);
            tokio::spawn(async move {
                for i in 0..chunks {
                    tokio::time::sleep(interval).await;
                    let last = i + 1 == chunks;
                    let chunk =
                        gemini_chunk(&model, &format!("t{i} "), last, gemini_usage(cached, i + 1));
                    if tx
                        .send(Ok(Bytes::from(format!("data: {chunk}\r\n\r\n"))))
                        .await
                        .is_err()
                    {
                        return; // the client went away
                    }
                }
            });
            sse(rx)
        }
        _ => unrouted().await.into_response(),
    }
}

async fn messages(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let chunks = header_number(&headers, "x-mock-chunks", 20);
    let cached = cached(&headers);
    let model = body["model"].as_str().unwrap_or("mock-model").to_owned();
    if body["stream"].as_bool() != Some(true) {
        let text: String = (0..chunks).map(|i| format!("t{i} ")).collect();
        return Json(json!({
            "id": "msg_mock",
            "type": "message",
            "role": "assistant",
            "model": model,
            "content": [{ "type": "text", "text": text }],
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {
                "input_tokens": PROMPT - cached, "cache_read_input_tokens": cached,
                "output_tokens": chunks,
            },
        }))
        .into_response();
    }

    let interval = Duration::from_millis(header_number(&headers, "x-mock-interval-ms", 1000));
    let error_after = headers
        .get("x-mock-error-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(4);
    tokio::spawn(async move {
        let event =
            |name: &str, value: Value| Ok(Bytes::from(format!("event: {name}\ndata: {value}\n\n")));
        let start = json!({
            "type": "message_start",
            "message": {
                "id": "msg_mock", "type": "message", "role": "assistant", "model": model,
                "content": [], "stop_reason": null, "stop_sequence": null,
                "usage": {
                    "input_tokens": PROMPT - cached, "cache_read_input_tokens": cached,
                    "output_tokens": 1,
                },
            },
        });
        let block = json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } });
        for (name, value) in [("message_start", start), ("content_block_start", block)] {
            if tx.send(event(name, value)).await.is_err() {
                return;
            }
        }
        for i in 0..chunks {
            tokio::time::sleep(interval).await;
            let delta = json!({
                "type": "content_block_delta", "index": 0,
                "delta": { "type": "text_delta", "text": format!("t{i} ") },
            });
            if tx.send(event("content_block_delta", delta)).await.is_err() {
                return; // the client went away
            }
            if error_after == Some(i + 1) {
                let error = json!({ "type": "error", "error": { "type": "overloaded_error", "message": "Overloaded" } });
                let _ = tx.send(event("error", error)).await;
                return;
            }
        }
        for (name, value) in [
            (
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": 0 }),
            ),
            (
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn", "stop_sequence": null },
                    "usage": { "output_tokens": chunks },
                }),
            ),
            ("message_stop", json!({ "type": "message_stop" })),
        ] {
            if tx.send(event(name, value)).await.is_err() {
                return;
            }
        }
    });
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response()
}

async fn completions(headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let chunks = header_number(&headers, "x-mock-chunks", 20);
    let interval = Duration::from_millis(header_number(&headers, "x-mock-interval-ms", 1000));
    let model = headers
        .get("x-mock-model")
        .and_then(|v| v.to_str().ok())
        .or(body["model"].as_str())
        .unwrap_or("mock-model")
        .to_owned();
    let mut usage = json!({
        "prompt_tokens": PROMPT,
        "completion_tokens": chunks,
        "total_tokens": PROMPT + chunks,
    });
    let cached = cached(&headers);
    if cached > 0 {
        usage["prompt_tokens_details"] = json!({ "cached_tokens": cached });
    }
    if body["stream"].as_bool() != Some(true) {
        let text: String = (0..chunks).map(|i| format!("t{i} ")).collect();
        return Json(json!({
            "id": "mock",
            "object": "chat.completion",
            "created": 0,
            "model": model,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": text },
                "finish_reason": "stop",
            }],
            "usage": usage,
        }))
        .into_response();
    }

    let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(4);
    tokio::spawn(async move {
        let event = |value: Value| Ok(Bytes::from(format!("data: {value}\n\n")));
        let chunk = |delta: Value, finish: Value| {
            json!({
                "id": "mock",
                "object": "chat.completion.chunk",
                "created": 0,
                "model": model,
                "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
            })
        };
        for i in 0..chunks {
            tokio::time::sleep(interval).await;
            if tx
                .send(event(chunk(
                    json!({ "content": format!("t{i} ") }),
                    Value::Null,
                )))
                .await
                .is_err()
            {
                return; // the client went away
            }
        }
        let _ = tx.send(event(chunk(json!({}), json!("stop")))).await;
        let mut with_usage = chunk(json!({}), Value::Null);
        with_usage["choices"] = json!([]);
        with_usage["usage"] = usage;
        let _ = tx.send(event(with_usage)).await;
        let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
    });
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response()
}

/// Converse's usage for 1000 input tokens and one output token per chunk.
fn converse_usage(chunks: u64) -> Value {
    json!({ "inputTokens": 1000, "outputTokens": chunks, "totalTokens": 1000 + chunks })
}

async fn converse(headers: HeaderMap) -> Json<Value> {
    let chunks = header_number(&headers, "x-mock-chunks", 20);
    let text: String = (0..chunks).map(|i| format!("t{i} ")).collect();
    Json(json!({
        "output": { "message": { "role": "assistant", "content": [{ "text": text }] } },
        "stopReason": "end_turn",
        "usage": converse_usage(chunks),
        "metrics": { "latencyMs": 1 },
    }))
}

async fn converse_stream(headers: HeaderMap) -> Response {
    let chunks = header_number(&headers, "x-mock-chunks", 20);
    let interval = Duration::from_millis(header_number(&headers, "x-mock-interval-ms", 1000));
    let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(4);
    tokio::spawn(async move {
        let event = |kind: &str, value: Value| {
            Ok(Bytes::from(eventstream::frame(
                kind,
                value.to_string().as_bytes(),
            )))
        };
        if tx
            .send(event("messageStart", json!({ "role": "assistant" })))
            .await
            .is_err()
        {
            return;
        }
        for i in 0..chunks {
            tokio::time::sleep(interval).await;
            let delta = json!({ "contentBlockIndex": 0, "delta": { "text": format!("t{i} ") } });
            if tx.send(event("contentBlockDelta", delta)).await.is_err() {
                return; // the client went away
            }
        }
        for (kind, value) in [
            ("contentBlockStop", json!({ "contentBlockIndex": 0 })),
            ("messageStop", json!({ "stopReason": "end_turn" })),
            (
                "metadata",
                json!({ "usage": converse_usage(chunks), "metrics": { "latencyMs": 1 } }),
            ),
        ] {
            if tx.send(event(kind, value)).await.is_err() {
                return;
            }
        }
    });
    (
        [(header::CONTENT_TYPE, "application/vnd.amazon.eventstream")],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response()
}
