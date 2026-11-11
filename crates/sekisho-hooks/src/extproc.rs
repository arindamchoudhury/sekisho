//! The Envoy external processor, used to end running streams and to tally
//! what they relayed.
//!
//! agentgateway sends the request headers, then each response chunk
//! (`responseBodyMode: fullDuplexStreamed`). The guard echoes every chunk
//! unchanged. Once the key is revoked, suspended or expired, it answers the
//! next message with a 403 `ImmediateResponse` and ends the stream; the
//! client keeps its 200 but gets no more data, and the upstream connection
//! closes.
//!
//! It also gives each request a [`Tallies`] ID, answered as dynamic metadata
//! (`extproc.sekisho_request` in agentgateway's CEL), and counts the answer
//! text of every chunk it relays, for the spend estimate when a stream ends
//! before the provider reports its usage.

use std::sync::Arc;

use prost_types::value::Kind;
use sekisho_core::keys::EnvKeyId;
use sekisho_proto::common::{HeaderValue, HeaderValueOption, HttpStatus};
use sekisho_proto::ext_proc::body_mutation::Mutation;
use sekisho_proto::ext_proc::external_processor_server::ExternalProcessor;
use sekisho_proto::ext_proc::processing_request::Request as Message;
use sekisho_proto::ext_proc::processing_response::Response as Answer;
use sekisho_proto::ext_proc::{
    BodyMutation, BodyResponse, CommonResponse, HeaderMutation, HeadersResponse, HttpBody,
    HttpHeaders, ImmediateResponse, ProcessingRequest, ProcessingResponse, StreamedBodyResponse,
    TrailersResponse,
};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::cache::AuthCache;
use crate::credential::{Credential, refusal, resolve};
use crate::tally::{AnswerText, Tallies};

/// The dynamic-metadata field holding the request's tally ID.
const REQUEST_FIELD: &str = "sekisho_request";

pub struct StreamGuard {
    pub cache: Arc<AuthCache>,
    pub clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
    /// Shared with the rate-limit service, which reads each finished tally.
    pub tallies: Tallies,
}

#[tonic::async_trait]
impl ExternalProcessor for StreamGuard {
    type ProcessStream = ReceiverStream<Result<ProcessingResponse, Status>>;

    async fn process(
        &self,
        request: Request<Streaming<ProcessingRequest>>,
    ) -> Result<Response<Self::ProcessStream>, Status> {
        let inbound = request.into_inner();
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(guard(
            inbound,
            tx,
            Arc::clone(&self.cache),
            Arc::clone(&self.clock),
            self.tallies.clone(),
        ));
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

async fn guard(
    mut inbound: Streaming<ProcessingRequest>,
    tx: mpsc::Sender<Result<ProcessingResponse, Status>>,
    cache: Arc<AuthCache>,
    clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
    tallies: Tallies,
) {
    let mut key: Option<EnvKeyId> = None;
    // Finished when this function ends, however it ends.
    let mut tally: Option<OpenTally> = None;
    let mut text = AnswerText::default();
    while let Some(Ok(message)) = inbound.next().await {
        let Some(message) = message.request else {
            continue;
        };
        if let (None, Message::RequestHeaders(headers)) = (&key, &message) {
            key = match resolve(&cache, authorization(headers).as_deref()) {
                Credential::Known(k) => Some(k.id),
                _ => None,
            };
        }
        // Checked before every answer, so a revocation lands on the next chunk.
        let refused = match &key {
            None => Some("unknown_key"),
            Some(id) => match cache.key(id) {
                Some(k) => refusal(&k, clock()),
                None => Some("unknown_key"), // deleted with its environment
            },
        };
        if let Some(reason) = refused {
            let _ = tx.send(Ok(refuse(reason))).await;
            return; // ending the stream tells agentgateway nothing more comes
        }
        // Only what is relayed counts: a refused chunk never reaches the client.
        let mut metadata = None;
        match (&message, &tally) {
            (Message::RequestHeaders(_), None) => {
                let opened = OpenTally {
                    id: tallies.open(),
                    tallies: tallies.clone(),
                };
                metadata = Some(request_metadata(&opened.id));
                tally = Some(opened);
            }
            (Message::ResponseHeaders(headers), Some(t)) => {
                if let Some(status) = header(headers, ":status").and_then(|s| s.parse().ok()) {
                    tallies.set_status(&t.id, status);
                }
            }
            (Message::ResponseBody(body), Some(t)) => {
                let before = text.usage();
                tallies.add(&t.id, text.push(&body.body));
                if text.usage() != before {
                    tallies.set_usage(&t.id, text.usage());
                }
            }
            _ => {}
        }
        let mut response = answer(message);
        response.dynamic_metadata = metadata;
        if tx.send(Ok(response)).await.is_err() {
            return;
        }
    }
}

/// A request's tally, finished when dropped: the stream ended, was refused,
/// or the guard's task was cancelled.
struct OpenTally {
    id: String,
    tallies: Tallies,
}

impl Drop for OpenTally {
    fn drop(&mut self) {
        self.tallies.finish(&self.id);
    }
}

/// `{"sekisho_request": id}`, which agentgateway exposes to the spend
/// descriptors as `extproc.sekisho_request`.
fn request_metadata(id: &str) -> prost_types::Struct {
    let value = prost_types::Value {
        kind: Some(Kind::StringValue(id.to_owned())),
    };
    prost_types::Struct {
        fields: [(REQUEST_FIELD.to_owned(), value)].into(),
    }
}

/// The `authorization` request header.
fn authorization(headers: &HttpHeaders) -> Option<String> {
    header(headers, "authorization")
}

/// The one header called `name`. Ordinary headers arrive as `raw_value`
/// bytes; `value` is accepted too.
fn header(headers: &HttpHeaders, name: &str) -> Option<String> {
    let mut found = headers
        .headers
        .as_ref()?
        .headers
        .iter()
        .filter(|h| h.key.eq_ignore_ascii_case(name));
    let header = found.next()?;
    if found.next().is_some() {
        return None; // two of them: refuse rather than guess
    }
    if header.raw_value.is_empty() {
        Some(header.value.clone())
    } else {
        String::from_utf8(header.raw_value.clone()).ok()
    }
}

fn echo(body: HttpBody) -> BodyResponse {
    BodyResponse {
        response: Some(CommonResponse {
            body_mutation: Some(BodyMutation {
                mutation: Some(Mutation::StreamedResponse(StreamedBodyResponse {
                    body: body.body,
                    end_of_stream: body.end_of_stream,
                })),
            }),
            ..Default::default()
        }),
    }
}

/// Lets each phase continue; body chunks are echoed as streamed responses,
/// as `fullDuplexStreamed` requires.
fn answer(message: Message) -> ProcessingResponse {
    let response = match message {
        Message::RequestHeaders(_) => Answer::RequestHeaders(HeadersResponse::default()),
        Message::ResponseHeaders(_) => Answer::ResponseHeaders(HeadersResponse::default()),
        Message::RequestBody(body) => Answer::RequestBody(echo(body)),
        Message::ResponseBody(body) => Answer::ResponseBody(echo(body)),
        Message::RequestTrailers(_) => Answer::RequestTrailers(TrailersResponse::default()),
        Message::ResponseTrailers(_) => Answer::ResponseTrailers(TrailersResponse::default()),
    };
    ProcessingResponse {
        response: Some(response),
        ..Default::default()
    }
}

fn refuse(reason: &'static str) -> ProcessingResponse {
    let body = serde_json::json!({ "error": "forbidden", "reason": reason }).to_string();
    ProcessingResponse {
        response: Some(Answer::ImmediateResponse(ImmediateResponse {
            status: Some(HttpStatus { code: 403 }),
            headers: Some(HeaderMutation {
                set_headers: vec![HeaderValueOption {
                    header: Some(HeaderValue {
                        key: "content-type".into(),
                        value: "application/json".into(),
                        raw_value: Vec::new(),
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            body,
            grpc_status: None,
            details: reason.into(),
        })),
        ..Default::default()
    }
}
