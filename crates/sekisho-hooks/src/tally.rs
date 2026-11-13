//! What the stream hook counted of each answer, kept until the request is
//! charged.
//!
//! The stream hook opens a tally when a request starts and hands its ID to
//! agentgateway as ext_proc dynamic metadata; the spend descriptors carry it
//! back as `extproc.sekisho_request`. The hook adds the tokens of the answer
//! text it relays to the client and finishes the tally when the stream ends,
//! however it ends.
//!
//! agentgateway v1.7.0 reports a request's usage to the rate-limit service
//! when the provider's usage arrives, and never for a stream that ends
//! before it. So the rate-limit service records each request's start here
//! (key, model and prompt count, from agentgateway's request-side call) and
//! watches its tally: a tally that ends with no report is charged an
//! estimate, and a report that comes after that estimate is refused, so each
//! request is charged once.
//!
//! Tallies live in this process: a request's stream hook and its charge must
//! reach the same Sekisho (plan 8 shares them between replicas).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rand::TryRng;
use rand::rngs::SysRng;
use sekisho_core::names::EnvName;
use sekisho_core::pricing::Usage;
use serde_json::Value;
use tokio::sync::Notify;

use crate::ratelimit::Payer;
use crate::tokens::count_text;

/// A tally nobody watches, untouched for this long, is dropped: no charge
/// came for it. Measured from the last change, so a long stream keeps it.
pub const KEEP: Duration = Duration::from_secs(10 * 60);

/// A watched stream that changes nothing for this long is taken as stalled
/// and estimated as it stands. Requests run for minutes, not an hour, so a
/// live one (a long answer, or a slow non-streaming one) is never cut short
/// and its report is charged. An estimated request's ID is remembered this
/// long, so a report after the estimate is never charged on top.
pub const STALLED: Duration = Duration::from_secs(60 * 60);

/// A finished tally that no rate-limit call ever started (count_tokens, a
/// path that isn't a model call) is dropped this long after its end. The
/// request-side call comes before the provider is even asked, so a minute
/// is far more than it needs.
const UNCLAIMED: Duration = Duration::from_secs(60);

/// How often [`Tallies::open`] looks for tallies to drop.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// A line longer than this is not an answer event (those are a few hundred
/// bytes); it is skipped rather than kept in memory.
const MAX_LINE: usize = 1 << 20;

/// What the stream hook saw of one answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// The answer's HTTP status, once its headers arrived. agentgateway's
    /// own error answers (an unreachable provider) never get one.
    pub status: Option<u16>,
    /// The `o200k` count of the answer text relayed to the client, before
    /// any model's scaling.
    pub answer_o200k: u64,
    /// The provider's counts, from its own usage events in the stream.
    pub usage: ProviderUsage,
}

/// The counts a provider reports in its own stream: the prompt's near the
/// start (Anthropic, Gemini) or with the output at the end (OpenAI), and the
/// output once the answer is complete.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProviderUsage {
    /// The whole prompt with its cache reads and writes (`output` is 0).
    pub prompt: Option<Usage>,
    /// The answer's final token count.
    pub output: Option<u64>,
    /// The output count in Anthropic's `message_start`. agentgateway
    /// reports it as the output when a stream ends without `message_delta`
    /// (the provider failed mid-answer), so a report of it is stale.
    pub start_output: Option<u64>,
}

/// What agentgateway reported of a request that has no output count yet:
/// whom to charge, for which model, and the prompt (or, from a response-side
/// call without the output count, every count it had).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Start {
    /// Whom the request is charged to.
    pub payer: Payer,
    pub env: EnvName,
    pub model: String,
    /// The output count is always 0 here.
    pub usage: Usage,
    /// The counts came from a response-side call (the provider's, through
    /// agentgateway), which a request-side prompt count never replaces.
    pub from_response: bool,
}

/// What [`Tallies::start`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recorded {
    /// The request's first start: the caller watches it
    /// ([`Tallies::unreported`]).
    First,
    /// A later one, whose counts replaced the earlier ones.
    Again,
    /// The request was already charged an estimate.
    Estimated,
    /// No tally here for this ID.
    Unknown,
}

struct Entry {
    tally: Tally,
    finished: bool,
    start: Option<Start>,
    touched: Instant,
    /// Wakes this tally's waiters when it finishes or is taken; each tally
    /// has its own, so one stream's end wakes only its own watcher.
    changed: Arc<Notify>,
}

impl Entry {
    /// Wakes its waiters as it is taken out of the book.
    fn taken(self) -> Self {
        self.changed.notify_waiters();
        self
    }
}

struct Book {
    entries: HashMap<String, Entry>,
    /// IDs charged an estimate, so a later report is refused.
    estimated: HashMap<String, Instant>,
    swept: Instant,
}

struct Inner {
    book: Mutex<Book>,
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
}

/// Every request's tally, shared by the stream hook and the rate-limit
/// service. Cloning shares it.
#[derive(Clone)]
pub struct Tallies(Arc<Inner>);

impl Default for Tallies {
    fn default() -> Self {
        Self::new()
    }
}

/// How [`Tallies::until`] ended.
enum Until {
    Done,
    Gone,
    TimedOut,
}

impl Tallies {
    pub fn new() -> Self {
        Self::with_clock(Arc::new(Instant::now))
    }

    /// Tallies whose ages are measured by `clock`, for tests.
    pub fn with_clock(clock: Arc<dyn Fn() -> Instant + Send + Sync>) -> Self {
        let now = clock();
        Self(Arc::new(Inner {
            book: Mutex::new(Book {
                entries: HashMap::new(),
                estimated: HashMap::new(),
                swept: now,
            }),
            clock,
        }))
    }

    /// Opens a tally for a new request and returns its ID: 32 lowercase hex
    /// characters, random, so a client can't guess another request's.
    pub fn open(&self) -> String {
        let mut bytes = [0u8; 16];
        SysRng
            .try_fill_bytes(&mut bytes)
            .expect("the operating system's random number generator failed");
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let now = (self.0.clock)();
        let mut book = self.book();
        if now.saturating_duration_since(book.swept) >= SWEEP_EVERY {
            let within = |at: &Instant, limit| now.saturating_duration_since(*at) < limit;
            // A watched tally is its watcher's to take; this only catches one
            // whose watcher is gone.
            book.entries.retain(|_, e| {
                let limit = match (&e.start, e.finished) {
                    (Some(_), _) => STALLED + KEEP,
                    (None, true) => UNCLAIMED,
                    (None, false) => KEEP,
                };
                let keep = within(&e.touched, limit);
                if !keep {
                    e.changed.notify_waiters();
                }
                keep
            });
            book.estimated.retain(|_, at| within(at, STALLED));
            book.swept = now;
        }
        book.entries.insert(
            id.clone(),
            Entry {
                tally: Tally::default(),
                finished: false,
                start: None,
                touched: now,
                changed: Arc::new(Notify::new()),
            },
        );
        id
    }

    /// Records the answer's HTTP status.
    pub fn set_status(&self, id: &str, status: u16) {
        self.touch(id, |e| e.tally.status = Some(status));
    }

    /// Records the provider's counts from its stream.
    pub fn set_usage(&self, id: &str, usage: ProviderUsage) {
        self.touch(id, |e| e.tally.usage = usage);
    }

    /// Adds `tokens` of relayed answer text.
    pub fn add(&self, id: &str, tokens: u64) {
        self.touch(id, |e| {
            e.tally.answer_o200k = e.tally.answer_o200k.saturating_add(tokens);
        });
    }

    /// Marks the tally complete: its stream has ended.
    pub fn finish(&self, id: &str) {
        self.touch(id, |e| {
            e.finished = true;
            e.changed.notify_waiters();
        });
    }

    /// Drops a tally nobody will read.
    pub fn discard(&self, id: &str) {
        self.book().entries.remove(id).map(Entry::taken);
    }

    /// Records what agentgateway reported of a request without its output
    /// count. A later record replaces an earlier one's counts, except that
    /// a request-side prompt count never replaces a response-side call's.
    pub fn start(&self, id: &str, start: Start) -> Recorded {
        let now = (self.0.clock)();
        let mut book = self.book();
        if book.estimated.contains_key(id) {
            return Recorded::Estimated;
        }
        let Some(entry) = book.entries.get_mut(id) else {
            return Recorded::Unknown;
        };
        entry.touched = now;
        match &entry.start {
            None => {
                entry.start = Some(start);
                Recorded::First
            }
            Some(recorded) => {
                if start.from_response || !recorded.from_response {
                    entry.start = Some(start);
                }
                Recorded::Again
            }
        }
    }

    /// Whether a report's `output` is only the stream's `message_start`
    /// count: the stream showed no final count, and agentgateway reported
    /// the start's as the output.
    pub fn stale(&self, id: &str, output: u64) -> bool {
        self.book().entries.get(id).is_some_and(|e| {
            e.tally.usage.output.is_none() && e.tally.usage.start_output == Some(output)
        })
    }

    /// agentgateway reported the request's usage, so its tally isn't needed.
    /// `false` when Sekisho already charged the request an estimate: the
    /// report must not be charged too.
    pub fn reported(&self, id: &str) -> bool {
        let mut book = self.book();
        if book.estimated.contains_key(id) {
            return false;
        }
        book.entries.remove(id).map(Entry::taken);
        true
    }

    /// For a request agentgateway may never report: waits until its stream
    /// ends, then up to `grace` for a report, then takes the tally to charge
    /// an estimate, remembering that it did. A stream that changes nothing
    /// for `quiet` is taken as stalled. `None` when a report took the tally
    /// first, or the request has no start.
    pub async fn unreported(
        &self,
        id: &str,
        grace: Duration,
        quiet: Duration,
    ) -> Option<(Start, Tally)> {
        loop {
            match self
                .until(id, self.quiet_left(id, quiet)?, |e| e.finished)
                .await
            {
                Until::Gone => return None,
                Until::Done => {
                    if let Until::Gone = self.until(id, grace, |_| false).await {
                        return None; // reported in the grace
                    }
                    break;
                }
                // Text kept coming: wait on from its last change.
                Until::TimedOut if !self.quiet_left(id, quiet)?.is_zero() => {}
                Until::TimedOut => {
                    tracing::warn!(
                        request = id,
                        "a stream changed nothing for {quiet:?}: estimating it as it stands"
                    );
                    break;
                }
            }
        }
        let now = (self.0.clock)();
        let mut book = self.book();
        let entry = book.entries.remove(id)?.taken();
        let start = entry.start?;
        book.estimated.insert(id.to_owned(), now);
        Some((start, entry.tally))
    }

    /// The tally once its stream has ended, removed so it is read only once.
    /// `None` for an unknown ID, or when the stream hasn't ended within
    /// `timeout`.
    pub async fn wait(&self, id: &str, timeout: Duration) -> Option<Tally> {
        match self.until(id, timeout, |e| e.finished).await {
            Until::Done => self.book().entries.remove(id).map(|e| e.taken().tally),
            Until::Gone | Until::TimedOut => None,
        }
    }

    /// How much longer `id`'s tally may stay unchanged before it counts as
    /// stalled. `None` when it is gone.
    fn quiet_left(&self, id: &str, quiet: Duration) -> Option<Duration> {
        let now = (self.0.clock)();
        let touched = self.book().entries.get(id)?.touched;
        Some(quiet.saturating_sub(now.saturating_duration_since(touched)))
    }

    /// Waits until `id`'s tally is `done`, is gone, or `timeout` passes.
    async fn until(&self, id: &str, timeout: Duration, done: impl Fn(&Entry) -> bool) -> Until {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let Some(signal) = self.book().entries.get(id).map(|e| Arc::clone(&e.changed)) else {
                return Until::Gone;
            };
            // Registered before looking, so a change in between still wakes us.
            let changed = signal.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.book().entries.get(id) {
                None => return Until::Gone,
                Some(entry) if done(entry) => return Until::Done,
                Some(_) => {}
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return Until::TimedOut;
            }
        }
    }

    fn touch(&self, id: &str, change: impl FnOnce(&mut Entry)) {
        let now = (self.0.clock)();
        if let Some(entry) = self.book().entries.get_mut(id) {
            change(entry);
            entry.touched = now;
        }
    }

    fn book(&self) -> MutexGuard<'_, Book> {
        // A panic elsewhere can't leave an entry half-written: keep going.
        self.0.book.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Reads the answer text out of a streamed (server-sent events) answer,
/// chunk by chunk, in whichever format the client speaks.
#[derive(Default)]
pub struct AnswerText {
    /// The unfinished line from the chunks so far.
    line: Vec<u8>,
    /// The unfinished line grew past [`MAX_LINE`]: skip to its end.
    overlong: bool,
    /// The provider's counts from the usage events so far.
    usage: ProviderUsage,
}

impl AnswerText {
    /// The `o200k` count of the answer text in the events this chunk
    /// completes. An event split across chunks is counted once, when its
    /// line ends.
    pub fn push(&mut self, chunk: &[u8]) -> u64 {
        let mut tokens = 0u64;
        let mut rest = chunk;
        while let Some(end) = rest.iter().position(|&b| b == b'\n') {
            let (head, tail) = (&rest[..end], &rest[end + 1..]);
            rest = tail;
            if std::mem::take(&mut self.overlong) {
                continue;
            }
            let line = if self.line.is_empty() {
                head
            } else {
                self.line.extend_from_slice(head);
                &self.line
            };
            tokens = tokens.saturating_add(read_line(line, &mut self.usage));
            self.line.clear();
        }
        if !self.overlong {
            self.line.extend_from_slice(rest);
            if self.line.len() > MAX_LINE {
                self.line = Vec::new();
                self.overlong = true;
            }
        }
        tokens
    }

    /// The provider's counts from the usage events read so far.
    pub fn usage(&self) -> ProviderUsage {
        self.usage
    }
}

/// The answer tokens of one SSE line, a `data:` line holding an answer
/// event; a usage event's counts go into `usage`.
fn read_line(line: &[u8], usage: &mut ProviderUsage) -> u64 {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let Some(data) = line.strip_prefix(b"data:") else {
        return 0; // `event:`, comments, blank lines
    };
    let data = data.strip_prefix(b" ").unwrap_or(data);
    let Ok(event) = serde_json::from_slice::<Value>(data) else {
        return 0; // `[DONE]`
    };
    provider_usage(&event, usage);
    let mut tokens = 0u64;
    answer_text(&event, &mut |text| {
        tokens = tokens.saturating_add(count_text(text));
    });
    tokens
}

/// Reads a provider's usage event into `usage`. A prompt count of 0 (an
/// empty start from a translated stream) is ignored.
fn provider_usage(event: &Value, usage: &mut ProviderUsage) {
    let count = |value: &Value| value.as_u64().unwrap_or(0);
    let mut prompt = |input: u64, cache_read: u64, cache_write: u64| {
        if input > 0 {
            usage.prompt = Some(Usage {
                input,
                cache_read,
                cache_write,
                output: 0,
            });
        }
    };
    // Anthropic: uncached input plus cache reads and writes.
    let anthropic = |u: &Value| {
        let (read, write) = (
            count(&u["cache_read_input_tokens"]),
            count(&u["cache_creation_input_tokens"]),
        );
        (count(&u["input_tokens"]) + read + write, read, write)
    };
    match event["type"].as_str() {
        Some("message_start") => {
            let (input, read, write) = anthropic(&event["message"]["usage"]);
            if input > 0 {
                usage.start_output = event["message"]["usage"]["output_tokens"].as_u64();
            }
            prompt(input, read, write);
        }
        Some("message_delta") if event["usage"].is_object() => {
            let (input, read, write) = anthropic(&event["usage"]);
            prompt(input, read, write);
            usage.output = event["usage"]["output_tokens"].as_u64();
        }
        // OpenAI Responses: input with its cached part, output with reasoning.
        Some("response.completed" | "response.incomplete") => {
            let u = &event["response"]["usage"];
            prompt(
                count(&u["input_tokens"]),
                count(&u["input_tokens_details"]["cached_tokens"]),
                0,
            );
            usage.output = u["output_tokens"].as_u64();
        }
        _ => {}
    }
    // OpenAI chat: the usage chunk at the end.
    let u = &event["usage"];
    if u["prompt_tokens"].is_u64() {
        prompt(
            count(&u["prompt_tokens"]),
            count(&u["prompt_tokens_details"]["cached_tokens"]),
            0,
        );
        usage.output = u["completion_tokens"].as_u64();
    }
    // Gemini: in every chunk; the output is final once a candidate finished.
    let u = &event["usageMetadata"];
    if u.is_object() {
        prompt(
            count(&u["promptTokenCount"]),
            count(&u["cachedContentTokenCount"]),
            0,
        );
        let finished = event["candidates"]
            .as_array()
            .is_some_and(|c| c.iter().any(|c| c["finishReason"].is_string()));
        if finished {
            usage.output =
                Some(count(&u["candidatesTokenCount"]) + count(&u["thoughtsTokenCount"]));
        }
    }
}

/// Calls `found` with each piece of answer text in one event. Usage and
/// bookkeeping events hold none.
fn answer_text(event: &Value, found: &mut dyn FnMut(&str)) {
    match &event["delta"] {
        // Anthropic: text, thinking and tool input.
        delta @ Value::Object(_) => {
            for field in ["text", "thinking", "partial_json"] {
                emit(&delta[field], found);
            }
        }
        // OpenAI Responses: every `*.delta` event's text.
        delta @ Value::String(_)
            if event["type"]
                .as_str()
                .is_some_and(|t| t.ends_with(".delta")) =>
        {
            emit(delta, found);
        }
        _ => {}
    }
    // OpenAI chat: content, reasoning (`reasoning_content`, or `reasoning`
    // from OpenRouter and vLLM) and tool arguments.
    for choice in event["choices"].as_array().into_iter().flatten() {
        let delta = &choice["delta"];
        for field in ["content", "reasoning_content", "reasoning"] {
            emit(&delta[field], found);
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            emit(&call["function"]["arguments"], found);
        }
    }
    // Gemini: text parts and function-call arguments as compact JSON.
    for candidate in event["candidates"].as_array().into_iter().flatten() {
        for part in candidate["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
        {
            emit(&part["text"], found);
            let args = &part["functionCall"]["args"];
            if !args.is_null() {
                found(&args.to_string());
            }
        }
    }
}

/// Passes `value` on when it is a non-empty string.
fn emit(value: &Value, found: &mut dyn FnMut(&str)) {
    if let Some(text) = value.as_str().filter(|s| !s.is_empty()) {
        found(text);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use sekisho_core::keys::EnvKeyId;
    use sekisho_core::names::EnvName;
    use sekisho_core::pricing::Usage;

    use super::*;
    use crate::tokens::count_text;

    fn start(prompt: u64) -> Start {
        Start {
            payer: Payer {
                person: uuid::Uuid::nil(),
                key: EnvKeyId::parse("aaaaaaaaaaaa"),
            },
            env: EnvName::parse("data-team").unwrap(),
            model: "gpt-priced".into(),
            usage: Usage {
                input: prompt,
                ..Usage::default()
            },
            from_response: false,
        }
    }

    fn event(json: &str) -> Vec<u8> {
        format!("data: {json}\n\n").into_bytes()
    }

    #[test]
    fn each_format_yields_its_answer_text() {
        let cases = [
            // Anthropic: text, thinking and tool input.
            (
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello there"}}"#,
                "Hello there",
            ),
            (
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think it over"}}"#,
                "Let me think it over",
            ),
            (
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\": \"Par"}}"#,
                r#"{"city": "Par"#,
            ),
            // OpenAI chat: content, reasoning and tool arguments.
            (
                r#"{"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Sure, here it is"}}]}"#,
                "Sure, here it is",
            ),
            (
                r#"{"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"reasoning_content":"First, the files"}}]}"#,
                "First, the files",
            ),
            (
                r#"{"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"reasoning":"Then the tests"}}]}"#,
                "Then the tests",
            ),
            (
                r#"{"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":\"src"}}]}}]}"#,
                r#"{"path":"src"#,
            ),
            // OpenAI Responses: any `*.delta` event's text.
            (
                r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"Done in two steps"}"#,
                "Done in two steps",
            ),
            (
                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":1,"delta":"{\"query\":\"rust"}"#,
                r#"{"query":"rust"#,
            ),
            // Gemini: text parts and function-call arguments as compact JSON.
            (
                r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"The answer is 42"}]}}]}"#,
                "The answer is 42",
            ),
            (
                r#"{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"get_weather","args":{"city": "Paris"}}}]}}]}"#,
                r#"{"city":"Paris"}"#,
            ),
        ];
        for (json, text) in cases {
            assert!(count_text(text) > 0, "{text}");
            assert_eq!(
                AnswerText::default().push(&event(json)),
                count_text(text),
                "{json}"
            );
        }
        // `data:` without the space, and CRLF line ends.
        let crlf = "data:{\"choices\":[{\"delta\":{\"content\":\"over CRLF\"}}]}\r\n\r\n";
        assert_eq!(
            AnswerText::default().push(crlf.as_bytes()),
            count_text("over CRLF")
        );
        // Several events in one chunk are each counted.
        let two = [
            event(r#"{"choices":[{"delta":{"content":"one "}}]}"#),
            event(r#"{"choices":[{"delta":{"content":"two"}}]}"#),
        ]
        .concat();
        assert_eq!(
            AnswerText::default().push(&two),
            count_text("one ") + count_text("two")
        );
    }

    #[test]
    fn split_events_are_counted_once() {
        let text = "split across two chunks of the stream";
        let whole = event(&format!(
            r#"{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"{text}"}}}}"#
        ));
        for at in [1, 20, whole.len() / 2, whole.len() - 2, whole.len() - 1] {
            let mut answer = AnswerText::default();
            let first = answer.push(&whole[..at]);
            let second = answer.push(&whole[at..]);
            assert_eq!(first + second, count_text(text), "split at {at}");
        }
        // Split between the CR and the LF of a CRLF line end.
        let crlf =
            format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\r\n\r\n");
        let at = crlf.find('\r').unwrap() + 1;
        let mut answer = AnswerText::default();
        let total = answer.push(&crlf.as_bytes()[..at]) + answer.push(&crlf.as_bytes()[at..]);
        assert_eq!(total, count_text(text));
    }

    #[test]
    fn usage_and_bookkeeping_events_add_nothing() {
        let chunks = [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-5-5\",\"usage\":{\"input_tokens\":25,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: ping\ndata: {\"type\": \"ping\"}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":15}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n",
            "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":10}}\n\n",
            "data: [DONE]\n\n",
            ": keep-alive comment\n\n",
            "data: not json\n\n",
        ];
        for chunk in chunks {
            assert_eq!(AnswerText::default().push(chunk.as_bytes()), 0, "{chunk}");
        }
    }

    #[tokio::test]
    async fn wait_returns_a_finished_tally_once() {
        let tallies = Tallies::new();
        let id = tallies.open();
        assert_eq!(id.len(), 32);
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        assert_ne!(tallies.open(), id, "every request gets its own ID");
        tallies.set_status(&id, 200);
        tallies.add(&id, 3);
        tallies.add(&id, 4);
        // Finished while the charge is already waiting.
        let waiting = {
            let tallies = tallies.clone();
            let id = id.clone();
            tokio::spawn(async move { tallies.wait(&id, Duration::from_secs(1)).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        tallies.finish(&id);
        assert_eq!(
            waiting.await.unwrap(),
            Some(Tally {
                status: Some(200),
                answer_o200k: 7,
                ..Tally::default()
            })
        );
        assert_eq!(tallies.wait(&id, Duration::from_millis(50)).await, None);
        assert_eq!(tallies.wait("unknown", Duration::from_secs(5)).await, None);

        // Finished before the charge asks.
        let early = tallies.open();
        tallies.finish(&early);
        assert_eq!(
            tallies.wait(&early, Duration::ZERO).await,
            Some(Tally {
                status: None,
                answer_o200k: 0,
                ..Tally::default()
            })
        );
        // A discarded tally is gone.
        let dropped = tallies.open();
        tallies.finish(&dropped);
        tallies.discard(&dropped);
        assert_eq!(tallies.wait(&dropped, Duration::ZERO).await, None);
    }

    #[tokio::test]
    async fn wait_times_out_without_finish() {
        let tallies = Tallies::new();
        let id = tallies.open();
        tallies.add(&id, 5);
        let started = Instant::now();
        assert_eq!(tallies.wait(&id, Duration::from_millis(50)).await, None);
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[tokio::test]
    async fn old_tallies_are_swept() {
        let start = Instant::now();
        let now = Arc::new(Mutex::new(start));
        let clock_now = Arc::clone(&now);
        let tallies = Tallies::with_clock(Arc::new(move || *clock_now.lock().unwrap()));
        let advance = |minutes: u64| *now.lock().unwrap() += Duration::from_secs(minutes * 60);

        let old = tallies.open();
        tallies.finish(&old);
        let busy = tallies.open(); // a long stream still relaying text
        advance(6);
        tallies.add(&busy, 1);
        advance(5); // 11 minutes since both opened
        let new = tallies.open();
        for id in [&busy, &new] {
            tallies.finish(id);
        }
        assert_eq!(tallies.wait(&old, Duration::ZERO).await, None, "swept");
        assert!(
            tallies.wait(&busy, Duration::ZERO).await.is_some(),
            "touched 5 minutes ago"
        );
        assert!(tallies.wait(&new, Duration::ZERO).await.is_some());
    }

    #[tokio::test]
    async fn start_records_once_and_replaces_the_usage() {
        let tallies = Tallies::new();
        assert_eq!(tallies.start("unknown", start(10)), Recorded::Unknown);
        let id = tallies.open();
        assert_eq!(tallies.start(&id, start(10)), Recorded::First);
        // A response-side call without the output count: its counts win.
        assert_eq!(tallies.start(&id, start(12)), Recorded::Again);
        tallies.finish(&id);
        let (recorded, _) = tallies
            .unreported(&id, Duration::ZERO, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(recorded, start(12));
    }

    #[tokio::test]
    async fn an_unreported_stream_is_taken_after_its_end_and_the_grace() {
        let tallies = Tallies::new();
        let id = tallies.open();
        tallies.start(&id, start(10));
        tallies.set_status(&id, 200);
        tallies.add(&id, 7);
        let watcher = {
            let (tallies, id) = (tallies.clone(), id.clone());
            tokio::spawn(async move {
                tallies
                    .unreported(&id, Duration::from_millis(100), Duration::from_secs(5))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!watcher.is_finished(), "waits for the stream's end");
        let ended = Instant::now();
        tallies.finish(&id);
        let (recorded, tally) = watcher.await.unwrap().expect("taken for an estimate");
        assert!(
            ended.elapsed() >= Duration::from_millis(100),
            "then the grace"
        );
        assert_eq!(recorded, start(10));
        assert_eq!(
            tally,
            Tally {
                status: Some(200),
                answer_o200k: 7,
                ..Tally::default()
            }
        );
        assert_eq!(tallies.wait(&id, Duration::ZERO).await, None, "taken");
        assert_eq!(
            tallies
                .unreported("unknown", Duration::ZERO, Duration::from_secs(5))
                .await,
            None
        );
    }

    #[tokio::test]
    async fn a_report_takes_the_tally_first() {
        let tallies = Tallies::new();
        // A report while the stream runs, and one in the grace after its end.
        for after_the_end in [false, true] {
            let id = tallies.open();
            tallies.start(&id, start(10));
            let watcher = {
                let (tallies, id) = (tallies.clone(), id.clone());
                tokio::spawn(async move {
                    tallies
                        .unreported(&id, Duration::from_millis(300), Duration::from_secs(5))
                        .await
                })
            };
            if after_the_end {
                tallies.finish(&id);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(tallies.reported(&id), "after the end: {after_the_end}");
            tallies.finish(&id); // the hook ends its stream later; nothing to finish
            assert_eq!(
                watcher.await.unwrap(),
                None,
                "after the end: {after_the_end}"
            );
        }
        // No tally here (another Sekisho's stream hook): the report is charged.
        assert!(tallies.reported("unknown"));
    }

    #[tokio::test]
    async fn an_estimate_refuses_a_late_report() {
        let tallies = Tallies::new();
        let id = tallies.open();
        tallies.start(&id, start(10));
        tallies.finish(&id);
        assert!(
            tallies
                .unreported(&id, Duration::ZERO, Duration::from_secs(1))
                .await
                .is_some()
        );
        assert!(!tallies.reported(&id), "already estimated");
        assert_eq!(tallies.start(&id, start(10)), Recorded::Estimated);
    }

    #[tokio::test]
    async fn a_stream_that_never_ends_is_taken_after_the_longest_wait() {
        let tallies = Tallies::new();
        let id = tallies.open();
        tallies.start(&id, start(10));
        tallies.add(&id, 3);
        let started = Instant::now();
        let (_, tally) = tallies
            .unreported(&id, Duration::from_secs(5), Duration::from_millis(200))
            .await
            .expect("taken after the longest wait");
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(200), "{waited:?}");
        assert!(
            waited < Duration::from_secs(5),
            "no grace after it: {waited:?}"
        );
        assert_eq!(tally.answer_o200k, 3);
        assert!(!tallies.reported(&id));
    }

    /// A long stream keeps its tally while it relays text, so agentgateway's
    /// report, when the provider's usage arrives, is charged.
    #[tokio::test]
    async fn a_stream_that_keeps_sending_is_not_cut_short() {
        let tallies = Tallies::new();
        let id = tallies.open();
        tallies.start(&id, start(10));
        let watcher = {
            let (tallies, id) = (tallies.clone(), id.clone());
            tokio::spawn(async move {
                tallies
                    .unreported(&id, Duration::from_millis(300), Duration::from_millis(200))
                    .await
            })
        };
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            tallies.add(&id, 1); // 400 ms of text, twice the quiet limit
        }
        assert!(
            !watcher.is_finished(),
            "a stream that changes isn't stalled"
        );
        assert!(tallies.reported(&id), "the provider's report is charged");
        assert_eq!(watcher.await.unwrap(), None);
    }

    /// The sweep drops idle tallies nobody watches, but keeps a watched
    /// tally until its watcher takes it, and an estimated request's mark
    /// for [`STALLED`], so a late report is never charged on top.
    #[tokio::test]
    async fn the_sweep_keeps_watched_tallies_and_estimates() {
        let now = Arc::new(Mutex::new(Instant::now()));
        let clock_now = Arc::clone(&now);
        let tallies = Tallies::with_clock(Arc::new(move || *clock_now.lock().unwrap()));
        let advance = |minutes: u64| *now.lock().unwrap() += Duration::from_secs(minutes * 60);

        let idle = tallies.open();
        let watched = tallies.open();
        tallies.start(&watched, start(10));
        let estimated = tallies.open();
        tallies.start(&estimated, start(10));
        tallies.finish(&estimated);
        assert!(
            tallies
                .unreported(&estimated, Duration::ZERO, STALLED)
                .await
                .is_some()
        );

        advance(11);
        tallies.open(); // sweeps
        assert_eq!(tallies.start(&idle, start(10)), Recorded::Unknown, "swept");
        assert_eq!(
            tallies.start(&watched, start(10)),
            Recorded::Again,
            "watched: kept"
        );
        assert!(!tallies.reported(&estimated), "the estimate's mark is kept");

        advance(60);
        tallies.open();
        assert!(
            tallies.reported(&estimated),
            "the mark expires after STALLED"
        );
    }

    #[test]
    fn each_format_yields_the_providers_counts() {
        let usage = |input, cache_read, cache_write| Usage {
            input,
            cache_read,
            cache_write,
            output: 0,
        };
        let cases: [(&[&str], ProviderUsage); 7] = [
            // Anthropic: the prompt at the start (uncached input plus cache
            // reads and writes), the output count in the last delta.
            (
                &[
                    r#"{"type":"message_start","message":{"id":"m","role":"assistant","content":[],"usage":{"input_tokens":5000,"cache_read_input_tokens":95000,"cache_creation_input_tokens":200,"output_tokens":1}}}"#,
                ],
                ProviderUsage {
                    prompt: Some(usage(100_200, 95_000, 200)),
                    output: None,
                    start_output: Some(1),
                },
            ),
            (
                &[
                    r#"{"type":"message_start","message":{"usage":{"input_tokens":30,"output_tokens":1}}}"#,
                    r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":57}}"#,
                ],
                ProviderUsage {
                    prompt: Some(usage(30, 0, 0)),
                    output: Some(57),
                    start_output: Some(1),
                },
            ),
            // A translated stream's empty start counts nothing.
            (
                &[
                    r#"{"type":"message_start","message":{"usage":{"input_tokens":0,"output_tokens":0}}}"#,
                ],
                ProviderUsage::default(),
            ),
            // OpenAI chat: the usage chunk at the end.
            (
                &[
                    r#"{"id":"c","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":20,"total_tokens":1020,"prompt_tokens_details":{"cached_tokens":600}}}"#,
                ],
                ProviderUsage {
                    prompt: Some(usage(1000, 600, 0)),
                    output: Some(20),
                    start_output: None,
                },
            ),
            // OpenAI Responses.
            (
                &[
                    r#"{"type":"response.completed","response":{"usage":{"input_tokens":900,"input_tokens_details":{"cached_tokens":100},"output_tokens":45}}}"#,
                ],
                ProviderUsage {
                    prompt: Some(usage(900, 100, 0)),
                    output: Some(45),
                    start_output: None,
                },
            ),
            // Gemini: the prompt in every chunk, the output once it finished.
            (
                &[
                    r#"{"candidates":[{"content":{"parts":[{"text":"a"}]}}],"usageMetadata":{"promptTokenCount":700,"cachedContentTokenCount":300,"candidatesTokenCount":1}}"#,
                ],
                ProviderUsage {
                    prompt: Some(usage(700, 300, 0)),
                    output: None,
                    start_output: None,
                },
            ),
            (
                &[
                    r#"{"candidates":[{"content":{"parts":[{"text":"b"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":700,"candidatesTokenCount":12,"thoughtsTokenCount":30}}"#,
                ],
                ProviderUsage {
                    prompt: Some(usage(700, 0, 0)),
                    output: Some(42),
                    start_output: None,
                },
            ),
        ];
        for (events, want) in cases {
            let mut answer = AnswerText::default();
            for json in events {
                answer.push(&event(json));
            }
            assert_eq!(answer.usage(), want, "{events:?}");
        }
    }

    /// A response-side call's counts (the provider's, from agentgateway)
    /// beat the request-side call's prompt count, whichever is handled last.
    #[tokio::test]
    async fn a_request_side_start_never_replaces_reported_counts() {
        let tallies = Tallies::new();
        let reported = Start {
            usage: Usage {
                input: 1_000,
                cache_read: 600,
                ..Usage::default()
            },
            from_response: true,
            ..start(0)
        };
        let id = tallies.open();
        assert_eq!(tallies.start(&id, reported.clone()), Recorded::First);
        assert_eq!(tallies.start(&id, start(8)), Recorded::Again);
        tallies.finish(&id);
        let (recorded, _) = tallies
            .unreported(&id, Duration::ZERO, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(recorded, reported);
    }

    /// A finished tally that no rate-limit call claimed (count_tokens, or a
    /// path that isn't a model call) goes within two minutes, not ten.
    #[test]
    fn finished_tallies_nobody_watches_go_soon() {
        let now = Arc::new(Mutex::new(Instant::now()));
        let clock_now = Arc::clone(&now);
        let tallies = Tallies::with_clock(Arc::new(move || *clock_now.lock().unwrap()));
        let advance = |minutes: u64| *now.lock().unwrap() += Duration::from_secs(minutes * 60);

        let unclaimed = tallies.open();
        tallies.finish(&unclaimed);
        let running = tallies.open(); // still streaming, nobody watching yet
        let watched = tallies.open();
        tallies.start(&watched, start(10));
        tallies.finish(&watched);
        advance(2);
        tallies.open(); // sweeps
        assert_eq!(tallies.start(&unclaimed, start(10)), Recorded::Unknown);
        assert_eq!(tallies.start(&running, start(10)), Recorded::First);
        assert_eq!(tallies.start(&watched, start(10)), Recorded::Again);
    }

    /// agentgateway reports Anthropic's `message_start` output count as the
    /// output when a stream ends without `message_delta` (a provider error
    /// mid-answer). Such a report is stale until the stream shows its final
    /// count.
    #[test]
    fn a_report_of_the_start_count_is_stale() {
        let tallies = Tallies::new();
        let id = tallies.open();
        assert!(!tallies.stale(&id, 1), "no start count seen");
        let mut usage = ProviderUsage {
            start_output: Some(1),
            ..ProviderUsage::default()
        };
        tallies.set_usage(&id, usage);
        assert!(tallies.stale(&id, 1));
        assert!(!tallies.stale(&id, 20), "a count of its own");
        usage.output = Some(1); // `message_delta`: one token after all
        tallies.set_usage(&id, usage);
        assert!(!tallies.stale(&id, 1));
        assert!(!tallies.stale("unknown", 1));
    }
}
