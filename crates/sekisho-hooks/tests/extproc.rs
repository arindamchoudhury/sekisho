use std::sync::{Arc, Mutex};
use std::time::Duration;

use sekisho_core::keys::EnvKeyId;
use sekisho_core::names::EnvName;
use sekisho_hooks::cache::AuthCache;
use sekisho_hooks::extproc::StreamGuard;
use sekisho_proto::common::HeaderValue;
use sekisho_proto::ext_proc::body_mutation::Mutation;
use sekisho_proto::ext_proc::external_processor_client::ExternalProcessorClient;
use sekisho_proto::ext_proc::external_processor_server::ExternalProcessorServer;
use sekisho_proto::ext_proc::processing_request::Request;
use sekisho_proto::ext_proc::processing_response::Response;
use sekisho_proto::ext_proc::{
    HeaderMap, HttpBody, HttpHeaders, HttpTrailers, ProcessingRequest, ProcessingResponse,
};
use sekisho_store::EnvironmentSpec;
use sekisho_testkit::TestStore;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep, timeout};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Streaming;
use tonic::transport::Channel;

struct Rig {
    client: ExternalProcessorClient<Channel>,
    store: TestStore,
    cache: Arc<AuthCache>,
    /// Moves the guard's clock.
    now: Arc<Mutex<OffsetDateTime>>,
    token: String,
    key: EnvKeyId,
}

async fn rig() -> Rig {
    let (store, _) = sekisho_testkit::store().await;
    let spec = EnvironmentSpec {
        name: EnvName::parse("data-team").unwrap(),
        description: String::new(),
        notice: String::new(),
        access_groups: Vec::new(),
        models: Vec::new(),
    };
    let env = store.upsert_environment(spec).await.unwrap().spec.name;
    let expires = OffsetDateTime::now_utc() + time::Duration::days(1);
    let (key, token) = store
        .create_key(&env, "alice@example.com", "ci", Some(expires))
        .await
        .unwrap();
    let cache = AuthCache::load(&store).await.unwrap();
    cache.spawn_refresh(store.clone());
    let now = Arc::new(Mutex::new(OffsetDateTime::now_utc()));
    let clock_now = Arc::clone(&now);
    let guard = StreamGuard {
        cache: Arc::clone(&cache),
        clock: Arc::new(move || *clock_now.lock().unwrap()),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(ExternalProcessorServer::new(guard))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    let client = ExternalProcessorClient::connect(format!("http://{addr}"))
        .await
        .unwrap();
    Rig {
        client,
        store,
        cache,
        now,
        token,
        key: key.id,
    }
}

/// Request headers as agentgateway sends them: ordinary headers as raw bytes.
fn request_headers(authorization: Option<&str>, raw: bool) -> ProcessingRequest {
    let mut headers = vec![HeaderValue {
        key: ":path".into(),
        value: "/v1/chat/completions".into(),
        raw_value: vec![],
    }];
    if let Some(value) = authorization {
        headers.push(if raw {
            HeaderValue {
                key: "authorization".into(),
                value: String::new(),
                raw_value: value.as_bytes().to_vec(),
            }
        } else {
            HeaderValue {
                key: "authorization".into(),
                value: value.into(),
                raw_value: vec![],
            }
        });
    }
    message(Request::RequestHeaders(HttpHeaders {
        headers: Some(HeaderMap { headers }),
        end_of_stream: true,
    }))
}

fn message(request: Request) -> ProcessingRequest {
    ProcessingRequest {
        request: Some(request),
        ..Default::default()
    }
}

type Tx = mpsc::Sender<ProcessingRequest>;

/// Opens a processing stream and sends the request headers.
async fn open_stream(
    rig: &Rig,
    authorization: Option<&str>,
    raw: bool,
) -> (Tx, Streaming<ProcessingResponse>) {
    let (tx, rx) = mpsc::channel(8);
    tx.send(request_headers(authorization, raw)).await.unwrap();
    let out = rig
        .client
        .clone()
        .process(ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    (tx, out)
}

async fn send_body_chunk(tx: &Tx, chunk: &[u8]) {
    tx.send(message(Request::ResponseBody(HttpBody {
        body: chunk.to_vec(),
        end_of_stream: false,
    })))
    .await
    .unwrap();
}

async fn next(out: &mut Streaming<ProcessingResponse>) -> Option<ProcessingResponse> {
    timeout(Duration::from_secs(2), out.next())
        .await
        .expect("an answer within 2 s")
        .map(|r| r.unwrap())
}

trait Answer {
    fn body(&self) -> Vec<u8>;
    fn immediate_status(&self) -> i32;
    fn immediate_body(&self) -> serde_json::Value;
}

impl Answer for ProcessingResponse {
    /// The echoed chunk of a streamed response-body mutation.
    fn body(&self) -> Vec<u8> {
        let Some(Response::ResponseBody(body)) = &self.response else {
            panic!("not a body answer: {self:?}")
        };
        match body
            .response
            .as_ref()
            .and_then(|c| c.body_mutation.as_ref())
            .and_then(|m| m.mutation.as_ref())
        {
            Some(Mutation::StreamedResponse(streamed)) => streamed.body.clone(),
            other => panic!("not a streamed_response mutation: {other:?}"),
        }
    }

    fn immediate_status(&self) -> i32 {
        let Some(Response::ImmediateResponse(immediate)) = &self.response else {
            panic!("not immediate: {self:?}")
        };
        immediate.status.as_ref().unwrap().code
    }

    fn immediate_body(&self) -> serde_json::Value {
        let Some(Response::ImmediateResponse(immediate)) = &self.response else {
            panic!("not immediate: {self:?}")
        };
        serde_json::from_str(&immediate.body).unwrap()
    }
}

async fn wait_cache(rig: &Rig, what: &str, done: impl Fn(&AuthCache) -> bool) {
    let start = Instant::now();
    while !done(&rig.cache) {
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "{what} not cached within 1 s"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn stream_cut_after_revocation() {
    let rig = rig().await;
    let token = format!("Bearer {}", rig.token);
    let (tx, mut out) = open_stream(&rig, Some(&token), true).await;
    assert!(matches!(
        next(&mut out).await.unwrap().response,
        Some(Response::RequestHeaders(_))
    ));
    send_body_chunk(&tx, b"data: a\n\n").await;
    assert_eq!(next(&mut out).await.unwrap().body(), b"data: a\n\n");
    rig.store.revoke_key(&rig.key).await.unwrap();
    wait_cache(&rig, "revocation", |c| c.key(&rig.key).unwrap().revoked).await;
    send_body_chunk(&tx, b"data: b\n\n").await;
    let cut = next(&mut out).await.unwrap();
    assert_eq!(cut.immediate_status(), 403);
    assert_eq!(
        cut.immediate_body(),
        serde_json::json!({ "error": "forbidden", "reason": "revoked" })
    );
    assert!(next(&mut out).await.is_none(), "the guard ends the stream");
}

#[tokio::test]
async fn every_phase_is_answered_and_chunks_are_echoed_in_order() {
    let rig = rig().await;
    let token = format!("Bearer {}", rig.token);
    let (tx, mut out) = open_stream(&rig, Some(&token), false).await; // value instead of raw_value works too
    assert!(matches!(
        next(&mut out).await.unwrap().response,
        Some(Response::RequestHeaders(_))
    ));
    tx.send(message(Request::ResponseHeaders(HttpHeaders {
        headers: None,
        end_of_stream: false,
    })))
    .await
    .unwrap();
    assert!(matches!(
        next(&mut out).await.unwrap().response,
        Some(Response::ResponseHeaders(_))
    ));
    for chunk in [&b"data: 1\n\n"[..], b"data: 2\n\n", b""] {
        send_body_chunk(&tx, chunk).await;
        assert_eq!(next(&mut out).await.unwrap().body(), chunk);
    }
    tx.send(message(Request::ResponseBody(HttpBody {
        body: b"data: [DONE]\n\n".to_vec(),
        end_of_stream: true,
    })))
    .await
    .unwrap();
    let last = next(&mut out).await.unwrap();
    let Some(Response::ResponseBody(body)) = &last.response else {
        panic!()
    };
    let Some(Mutation::StreamedResponse(streamed)) = body
        .response
        .as_ref()
        .unwrap()
        .body_mutation
        .as_ref()
        .unwrap()
        .mutation
        .as_ref()
    else {
        panic!()
    };
    assert!(streamed.end_of_stream);
    assert_eq!(streamed.body, b"data: [DONE]\n\n");
    tx.send(message(Request::ResponseTrailers(HttpTrailers {
        trailers: None,
    })))
    .await
    .unwrap();
    assert!(matches!(
        next(&mut out).await.unwrap().response,
        Some(Response::ResponseTrailers(_))
    ));
    drop(tx);
    assert!(next(&mut out).await.is_none());
}

#[tokio::test]
async fn suspended_and_expired_keys_are_cut_with_their_reason() {
    let rig = rig().await;
    let token = format!("Bearer {}", rig.token);
    let (tx, mut out) = open_stream(&rig, Some(&token), true).await;
    next(&mut out).await.unwrap();
    rig.store.set_suspended(&rig.key, true).await.unwrap();
    wait_cache(&rig, "suspension", |c| c.key(&rig.key).unwrap().suspended).await;
    send_body_chunk(&tx, b"x").await;
    assert_eq!(
        next(&mut out).await.unwrap().immediate_body()["reason"],
        "suspended"
    );

    rig.store.set_suspended(&rig.key, false).await.unwrap();
    wait_cache(&rig, "resumption", |c| !c.key(&rig.key).unwrap().suspended).await;
    let (tx, mut out) = open_stream(&rig, Some(&token), true).await;
    next(&mut out).await.unwrap();
    *rig.now.lock().unwrap() = OffsetDateTime::now_utc() + time::Duration::days(2); // past the key's expiry
    send_body_chunk(&tx, b"x").await;
    assert_eq!(
        next(&mut out).await.unwrap().immediate_body()["reason"],
        "expired"
    );
}

#[tokio::test]
async fn unknown_or_missing_keys_are_refused_at_headers() {
    let rig = rig().await;
    let unknown = format!("Bearer sek_aaaaaaaaaaaa_{}", "A".repeat(43));
    let (id, _) = sekisho_core::keys::parse_token(&rig.token).unwrap();
    let wrong_secret = format!("Bearer sek_{id}_{}", "A".repeat(43));
    for authorization in [
        None,
        Some("Bearer garbage"),
        Some(unknown.as_str()),
        Some(wrong_secret.as_str()),
    ] {
        let (_tx, mut out) = open_stream(&rig, authorization, true).await;
        let refused = next(&mut out).await.unwrap();
        assert_eq!(refused.immediate_status(), 403, "{authorization:?}");
        assert_eq!(
            refused.immediate_body()["reason"],
            "unknown_key",
            "{authorization:?}"
        );
        assert!(next(&mut out).await.is_none());
    }
}
