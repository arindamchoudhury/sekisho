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
//! `POST /v1/messages` answers like Anthropic's Messages API (not streamed),
//! with the same usage.
//!
//! Azure's `v1` path, `POST /openai/v1/chat/completions`, answers like
//! `/v1/chat/completions`.
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
        .route("/openai/v1/chat/completions", post(completions))
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

async fn messages(headers: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
    let chunks = header_number(&headers, "x-mock-chunks", 20);
    let text: String = (0..chunks).map(|i| format!("t{i} ")).collect();
    Json(json!({
        "id": "msg_mock",
        "type": "message",
        "role": "assistant",
        "model": body["model"].as_str().unwrap_or("mock-model"),
        "content": [{ "type": "text", "text": text }],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": { "input_tokens": 1000, "output_tokens": chunks },
    }))
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
    let usage = json!({
        "prompt_tokens": 1000,
        "completion_tokens": chunks,
        "total_tokens": 1000 + chunks,
    });
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
