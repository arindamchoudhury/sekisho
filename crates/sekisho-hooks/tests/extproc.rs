use std::sync::{Arc, Mutex};
use std::time::Duration;

use prost_types::value::Kind;
use sekisho_core::keys::EnvKeyId;
use sekisho_core::names::EnvName;
use sekisho_core::pricing::Usage;
use sekisho_core::secret_box::SecretBox;
use sekisho_hooks::cache::AuthCache;
use sekisho_hooks::extproc::StreamGuard;
use sekisho_hooks::tally::{ProviderUsage, Tallies, Tally};
use sekisho_hooks::tokens::count_text;
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
use sekisho_testkit::people::session_token;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep, timeout};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Streaming;
use tonic::transport::Channel;
use uuid::Uuid;

const ISSUER: &str = "https://sekisho.test";

struct Rig {
    client: ExternalProcessorClient<Channel>,
    store: TestStore,
    secret_box: SecretBox,
    cache: Arc<AuthCache>,
    /// The owner of `token`.
    alice: Uuid,
    /// Moves the guard's clock.
    now: Arc<Mutex<OffsetDateTime>>,
    token: String,
    key: EnvKeyId,
    /// What the guard tallied for each request.
    tallies: Tallies,
}

async fn rig() -> Rig {
    let (store, secret_box) = sekisho_testkit::store().await;
    let spec = EnvironmentSpec {
        name: EnvName::parse("data-team").unwrap(),
        description: String::new(),
        notice: String::new(),
        access_groups: vec!["eng".into()],
        models: Vec::new(),
        owner: None,
    };
    let env = store
        .upsert_environment(
            spec,
            sekisho_testkit::people::person(&store, "admin@example.com", &["eng"]).await,
        )
        .await
        .unwrap()
        .spec
        .name;
    let expires = OffsetDateTime::now_utc() + time::Duration::days(1);
    let (key, token) = store
        .create_key(
            &env,
            sekisho_testkit::people::person(&store, "alice@example.com", &["eng"]).await,
            "ci",
            Some(expires),
        )
        .await
        .unwrap();
    let cache = AuthCache::load(&store, ISSUER.into()).await.unwrap();
    cache.spawn_refresh(store.clone());
    let now = Arc::new(Mutex::new(OffsetDateTime::now_utc()));
    let clock_now = Arc::clone(&now);
    let tallies = Tallies::new();
    let guard = StreamGuard {
        cache: Arc::clone(&cache),
        store: store.clone(),
        clock: Arc::new(move || *clock_now.lock().unwrap()),
        tallies: tallies.clone(),
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
        secret_box,
        cache,
        alice: key.owner,
        now,
        token,
        key: key.id,
        tallies,
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

/// Response headers with the answer's status, as agentgateway sends them.
async fn send_response_status(tx: &Tx, status: &str) {
    tx.send(message(Request::ResponseHeaders(HttpHeaders {
        headers: Some(HeaderMap {
            headers: vec![HeaderValue {
                key: ":status".into(),
                value: String::new(),
                raw_value: status.as_bytes().to_vec(),
            }],
        }),
        end_of_stream: false,
    })))
    .await
    .unwrap();
}

/// An OpenAI chat chunk carrying `text`.
fn openai_chunk(text: &str) -> Vec<u8> {
    format!("data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\n")
        .into_bytes()
}

/// The request ID the guard put in its answer to the request headers.
fn request_id(answer: &ProcessingResponse) -> String {
    assert!(
        matches!(answer.response, Some(Response::RequestHeaders(_))),
        "not the request-headers answer: {answer:?}"
    );
    let metadata = answer.dynamic_metadata.as_ref().expect("dynamic metadata");
    match &metadata.fields["sekisho_request"].kind {
        Some(Kind::StringValue(id)) => id.clone(),
        other => panic!("sekisho_request is not a string: {other:?}"),
    }
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

#[tokio::test]
async fn requests_get_an_id_in_dynamic_metadata() {
    let rig = rig().await;
    let token = format!("Bearer {}", rig.token);
    let (_tx, mut out) = open_stream(&rig, Some(&token), true).await;
    let id = request_id(&next(&mut out).await.unwrap());
    assert_eq!(id.len(), 32);
    assert!(
        id.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    let (_tx, mut out) = open_stream(&rig, Some(&token), true).await;
    assert_ne!(request_id(&next(&mut out).await.unwrap()), id);
}

#[tokio::test]
async fn answer_text_is_tallied_when_the_stream_ends() {
    let rig = rig().await;
    let token = format!("Bearer {}", rig.token);
    let (tx, mut out) = open_stream(&rig, Some(&token), true).await;
    let id = request_id(&next(&mut out).await.unwrap());
    send_response_status(&tx, "200").await;
    next(&mut out).await.unwrap();
    for text in ["t0 ", "t1 "] {
        send_body_chunk(&tx, &openai_chunk(text)).await;
        assert_eq!(next(&mut out).await.unwrap().body(), openai_chunk(text));
    }
    assert_eq!(
        rig.tallies.wait(&id, Duration::from_millis(100)).await,
        None,
        "not finished while the stream runs"
    );
    drop(tx); // the client ends the stream
    assert_eq!(
        rig.tallies.wait(&id, Duration::from_secs(1)).await,
        Some(Tally {
            status: Some(200),
            answer_o200k: count_text("t0 ") + count_text("t1 "),
            ..Tally::default()
        })
    );
}

#[tokio::test]
async fn a_revoked_stream_is_tallied_too() {
    let rig = rig().await;
    let token = format!("Bearer {}", rig.token);
    let (tx, mut out) = open_stream(&rig, Some(&token), true).await;
    let id = request_id(&next(&mut out).await.unwrap());
    send_response_status(&tx, "200").await;
    next(&mut out).await.unwrap();
    send_body_chunk(&tx, &openai_chunk("relayed")).await;
    next(&mut out).await.unwrap();
    rig.store.revoke_key(&rig.key).await.unwrap();
    wait_cache(&rig, "revocation", |c| c.key(&rig.key).unwrap().revoked).await;
    send_body_chunk(&tx, &openai_chunk("never relayed")).await;
    assert_eq!(next(&mut out).await.unwrap().immediate_status(), 403);
    // The tally finishes when the guard ends the stream, with the client
    // still connected: only what reached the client is counted.
    assert_eq!(
        rig.tallies.wait(&id, Duration::from_secs(1)).await,
        Some(Tally {
            status: Some(200),
            answer_o200k: count_text("relayed"),
            ..Tally::default()
        })
    );
    drop(tx);
}

#[tokio::test]
async fn the_providers_counts_are_tallied() {
    let rig = rig().await;
    let token = format!("Bearer {}", rig.token);
    let (tx, mut out) = open_stream(&rig, Some(&token), true).await;
    let id = request_id(&next(&mut out).await.unwrap());
    send_response_status(&tx, "200").await;
    next(&mut out).await.unwrap();
    let usage = br#"data: {"choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":2}}

"#;
    for chunk in [&openai_chunk("t0 ")[..], usage] {
        send_body_chunk(&tx, chunk).await;
        next(&mut out).await.unwrap();
    }
    drop(tx);
    assert_eq!(
        rig.tallies.wait(&id, Duration::from_secs(1)).await,
        Some(Tally {
            status: Some(200),
            answer_o200k: count_text("t0 "),
            usage: ProviderUsage {
                prompt: Some(Usage {
                    input: 1000,
                    ..Usage::default()
                }),
                output: Some(2),
                start_output: None,
            },
        })
    );
}

/// A session for `user` and its bearer header, with the cache reloaded so
/// it knows the signing key.
async fn session(rig: &Rig, user: Uuid) -> (String, Uuid) {
    let (token, session) = session_token(&rig.store, &rig.secret_box, ISSUER, user).await;
    rig.cache.reload(&rig.store).await.unwrap();
    (format!("Bearer {token}"), session)
}

/// Opens a stream with `authorization` and checks one chunk is echoed.
async fn streaming(rig: &Rig, authorization: &str) -> (Tx, Streaming<ProcessingResponse>) {
    let (tx, mut out) = open_stream(rig, Some(authorization), true).await;
    assert!(matches!(
        next(&mut out).await.unwrap().response,
        Some(Response::RequestHeaders(_))
    ));
    send_body_chunk(&tx, b"data: a\n\n").await;
    assert_eq!(next(&mut out).await.unwrap().body(), b"data: a\n\n");
    (tx, out)
}

async fn assert_cut(tx: &Tx, out: &mut Streaming<ProcessingResponse>, reason: &str) {
    send_body_chunk(tx, b"data: b\n\n").await;
    let cut = next(out).await.unwrap();
    assert_eq!(cut.immediate_status(), 403);
    assert_eq!(
        cut.immediate_body(),
        serde_json::json!({ "error": "forbidden", "reason": reason })
    );
    assert!(next(out).await.is_none(), "the guard ends the stream");
}

#[tokio::test]
async fn ending_a_session_cuts_its_stream() {
    let rig = rig().await;
    let (authorization, session) = session(&rig, rig.alice).await;
    let (tx, mut out) = streaming(&rig, &authorization).await;
    rig.store
        .end_session(&session, "logout", OffsetDateTime::now_utc())
        .await
        .unwrap();
    wait_cache(&rig, "the ended session", |c| c.session_ended(&session)).await;
    assert_cut(&tx, &mut out, "session_ended").await;
}

#[tokio::test]
async fn disabling_the_user_cuts_their_stream() {
    let rig = rig().await;
    let (authorization, _) = session(&rig, rig.alice).await;
    let (tx, mut out) = streaming(&rig, &authorization).await;
    rig.store
        .set_disabled(&rig.alice, true, OffsetDateTime::now_utc())
        .await
        .unwrap();
    wait_cache(&rig, "the disabling", |c| {
        c.user(&rig.alice).is_some_and(|u| u.access.disabled)
    })
    .await;
    assert_cut(&tx, &mut out, "session_ended").await;

    // Disabling ends sessions too; a disabled user is cut even where their
    // session row was left open.
    let bob = sekisho_testkit::people::person(&rig.store, "bob@example.com", &["eng"]).await;
    let (authorization, _) = session(&rig, bob).await;
    let (tx, mut out) = streaming(&rig, &authorization).await;
    sqlx::query("update users set disabled_at = now() where id = $1")
        .bind(bob)
        .execute(rig.store.pool())
        .await
        .unwrap();
    rig.cache.reload(&rig.store).await.unwrap();
    assert_cut(&tx, &mut out, "session_ended").await;
}

#[tokio::test]
async fn an_expiring_token_keeps_its_stream() {
    let rig = rig().await;
    let (authorization, _) = session(&rig, rig.alice).await;
    let (tx, mut out) = streaming(&rig, &authorization).await;
    // The token's hour runs out mid-stream: the stream goes on.
    *rig.now.lock().unwrap() = OffsetDateTime::now_utc() + time::Duration::hours(2);
    send_body_chunk(&tx, b"data: b\n\n").await;
    assert_eq!(next(&mut out).await.unwrap().body(), b"data: b\n\n");
    // A new stream with the expired token is refused at its headers.
    let (_tx, mut late) = open_stream(&rig, Some(&authorization), true).await;
    let refused = next(&mut late).await.unwrap();
    assert_eq!(refused.immediate_body()["reason"], "token_expired");
}

#[tokio::test]
async fn an_invalid_key_cuts_its_stream() {
    let rig = rig().await;
    let token = format!("Bearer {}", rig.token);
    let (tx, mut out) = streaming(&rig, &token).await;
    // alice leaves the environment's access group.
    sekisho_testkit::people::person(&rig.store, "alice@example.com", &[]).await;
    wait_cache(&rig, "the lost access", |c| {
        c.user(&rig.alice)
            .is_some_and(|u| u.access.groups.is_empty())
    })
    .await;
    assert_cut(&tx, &mut out, "invalid").await;
}
