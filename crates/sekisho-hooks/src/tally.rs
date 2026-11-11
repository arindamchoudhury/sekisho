//! What the stream hook counted of each answer, kept until the charge for
//! that request reads it.
//!
//! The stream hook opens a tally when a request starts and hands its ID to
//! agentgateway as ext_proc dynamic metadata; the spend descriptors carry it
//! back as `extproc.sekisho_request`. The hook adds the tokens of the answer
//! text it relays to the client and finishes the tally when the stream ends,
//! however it ends. When a stream ended before the provider reported its
//! output count, the rate-limit service waits for the finished tally and
//! charges an estimate from it.
//!
//! Tallies live in this process: a request's stream hook and its charge must
//! reach the same Sekisho (plan 8 shares them between replicas).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use rand::TryRng;
use rand::rngs::SysRng;
use serde_json::Value;
use tokio::sync::Notify;

use crate::tokens::count_text;

/// A tally untouched for this long is dropped: no charge came for it, or its
/// stream hook died without finishing it. Measured from the last chunk, so a
/// long stream keeps its tally.
const KEEP: Duration = Duration::from_secs(10 * 60);

/// How often [`Tallies::open`] looks for tallies to drop.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// A line longer than this is not an answer event (those are a few hundred
/// bytes); it is skipped rather than kept in memory.
const MAX_LINE: usize = 1 << 20;

/// What the stream hook saw of one answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// The answer's HTTP status, once its headers arrived.
    pub status: Option<u16>,
    /// The `o200k` count of the answer text relayed to the client, before
    /// any model's scaling.
    pub answer_o200k: u64,
}

struct Entry {
    tally: Tally,
    finished: bool,
    touched: Instant,
}

struct Book {
    entries: HashMap<String, Entry>,
    swept: Instant,
}

struct Inner {
    book: Mutex<Book>,
    finished: Notify,
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
                swept: now,
            }),
            finished: Notify::new(),
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
            book.entries
                .retain(|_, e| now.saturating_duration_since(e.touched) < KEEP);
            book.swept = now;
        }
        book.entries.insert(
            id.clone(),
            Entry {
                tally: Tally::default(),
                finished: false,
                touched: now,
            },
        );
        id
    }

    /// Records the answer's HTTP status.
    pub fn set_status(&self, id: &str, status: u16) {
        self.touch(id, |e| e.tally.status = Some(status));
    }

    /// Adds `tokens` of relayed answer text.
    pub fn add(&self, id: &str, tokens: u64) {
        self.touch(id, |e| {
            e.tally.answer_o200k = e.tally.answer_o200k.saturating_add(tokens);
        });
    }

    /// Marks the tally complete: its stream has ended.
    pub fn finish(&self, id: &str) {
        self.touch(id, |e| e.finished = true);
        self.0.finished.notify_waiters();
    }

    /// Drops a tally nobody will read.
    pub fn discard(&self, id: &str) {
        self.book().entries.remove(id);
    }

    /// The tally once its stream has ended, removed so it is read only once.
    /// `None` for an unknown ID, or when the stream hasn't ended within
    /// `timeout`.
    pub async fn wait(&self, id: &str, timeout: Duration) -> Option<Tally> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // Registered before looking, so a finish in between still wakes us.
            let finished = self.0.finished.notified();
            tokio::pin!(finished);
            finished.as_mut().enable();
            {
                let mut book = self.book();
                let done = book.entries.get(id)?.finished;
                if done {
                    return book.entries.remove(id).map(|e| e.tally);
                }
            }
            tokio::time::timeout_at(deadline, finished).await.ok()?;
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
            tokens = tokens.saturating_add(line_tokens(line));
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
}

/// The tokens of one SSE line: a `data:` line holding an answer event.
fn line_tokens(line: &[u8]) -> u64 {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let Some(data) = line.strip_prefix(b"data:") else {
        return 0; // `event:`, comments, blank lines
    };
    let data = data.strip_prefix(b" ").unwrap_or(data);
    let Ok(event) = serde_json::from_slice::<Value>(data) else {
        return 0; // `[DONE]`
    };
    let mut tokens = 0u64;
    answer_text(&event, &mut |text| {
        tokens = tokens.saturating_add(count_text(text));
    });
    tokens
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
    // OpenAI chat: content, reasoning and tool arguments.
    for choice in event["choices"].as_array().into_iter().flatten() {
        let delta = &choice["delta"];
        for field in ["content", "reasoning_content"] {
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

    use super::*;
    use crate::tokens::count_text;

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
                answer_o200k: 7
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
                answer_o200k: 0
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
}
