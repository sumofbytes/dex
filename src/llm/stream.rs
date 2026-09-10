//! SSE stream readers. A shared driver owns framing, printing, cancellation,
//! and usage threading; one thin parser per wire protocol translates raw
//! `data:` lines into protocol-agnostic [`StreamEvent`]s. Protocol state
//! (tool-call merging, reasoning replay) stays parser-local, so the driver
//! is written once and a new protocol plugs in as another [`StreamParser`].

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, Write};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::core::console::with_console;
use crate::core::highlight::{print_code_block, print_markdown_text};
use crate::core::types::{
    ChatMessage, LlmToolCall, Role, SinkLine, StopReason, StreamChunk, StreamDelta, Usage,
};
use crate::llm::protocol::{merge_chat_tool_call, response_call_index, response_tool_call};

/// Incremental markdown printer: prose is flushed as soon as a full line
/// arrives; code fences are buffered until closed so they can be highlighted
/// as one block. If a fence is still open when the stream ends, it is
/// flushed as-is.
pub(crate) struct StreamPrinter {
    in_code: bool,
    code_lang: String,
    code_body: String,
    sink: Option<mpsc::Sender<SinkLine>>,
    /// Headless (no-sink) gap state so plain-terminal output follows the same
    /// blanks-around-blocks rule as the TUI (MD022/MD031/MD032/MD058).
    /// `prev` is the last printed prose line (trimmed-start); `air` mirrors
    /// `markdown_leaves_air`; `empty` avoids doubling blanks / leading blank.
    headless_prev: String,
    headless_air: bool,
    headless_empty: bool,
}

impl StreamPrinter {
    pub(crate) fn new(sink: Option<mpsc::Sender<SinkLine>>) -> Self {
        Self {
            in_code: false,
            code_lang: String::new(),
            code_body: String::new(),
            sink,
            headless_prev: String::new(),
            headless_air: false,
            headless_empty: true,
        }
    }

    /// Blank line needed before headless `line` (never doubled, never
    /// between tight-continuation rows like list items). Uses the shared
    /// rule (`core::markdown`) — the same one the TUI throttle normalizes
    /// with — so both renderers agree on air. Only called outside fences.
    fn headless_gap(&self, line: &str) -> bool {
        use crate::core::markdown as md;
        if line.trim().is_empty() {
            return false;
        }
        if md::is_continuation_lines(&self.headless_prev, line) {
            return false;
        }
        self.headless_air || md::needs_gap_before(self.headless_empty, line)
    }

    fn headless_note(&mut self, line: &str) {
        use crate::core::markdown as md;
        if line.trim().is_empty() {
            self.headless_empty = true;
            self.headless_air = false;
            self.headless_prev.clear();
        } else {
            let t = line.trim_start();
            self.headless_empty = false;
            self.headless_air = md::block_leaves_air(t, md::is_table_line(t));
            self.headless_prev = t.to_string();
        }
    }

    /// Headless bookkeeping shared by the sync/async fence paths: closing
    /// fence leaves air (MD031), opening fence gets air before it.
    fn headless_on_fence_close(&mut self) {
        self.headless_empty = false;
        self.headless_air = true;
        self.headless_prev = "```".to_string();
    }

    fn headless_on_fence_open(&mut self) {
        if self.sink.is_none() && !self.headless_empty {
            println!();
            self.headless_empty = true;
        }
    }

    /// Headless prose path shared by sync/async inners (both print sync).
    /// Blank runs collapse to one air row like the TUI: a blank line prints
    /// exactly one row, so repeats are skipped once `headless_empty` says the
    /// cursor is already on air (never doubled, never leading).
    fn headless_on_prose(&mut self, line: &str) {
        if line.trim().is_empty() && self.headless_empty {
            return;
        }
        if self.headless_gap(line) {
            println!();
        }
        print_markdown_text(line);
        self.headless_note(line);
    }

    /// Sync variant for the in-memory test driver only.
    #[cfg(test)]
    pub(crate) fn feed_line(&mut self, line: &str) {
        if self.sink.is_some() {
            // Sink mode: no spinner to erase; stream directly.
            self.feed_line_inner(line);
        } else {
            with_console(self.sink.is_some(), || self.feed_line_inner(line));
        }
    }

    /// Async variant for live network paths: back-pressured `send().await`
    /// instead of `try_send`, so a full channel applies backpressure rather
    /// than silently dropping transcript lines. The sync `feed_line` above
    /// stays for the in-memory test driver (channel never fills there).
    pub(crate) async fn feed_line_async(&mut self, line: &str) {
        if self.sink.is_some() {
            self.feed_line_inner_async(line).await;
        } else {
            with_console(self.sink.is_some(), || self.feed_line_inner(line));
        }
    }

    pub(crate) fn feed_line_inner(&mut self, line: &str) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if self.in_code {
                if let Some(sink) = &self.sink {
                    let _ = sink.try_send(SinkLine::Assistant(format!(
                        "```{}:\n{}\n```",
                        self.code_lang, self.code_body
                    )));
                } else {
                    print_code_block(&self.code_lang, &self.code_body);
                    self.headless_on_fence_close();
                }
                self.code_body.clear();
                self.code_lang.clear();
                self.in_code = false;
            } else {
                self.headless_on_fence_open();
                self.in_code = true;
                self.code_lang = trimmed.trim_start_matches('`').trim().to_string();
            }
        } else if self.in_code {
            self.code_body.push_str(line);
            self.code_body.push('\n');
        } else {
            if let Some(sink) = &self.sink {
                let _ = sink.try_send(SinkLine::Assistant(line.to_string()));
            } else {
                self.headless_on_prose(line);
            }
        }
    }

    async fn feed_line_inner_async(&mut self, line: &str) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if self.in_code {
                if let Some(sink) = &self.sink {
                    let _ = sink
                        .send(SinkLine::Assistant(format!(
                            "```{}:\n{}\n```",
                            self.code_lang, self.code_body
                        )))
                        .await;
                } else {
                    print_code_block(&self.code_lang, &self.code_body);
                    self.headless_on_fence_close();
                }
                self.code_body.clear();
                self.code_lang.clear();
                self.in_code = false;
            } else {
                self.headless_on_fence_open();
                self.in_code = true;
                self.code_lang = trimmed.trim_start_matches('`').trim().to_string();
            }
        } else if self.in_code {
            self.code_body.push_str(line);
            self.code_body.push('\n');
        } else if let Some(sink) = &self.sink {
            let _ = sink.send(SinkLine::Assistant(line.to_string())).await;
        } else {
            self.headless_on_prose(line);
        }
    }

    #[cfg(test)]
    pub(crate) fn finish(self) {
        if self.in_code && !self.code_body.is_empty() {
            if let Some(sink) = &self.sink {
                let _ = sink.try_send(SinkLine::Assistant(format!(
                    "```{}:\n{}\n```",
                    self.code_lang, self.code_body
                )));
            } else {
                with_console(self.sink.is_some(), || {
                    print_code_block(&self.code_lang, &self.code_body)
                });
            }
        }
    }

    pub(crate) async fn finish_async(self) {
        if self.in_code && !self.code_body.is_empty() {
            if let Some(sink) = &self.sink {
                let _ = sink
                    .send(SinkLine::Assistant(format!(
                        "```{}:\n{}\n```",
                        self.code_lang, self.code_body
                    )))
                    .await;
            } else {
                with_console(self.sink.is_some(), || {
                    print_code_block(&self.code_lang, &self.code_body)
                });
            }
        }
    }
}

/// Reasoning delta under the provider-specific chat-completions key, as a
/// string; non-string shapes (some providers send arrays) yield None.
pub(crate) fn delta_thought(delta: &StreamDelta) -> Option<&str> {
    delta
        .reasoning
        .as_ref()
        .and_then(Value::as_str)
        .or_else(|| delta.reasoning_content.as_ref().and_then(Value::as_str))
}

/// A failure raised after output has already been emitted to the user (text
/// or reasoning streamed live); a retried call would duplicate that partial
/// transcript. Purely a type-level marker: `Display` passes the inner message
/// through untouched so callers matching on `"cancelled"` etc. keep working.
#[derive(Debug)]
pub(crate) struct MidStreamError(pub String);

impl std::fmt::Display for MidStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MidStreamError {}

/// Wrap a stream failure as [`MidStreamError`] only once output has flowed;
/// a failure before any output is retryable (protocol fallback may re-run
/// the turn without duplicating anything).
fn stream_err(message: &str, output_flowed: bool) -> Box<dyn std::error::Error + Send + Sync> {
    if output_flowed {
        Box::new(MidStreamError(message.to_string()))
    } else {
        message.into()
    }
}

/// Terminal stream failure from a driver that owns a thinking line: close
/// the headless stderr thinking line first (a mid-stream error must never
/// leave the user's terminal stuck in DIM), then apply the usual
/// output-flowed retryability rule.
fn driver_err(driver: &mut SseDriver, message: &str) -> Box<dyn std::error::Error + Send + Sync> {
    driver.end_thinking();
    stream_err(message, driver.output_flowed)
}

/// Transport-failure variant of [`driver_err`]: pre-output the caller's
/// `chunk()` error is marked with [`StreamTransportError`] (provenance for
/// the retry gate) and otherwise passes through untouched; post-output it
/// wraps as [`MidStreamError`] like any other failure.
fn driver_err_transport(
    driver: &mut SseDriver,
    err: Box<dyn std::error::Error + Send + Sync>,
) -> Box<dyn std::error::Error + Send + Sync> {
    driver.end_thinking();
    if driver.output_flowed {
        Box::new(MidStreamError(err.to_string()))
    } else {
        Box::new(StreamTransportError(err))
    }
}

/// Mid-stream events a parser emits per SSE line; the driver owns what
/// happens to them (printing, accumulation, usage threading, lifecycle).
enum StreamEvent {
    /// Reasoning delta; UI-only, routed to the sink, never persisted.
    Thinking(String),
    /// Content delta; accumulated and printed live by the driver.
    Text(String),
    /// Provider-reported token usage (the last one wins).
    Usage(Usage),
    /// Terminal condition reported by the provider.
    Stop(StopReason),
    /// Protocol-level end of stream (chat-completions `[DONE]`, the
    /// Anthropic `message_stop`); the driver stops reading. Protocols that
    /// end at EOF (responses API) never emit it.
    Done,
    /// Provider-reported failure (e.g. Anthropic's terminal `error` event
    /// on a 200 body). Honors the same output-flowed rule as transport
    /// failures: before any output it stays retryable, after it becomes a
    /// [`MidStreamError`].
    Fail(String),
}

/// Parser-owned parts of the final assistant message; the driver owns
/// `content` and assembles the message itself.
struct ParsedMessage {
    tool_calls: Vec<LlmToolCall>,
    reasoning_items: Option<Vec<Value>>,
    reasoning_content: Option<String>,
}

/// One protocol parser, fed raw SSE lines. All protocol-specific state
/// (tool-call merging, reasoning replay) lives in the implementation.
trait StreamParser {
    fn feed(&mut self, line: &str) -> Vec<StreamEvent>;
    fn finish(self) -> ParsedMessage;
}

/// One model turn read off the wire: the assembled assistant message, the
/// last reported usage, and the provider-reported terminal condition (`None`
/// when the stream ended without one — mid-stream drop, or a provider that
/// omits `finish_reason`).
#[derive(Debug)]
pub(crate) struct Turn {
    pub(crate) message: ChatMessage,
    pub(crate) usage: Option<Usage>,
    pub(crate) stop_reason: Option<StopReason>,
}

/// Shared SSE driver state: feeds each line to the parser, prints
/// text/reasoning as it arrives, tracks usage and the terminal condition,
/// and assembles the final turn. Fails with [`MidStreamError`] once any
/// output has been emitted — a retry would duplicate it.
struct SseDriver {
    content: String,
    pending: String,
    printer: StreamPrinter,
    usage: Option<Usage>,
    stop_reason: Option<StopReason>,
    output_flowed: bool,
    /// Headless stderr thinking line is open (dim, no closing newline yet).
    thinking_open: bool,
}

impl SseDriver {
    fn new(sink: Option<mpsc::Sender<SinkLine>>) -> Self {
        Self {
            content: String::new(),
            pending: String::new(),
            printer: StreamPrinter::new(sink.clone()),
            usage: None,
            stop_reason: None,
            output_flowed: false,
            thinking_open: false,
        }
    }

    fn sink(&self) -> Option<&mpsc::Sender<SinkLine>> {
        self.printer.sink.as_ref()
    }

    /// Headless (no sink) thinking: the TUI renders reasoning live on the
    /// transcript, so pipe users get it dimmed on stderr instead — stdout
    /// stays model-prose-only. Fragments are deltas: write without a newline
    /// and close the line when a non-thinking event or turn end arrives.
    fn print_thinking(&mut self, thought: &str) {
        if thought.is_empty() {
            return;
        }
        if !self.thinking_open {
            self.thinking_open = true;
            let _ = io::stderr().write_all(crate::core::console::DIM.as_bytes());
        }
        let _ = io::stderr().write_all(thought.as_bytes());
    }

    fn end_thinking(&mut self) {
        if self.thinking_open {
            self.thinking_open = false;
            let _ = io::stderr().write_all(b"\x1b[0m\n");
            let _ = io::stderr().flush();
        }
    }

    /// Feed one raw SSE line; `Ok(true)` when the parser signalled Done.
    /// A parser `Fail` becomes `Err` honoring the output-flowed rule. Sync
    /// variant for the in-memory test driver (channel never fills). Live
    /// network paths use [`SseDriver::feed_raw_async`], which
    /// back-pressures with `send().await` instead of dropping on a full
    /// channel.
    #[cfg(test)]
    fn feed_raw(
        &mut self,
        line: &str,
        parser: &mut impl StreamParser,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        crate::log!(Trace, "sse {}", line.trim_end());
        let mut done = false;
        for event in parser.feed(line) {
            if !matches!(event, StreamEvent::Thinking(_)) {
                self.end_thinking();
            }
            match event {
                StreamEvent::Thinking(thought) => {
                    // Reasoning counts as flowed output: it renders live on the
                    // transcript, so a retry after a drop here would duplicate
                    // it, even though reasoning is never persisted to the
                    // session journal. Deliberate — don't narrow this to Text.
                    self.output_flowed = true;
                    if let Some(sink) = self.sink() {
                        let _ = sink.try_send(SinkLine::Thinking(thought));
                    } else {
                        self.print_thinking(&thought);
                    }
                }
                StreamEvent::Text(text) => {
                    self.output_flowed = true;
                    self.content.push_str(&text);
                    // Print complete lines live; keep any partial tail buffered.
                    self.pending.push_str(&text);
                    // Collect completed lines first to avoid borrow fights.
                    let mut completed: Vec<String> = Vec::new();
                    while let Some(pos) = self.pending.find('\n') {
                        let complete = self.pending[..=pos].to_string();
                        self.pending.replace_range(..=pos, "");
                        completed.push(complete);
                    }
                    for c in completed {
                        self.printer.feed_line(c.trim_end_matches('\n'));
                    }
                    if self.sink().is_none() {
                        let _ = io::stdout().flush();
                    }
                }
                StreamEvent::Usage(u) => self.usage = Some(u),
                StreamEvent::Stop(reason) => self.stop_reason = Some(reason),
                StreamEvent::Done => done = true,
                StreamEvent::Fail(message) => {
                    return Err(stream_err(&message, self.output_flowed));
                }
            }
        }
        Ok(done)
    }

    /// Async variant for live network paths: sink sends back-pressure with
    /// `send().await` so transcript lines are never dropped on a full
    /// channel. Same `Fail` → `Err` contract as [`SseDriver::feed_raw`].
    async fn feed_raw_async(
        &mut self,
        line: &str,
        parser: &mut impl StreamParser,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        crate::log!(Trace, "sse {}", line.trim_end());
        let mut done = false;
        for event in parser.feed(line) {
            if !matches!(event, StreamEvent::Thinking(_)) {
                self.end_thinking();
            }
            match event {
                StreamEvent::Thinking(thought) => {
                    // Same output_flowed contract as `feed_raw`: reasoning
                    // renders live, so a retry after a drop would duplicate it.
                    self.output_flowed = true;
                    if let Some(sink) = self.sink() {
                        let _ = sink.send(SinkLine::Thinking(thought)).await;
                    } else {
                        self.print_thinking(&thought);
                    }
                }
                StreamEvent::Text(text) => {
                    self.output_flowed = true;
                    self.content.push_str(&text);
                    // Print complete lines live; keep any partial tail buffered.
                    self.pending.push_str(&text);
                    // Collect completed lines first to avoid borrow fights.
                    let mut completed: Vec<String> = Vec::new();
                    while let Some(pos) = self.pending.find('\n') {
                        let complete = self.pending[..=pos].to_string();
                        self.pending.replace_range(..=pos, "");
                        completed.push(complete);
                    }
                    for c in completed {
                        self.printer.feed_line_async(c.trim_end_matches('\n')).await;
                    }
                    if self.sink().is_none() {
                        let _ = io::stdout().flush();
                    }
                }
                StreamEvent::Usage(u) => self.usage = Some(u),
                StreamEvent::Stop(reason) => self.stop_reason = Some(reason),
                StreamEvent::Done => done = true,
                StreamEvent::Fail(message) => {
                    return Err(stream_err(&message, self.output_flowed));
                }
            }
        }
        Ok(done)
    }

    #[cfg(test)]
    fn finish_turn(
        mut self,
        parser: impl StreamParser,
        sink_is_some: bool,
    ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
        if !self.pending.is_empty() {
            let tail = std::mem::take(&mut self.pending);
            self.printer.feed_line(&tail);
            let _ = io::stdout().flush();
        }
        let printer = std::mem::replace(&mut self.printer, StreamPrinter::new(None));
        printer.finish();
        self.finish_turn_tail(parser, sink_is_some)
    }

    async fn finish_turn_async(
        mut self,
        parser: impl StreamParser,
        sink_is_some: bool,
    ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
        if !self.pending.is_empty() {
            let tail = std::mem::take(&mut self.pending);
            self.printer.feed_line_async(&tail).await;
            let _ = io::stdout().flush();
        }
        let printer = std::mem::replace(&mut self.printer, StreamPrinter::new(None));
        printer.finish_async().await;
        self.finish_turn_tail(parser, sink_is_some)
    }

    fn finish_turn_tail(
        mut self,
        parser: impl StreamParser,
        sink_is_some: bool,
    ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
        self.end_thinking();
        let _ = io::stdout().flush();
        if !self.content.is_empty() {
            with_console(sink_is_some, || println!());
            let _ = io::stdout().flush();
        }
        let parsed = parser.finish();
        Ok(Turn {
            message: ChatMessage {
                role: Role::Assistant,
                content: (!self.content.is_empty()).then_some(self.content),
                tool_calls: (!parsed.tool_calls.is_empty()).then_some(parsed.tool_calls),
                tool_call_id: None,
                name: None,
                reasoning_items: parsed.reasoning_items,
                reasoning_content: parsed.reasoning_content,
            },
            usage: self.usage,
            stop_reason: self.stop_reason,
        })
    }
}

/// Idle cap between SSE chunks. Providers stall (silent proxies, hung
/// upstreams); without a watchdog the stream reader waits forever and the
/// turn never terminates. Every chunk (including keep-alives) resets the
/// timer. `DEX_STREAM_IDLE_TIMEOUT_SECS` overrides; `0` disables the
/// watchdog entirely (no timer armed).
///
/// The default is per-model ([`stream_idle_timeout_for`]): reasoning-capable
/// models buffer for minutes without emitting a chunk (300s), fast models
/// fail fast (90s) so a real stall surfaces instead of hanging. A stall past
/// the budget is retried automatically by the caller (same protocol,
/// bounded) before it ever fails the turn.
pub(crate) const DEFAULT_STREAM_IDLE_TIMEOUT_SECS: u64 = 90;
pub(crate) const REASONING_STREAM_IDLE_TIMEOUT_SECS: u64 = 300;

pub(crate) fn is_stream_idle_error(message: &str) -> bool {
    message.contains("stream idle for over")
}

/// Transport deaths that say nothing about the request: a middlebox or dead
/// peer dropped the socket mid-body (surfaced by `chunk()` once TCP
/// keepalives stop being ACKed). Pre-output these are pure re-issues —
/// nothing flowed, nothing to duplicate — and the protocol-fallback gate
/// excludes them too, so a dead socket is never learned as a mismatch.
pub(crate) fn is_dropped_connection(message: &str) -> bool {
    [
        "connection closed before message completed",
        "connection reset",
        "connection aborted",
        "broken pipe",
    ]
    .iter()
    .any(|s| message.contains(s))
}

/// Provenance marker: this error came out of `response.chunk()` — the head
/// was accepted and the body then proved unreadable (dropped socket, reset,
/// truncated encoding). `run_sse` is the only constructor, so presence in
/// the chain means transport death by construction: no wording or
/// `reqwest::Error`-kind matching (both shift across reqwest versions — the
/// same mid-body FIN reads as `is_decode` on one, `is_body` on another).
/// `Display` forwards to the inner error so logs and retry notices read
/// unchanged.
#[derive(Debug)]
pub(crate) struct StreamTransportError(pub Box<dyn std::error::Error + Send + Sync>);

impl std::fmt::Display for StreamTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for StreamTransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.0)
    }
}

/// Transport-death check for retry gates: a [`StreamTransportError`] in the
/// chain means `run_sse` itself saw the body die — matched by provenance,
/// never by wording, so provider error text can never trip this gate.
pub(crate) fn is_transport_error(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = source {
        if e.downcast_ref::<StreamTransportError>().is_some() {
            return true;
        }
        source = std::error::Error::source(e);
    }
    false
}

/// Idle budget for one stream: the explicit env override wins, `0`
/// disables; otherwise reasoning-capable models (thinking enabled for this
/// call, or the models.dev catalog advertises effort options for the model)
/// get the patient budget and fast models fail fast.
pub(crate) fn stream_idle_timeout_for(model: &str, thinking: bool) -> Option<Duration> {
    match env_secs("DEX_STREAM_IDLE_TIMEOUT_SECS") {
        Some(0) => None,
        Some(secs) => Some(Duration::from_secs(secs)),
        None => Some(Duration::from_secs(if model_reasons(model, thinking) {
            REASONING_STREAM_IDLE_TIMEOUT_SECS
        } else {
            DEFAULT_STREAM_IDLE_TIMEOUT_SECS
        })),
    }
}

fn model_reasons(model: &str, thinking: bool) -> bool {
    thinking || crate::llm::config::reasoning_options_for(model).is_some()
}

fn env_secs(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
}

/// Shared async SSE driver over `response.chunk()`: buffer bytes,
/// split on `\n`, feed the existing `StreamParser`s unchanged (pure
/// functions over `&str`). Cancel via `select!(cancelled(),
/// chunk = response.chunk())` — a stalled chunk no longer stalls cancel.
/// The idle budget is caller-computed per model
/// ([`stream_idle_timeout_for`]); `None` arms no timer. Preserves
/// `MidStreamError` semantics exactly.
async fn run_sse<P: StreamParser>(
    mut response: reqwest::Response,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    idle_timeout: Option<Duration>,
    mut parser: P,
) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
    let sink_is_some = sink.is_some();
    // Fast-path: pre-cancelled token unwinds before reading anything,
    // preserving cancel-before-first-delta fallback semantics.
    if cancel.is_cancelled() {
        with_console(sink_is_some, || println!());
        let _ = io::stdout().flush();
        return Err(stream_err("cancelled", false));
    }
    let mut driver = SseDriver::new(sink.clone());
    let mut buf: Vec<u8> = Vec::new();
    // Pin the cancel future once; `cancelled()` loops until set, so a
    // single future covers the whole stream.
    let cancel_fut = cancel_cancelled(cancel);
    tokio::pin!(cancel_fut);
    loop {
        tokio::select! {
            _ = &mut cancel_fut => {
                with_console(sink_is_some, || println!());
                let _ = io::stdout().flush();
                return Err(driver_err(&mut driver, "cancelled"));
            }
            // Idle watchdog: every chunk (including keep-alives) resets this
            // timer, so it only fires when the provider truly stopped
            // sending. Without it a stalled stream parks the turn forever.
            // Disabled (`None`) arms no timer at all — no overflow-prone
            // infinite deadline.
            // Transport errors keep their type *and* provenance: a `chunk()`
            // failure is marked with `StreamTransportError` pre-output, so
            // the retry gate matches the marker instead of message wording
            // or `reqwest::Error` kind (both shift across reqwest
            // versions). Only the watchdog timeout — which has no source
            // error — travels as a plain message.
            chunk_res = async {
                match idle_timeout {
                    Some(t) => match tokio::time::timeout(t, response.chunk()).await {
                        Err(_) => Err(None),
                        Ok(r) => r.map_err(|e| {
                            Some(
                                Box::new(e)
                                    as Box<dyn std::error::Error + Send + Sync>,
                            )
                        }),
                    },
                    None => response.chunk().await.map_err(|e| {
                        Some(Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                    }),
                }
            } => {
                match chunk_res {
                    Err(None) => {
                        let secs = idle_timeout.map(|t| t.as_secs()).unwrap_or(0);
                        return Err(driver_err(
                            &mut driver,
                            &format!(
                                "stream idle for over {secs}s; the provider stalled or the connection dropped (tune with DEX_STREAM_IDLE_TIMEOUT_SECS)"
                            ),
                        ));
                    }
                    Err(Some(err)) => {
                        return Err(driver_err_transport(&mut driver, err));
                    }
                    Ok(None) => break,
                    Ok(Some(bytes)) => {
                        buf.extend_from_slice(&bytes);
                        // Extract complete lines; keep partial tail buffered.
                        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                            // Borrow the line before draining: skips a
                            // throwaway byte Vec per SSE line.
                            let line = String::from_utf8_lossy(&buf[..=pos]).into_owned();
                            buf.drain(..=pos);
                            if driver.feed_raw_async(&line, &mut parser).await? {
                                // Chat-completions [DONE] / Anthropic
                                // message_stop: stop reading.
                                let sink_is_some = sink.is_some();
                                return driver.finish_turn_async(parser, sink_is_some).await;
                            }
                        }
                    }
                }
            }
        }
        // Cooperative preemption: a synchronous `take_cancelled` consumer
        // (tests) still observes promptly between chunks.
        if cancel.is_cancelled() {
            with_console(sink_is_some, || println!());
            let _ = io::stdout().flush();
            return Err(driver_err(&mut driver, "cancelled"));
        }
    }
    // EOF: flush trailing partial line (responses API ends at EOF).
    if !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf).into_owned();
        // Feed as one final line even without trailing newline.
        driver.feed_raw_async(&line, &mut parser).await?;
    }
    let sink_is_some = sink.is_some();
    driver.finish_turn_async(parser, sink_is_some).await
}

/// Poll-based cancel wait that stays `Send`: the sync trait offers no
/// future, so poll `is_cancelled()` with async sleep (~10ms granularity)
/// instead of holding a non-`Send` borrow. One path covers every
/// `CancellationSource` (`CancellationToken`, `GlobalCancellation`, test
/// doubles) — deliberately not the concrete `Notify`, whose future would
/// need a separate signature for zero user-visible gain.
async fn cancel_cancelled(cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync)) {
    // Poll with async sleep for ~10ms granularity without holding a
    // non-Send future.
    loop {
        if cancel.is_cancelled() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Test-only driver over in-memory lines (no HTTP): parser tests need no
/// server, per plan Phase 1.
#[cfg(test)]
fn run_sse_lines<P: StreamParser>(
    lines: &[&str],
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    mut parser: P,
) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
    let sink_is_some = sink.is_some();
    if cancel.is_cancelled() {
        return Err(stream_err("cancelled", false));
    }
    let mut driver = SseDriver::new(sink.clone());
    for line in lines {
        if cancel.is_cancelled() {
            return Err(driver_err(&mut driver, "cancelled"));
        }
        // Re-add newline: `feed` expects raw SSE lines.
        let owned = format!("{line}\n");
        if driver.feed_raw(&owned, &mut parser)? {
            break;
        }
    }
    driver.finish_turn(parser, sink_is_some)
}

/// Read a chat-completions SSE body into a turn.
pub(crate) async fn read_stream(
    response: reqwest::Response,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    idle_timeout: Option<Duration>,
) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
    run_sse(
        response,
        sink,
        cancel,
        idle_timeout,
        ChatCompletionsParser::default(),
    )
    .await
}

/// Read a responses-API SSE body into a turn.
pub(crate) async fn read_responses_stream(
    response: reqwest::Response,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    idle_timeout: Option<Duration>,
) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
    run_sse(
        response,
        sink,
        cancel,
        idle_timeout,
        ResponsesParser::default(),
    )
    .await
}

/// Read an Anthropic Messages SSE body into a turn.
pub(crate) async fn read_anthropic_stream(
    response: reqwest::Response,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    idle_timeout: Option<Duration>,
) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
    run_sse(
        response,
        sink,
        cancel,
        idle_timeout,
        AnthropicParser::default(),
    )
    .await
}

fn stop_reason_from_finish(finish: &str) -> Option<StopReason> {
    match finish {
        "stop" => Some(StopReason::Stop),
        "length" => Some(StopReason::Length),
        "tool_calls" | "function_call" => Some(StopReason::ToolUse),
        "content_filter" => Some(StopReason::ContentFilter),
        // Unrecognized provider reason — leave unset rather than guess.
        _ => None,
    }
}

/// `POST /chat/completions` stream: JSON chunks under `data: ` (note the
/// space), terminated by `[DONE]`. Tool-call deltas merge by `index`.
#[derive(Default)]
struct ChatCompletionsParser {
    tool_calls: Vec<LlmToolCall>,
    /// DeepSeek-style reasoning text, replayed on the assistant message.
    reasoning: String,
}

impl StreamParser for ChatCompletionsParser {
    fn feed(&mut self, line: &str) -> Vec<StreamEvent> {
        let Some(data) = line.strip_prefix("data: ") else {
            return Vec::new();
        };
        let data = data.trim();
        if data == "[DONE]" {
            return vec![StreamEvent::Done];
        }
        // Providers interleave non-chunk payloads (keep-alives, error
        // notices); skipping one unshapely line beats aborting a
        // multi-minute generation.
        let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        if let Some(usage) = &chunk.usage {
            events.push(StreamEvent::Usage(Usage {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                cached_tokens: usage.prompt_details.as_ref().map(|d| d.cached_tokens),
            }));
        }
        for choice in chunk.choices {
            if let Some(text) = delta_thought(&choice.delta) {
                events.push(StreamEvent::Thinking(text.to_string()));
            }
            // DeepSeek-style reasoning text: captured for replay on the
            // assistant message so the provider gets its own reasoning
            // thread back next request (self-gating: only set when the
            // provider streamed this exact field).
            if let Some(text) = choice
                .delta
                .reasoning_content
                .as_ref()
                .and_then(Value::as_str)
            {
                self.reasoning.push_str(text);
            }
            if let Some(text) = choice.delta.content {
                events.push(StreamEvent::Text(text));
            }
            for delta in choice.delta.tool_calls.unwrap_or_default() {
                merge_chat_tool_call(&mut self.tool_calls, delta);
            }
            if let Some(reason) = &choice.finish_reason {
                if let Some(stop) = stop_reason_from_finish(reason) {
                    events.push(StreamEvent::Stop(stop));
                }
            }
        }
        events
    }

    fn finish(self) -> ParsedMessage {
        ParsedMessage {
            tool_calls: self.tool_calls,
            reasoning_items: None,
            reasoning_content: (!self.reasoning.is_empty()).then_some(self.reasoning),
        }
    }
}

/// `POST /responses` stream: typed JSON events under `data:`; the body ends
/// at EOF. Tool calls arrive as items (added/done) plus argument deltas.
#[derive(Default)]
struct ResponsesParser {
    tool_calls: Vec<LlmToolCall>,
    response_items: HashMap<String, usize>,
    pending_arguments: HashMap<String, String>,
    reasoning_items: Vec<Value>,
}

impl ResponsesParser {
    /// Usage + terminal condition from a completed/incomplete response event.
    fn response_terminal(&self, response: &Value, events: &mut Vec<StreamEvent>) {
        if let Some(usage) = response.get("usage") {
            if let Some(input) = usage.get("input_tokens").and_then(Value::as_u64) {
                events.push(StreamEvent::Usage(Usage {
                    prompt_tokens: input,
                    completion_tokens: usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    cached_tokens: usage
                        .pointer("/input_tokens_details/cached_tokens")
                        .and_then(Value::as_u64),
                }));
            }
        }
        // `response.completed` (no incomplete details) is a clean stop;
        // `response.incomplete` reports why the reply was cut off. An
        // unrecognized reason stays unset rather than claiming a clean stop.
        let stop = match response
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
        {
            Some("max_output_tokens") => Some(StopReason::Length),
            Some("content_filter") => Some(StopReason::ContentFilter),
            Some(_) => None,
            None => Some(StopReason::Stop),
        };
        if let Some(stop) = stop {
            events.push(StreamEvent::Stop(stop));
        }
    }
}

impl StreamParser for ResponsesParser {
    fn feed(&mut self, line: &str) -> Vec<StreamEvent> {
        let Some(data) = line.strip_prefix("data:") else {
            return Vec::new();
        };
        let data = data.trim();
        if data == "[DONE]" || data.is_empty() {
            return Vec::new();
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut events = Vec::new();
        match event_type {
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    events.push(StreamEvent::Thinking(delta.to_string()));
                }
            }
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    events.push(StreamEvent::Text(delta.to_string()));
                }
            }
            "response.output_item.added" => {
                if event.pointer("/item/type").and_then(Value::as_str) == Some("function_call") {
                    let item = event.get("item").unwrap_or(&Value::Null);
                    let index = response_call_index(
                        &self.tool_calls,
                        event
                            .get("output_index")
                            .and_then(Value::as_u64)
                            .unwrap_or(self.tool_calls.len() as u64)
                            as usize,
                        item,
                    );
                    response_tool_call(&mut self.tool_calls, index, item);
                    if let Some(item_id) = item.get("id").and_then(Value::as_str) {
                        self.response_items.insert(item_id.to_string(), index);
                        if let Some(arguments) = self.pending_arguments.remove(item_id) {
                            if let Some(call) = self.tool_calls.get_mut(index) {
                                call.function.arguments.push_str(&arguments);
                            }
                        }
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    let key = event
                        .get("item_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            format!(
                                "output:{}",
                                event
                                    .get("output_index")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(0)
                            )
                        });
                    if let Some(index) = event
                        .get("item_id")
                        .and_then(Value::as_str)
                        .and_then(|id| self.response_items.get(id).copied())
                    {
                        if let Some(call) = self.tool_calls.get_mut(index) {
                            call.function.arguments.push_str(delta);
                        }
                    } else {
                        self.pending_arguments
                            .entry(key)
                            .or_default()
                            .push_str(delta);
                    }
                }
            }
            "response.output_item.done" => {
                if event.pointer("/item/type").and_then(Value::as_str) == Some("function_call") {
                    let item = event.get("item").unwrap_or(&Value::Null);
                    let index = response_call_index(
                        &self.tool_calls,
                        event
                            .get("output_index")
                            .and_then(Value::as_u64)
                            .unwrap_or(self.tool_calls.len() as u64)
                            as usize,
                        item,
                    );
                    response_tool_call(&mut self.tool_calls, index, item);
                    if let Some(item_id) = item.get("id").and_then(Value::as_str) {
                        self.response_items.insert(item_id.to_string(), index);
                        if let Some(arguments) = self.pending_arguments.remove(item_id) {
                            if let Some(call) = self.tool_calls.get_mut(index) {
                                call.function.arguments.push_str(&arguments);
                            }
                        }
                    }
                } else if event.pointer("/item/type").and_then(Value::as_str) == Some("reasoning") {
                    // Keep raw reasoning items for stateless replay next
                    // request; summary-only items (no encrypted_content)
                    // can't be replayed and would corrupt the thread.
                    let item = event.get("item").unwrap_or(&Value::Null);
                    if item.get("encrypted_content").is_some_and(|v| {
                        !v.is_null() && v.as_str().map(|s| !s.is_empty()).unwrap_or(true)
                    }) {
                        self.reasoning_items.push(item.clone());
                    }
                }
            }
            "response.completed" | "response.done" | "response.incomplete" => {
                if let Some(response) = event.get("response") {
                    self.response_terminal(response, &mut events);
                }
            }
            _ => {}
        }
        events
    }

    fn finish(self) -> ParsedMessage {
        // Completed calls without an id must be dropped, not executed.
        let mut tool_calls = self.tool_calls;
        tool_calls.retain(|call| !call.id.is_empty() && !call.function.name.is_empty());
        ParsedMessage {
            tool_calls,
            reasoning_items: (!self.reasoning_items.is_empty()).then_some(self.reasoning_items),
            reasoning_content: None,
        }
    }
}

fn stop_reason_from_anthropic(stop: &str) -> Option<StopReason> {
    match stop {
        "end_turn" | "stop_sequence" => Some(StopReason::Stop),
        "max_tokens" | "model_context_window" => Some(StopReason::Length),
        "tool_use" => Some(StopReason::ToolUse),
        "refusal" => Some(StopReason::ContentFilter),
        // `pause_turn` suspends a server-tool turn for continuation; dex
        // drives its own turns, so a paused stream is just a finished one.
        "pause_turn" => Some(StopReason::Stop),
        // Unrecognized provider reason — leave unset rather than guess.
        _ => None,
    }
}

/// One indexed content block under construction (text / thinking /
/// tool_use). Anthropic streams per-block deltas keyed by `index`, so each
/// block accumulates its own state until `content_block_stop`.
#[derive(Default)]
struct AnthropicBlock {
    kind: String,
    id: String,
    name: String,
    /// `input_json_delta` fragments for tool_use blocks.
    json: String,
    /// thinking_delta accumulation (replayed for signed thinking blocks).
    text: String,
    /// signature_delta accumulation (required to replay thinking).
    signature: String,
    /// Complete block captured at start for types with no deltas
    /// (`redacted_thinking` arrives whole).
    raw: Option<Value>,
}

/// `POST /v1/messages` stream (Anthropic Messages wire): typed events under
/// `data:`, body ends with `message_stop` (or EOF). Usage arrives split —
/// prompt-side counts on `message_start`, cumulative output tokens on
/// `message_delta` — so the parser accumulates and emits one complete
/// [`Usage`] per update instead of letting the last event clobber the rest.
#[derive(Default)]
struct AnthropicParser {
    blocks: HashMap<u64, AnthropicBlock>,
    tool_calls: Vec<LlmToolCall>,
    /// Signature-carrying thinking blocks (+ whole redacted_thinking
    /// blocks) for stateless replay on the next request.
    reasoning_items: Vec<Value>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: Option<u64>,
    cache_creation: u64,
}

impl AnthropicParser {
    fn block(&mut self, event: &Value) -> Option<&mut AnthropicBlock> {
        let index = event.get("index").and_then(Value::as_u64)?;
        self.blocks.get_mut(&index)
    }

    /// Merge one usage object (`message_start` / `message_delta` shape):
    /// every field is optional and only updates when present.
    fn absorb_usage(&mut self, usage: &Value) {
        if let Some(v) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.input_tokens = v;
        }
        if let Some(v) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.output_tokens = v;
        }
        if let Some(v) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.cache_read = Some(v);
        }
        if let Some(v) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
        {
            self.cache_creation = v;
        }
    }

    /// Prompt side counts every token that occupies context: uncached
    /// input, cache reads, and cache writes; cache reads are surfaced
    /// separately for cost discounting.
    fn current_usage(&self) -> Usage {
        Usage {
            prompt_tokens: self.input_tokens + self.cache_read.unwrap_or(0) + self.cache_creation,
            completion_tokens: self.output_tokens,
            cached_tokens: self.cache_read,
        }
    }
}

impl StreamParser for AnthropicParser {
    fn feed(&mut self, line: &str) -> Vec<StreamEvent> {
        let Some(data) = line.strip_prefix("data:") else {
            return Vec::new();
        };
        let data = data.trim();
        if data.is_empty() {
            return Vec::new();
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut events = Vec::new();
        match event_type {
            // Terminal failure on a 200 body (overloaded_error, …): the
            // driver turns this into a real error, retryable only before
            // any output flowed.
            "error" => {
                let kind = event
                    .pointer("/error/type")
                    .and_then(Value::as_str)
                    .unwrap_or("api_error");
                let message = event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown provider error");
                events.push(StreamEvent::Fail(format!("{kind}: {message}")));
            }
            "message_start" => {
                if let Some(usage) = event.pointer("/message/usage") {
                    self.absorb_usage(usage);
                }
            }
            "content_block_start" => {
                let block = event.get("content_block").cloned().unwrap_or(Value::Null);
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let kind = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.blocks.insert(
                    index,
                    AnthropicBlock {
                        id: block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        raw: (kind == "redacted_thinking").then_some(block),
                        kind,
                        ..Default::default()
                    },
                );
            }
            "content_block_delta" => {
                let delta = event.get("delta").cloned().unwrap_or(Value::Null);
                match delta
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "text_delta" => {
                        // Text flows to the caller via StreamEvent::Text;
                        // blocks only accumulate state that gets replayed
                        // (thinking), so nothing to keep here.
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            events.push(StreamEvent::Text(text.to_string()));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                            events.push(StreamEvent::Thinking(text.to_string()));
                            if let Some(block) = self.block(&event) {
                                block.text.push_str(text);
                            }
                        }
                    }
                    "signature_delta" => {
                        if let Some(sig) = delta.get("signature").and_then(Value::as_str) {
                            if let Some(block) = self.block(&event) {
                                block.signature.push_str(sig);
                            }
                        }
                    }
                    "input_json_delta" => {
                        if let Some(fragment) = delta.get("partial_json").and_then(Value::as_str) {
                            if let Some(block) = self.block(&event) {
                                block.json.push_str(fragment);
                            }
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(block) = self.blocks.remove(&index) {
                    match block.kind.as_str() {
                        "tool_use" => {
                            // Completed calls without an id/name must be
                            // dropped, not executed (responses-parser rule).
                            if !block.id.is_empty() && !block.name.is_empty() {
                                self.tool_calls.push(LlmToolCall {
                                    id: block.id,
                                    call_type: "function".to_string(),
                                    function: crate::core::types::FunctionCall {
                                        name: block.name,
                                        arguments: if block.json.is_empty() {
                                            "{}".to_string()
                                        } else {
                                            block.json
                                        },
                                    },
                                });
                            }
                        }
                        // Only signature-carrying thinking blocks replay;
                        // unsigned ones would corrupt the thread.
                        "thinking" => {
                            if !block.signature.is_empty() {
                                self.reasoning_items.push(json!({
                                    "type": "thinking",
                                    "thinking": block.text,
                                    "signature": block.signature,
                                }));
                            }
                        }
                        "redacted_thinking" => {
                            if let Some(raw) = block.raw {
                                self.reasoning_items.push(raw);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "message_delta" => {
                if let Some(usage) = event.get("usage") {
                    self.absorb_usage(usage);
                    events.push(StreamEvent::Usage(self.current_usage()));
                }
                if let Some(stop) = event
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .and_then(stop_reason_from_anthropic)
                {
                    events.push(StreamEvent::Stop(stop));
                }
            }
            "message_stop" => events.push(StreamEvent::Done),
            // `ping` keep-alives and anything unrecognized are noise.
            _ => {}
        }
        events
    }

    fn finish(self) -> ParsedMessage {
        // Completed calls without an id must be dropped, not executed.
        let mut tool_calls = self.tool_calls;
        tool_calls.retain(|call| !call.id.is_empty() && !call.function.name.is_empty());
        ParsedMessage {
            tool_calls,
            reasoning_items: (!self.reasoning_items.is_empty()).then_some(self.reasoning_items),
            reasoning_content: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        delta_thought, driver_err, is_dropped_connection, is_transport_error, read_stream,
        stream_err, stream_idle_timeout_for, SinkLine, SseDriver, StreamDelta, StreamPrinter,
        Usage,
    };
    use crate::core::console::CancellationToken;
    use crate::core::types::{StopReason, StreamUsage};
    use crate::llm::streaming::is_mid_stream;
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// The compaction summarizer (and any in-process caller) must be able to
    /// pass a sink and have ALL streamed output routed into the channel —
    /// never printed raw to stdout, which inside the TUI process is the
    /// alternate screen (ghost text until resize). With sink=None the raw
    /// print is intentional (plain one-shot CLI streaming); callers running
    /// beside a TUI must always pass a sink.
    #[test]
    fn stream_printer_with_sink_routes_lines_to_channel_not_stdout() {
        let (tx, mut rx) = mpsc::channel(32);
        let mut printer = StreamPrinter::new(Some(tx));
        printer.feed_line("Key facts: internal summary line");
        printer.feed_line("```rust");
        printer.feed_line("fn main() {}");
        printer.feed_line("```");
        printer.finish();

        let lines: Vec<SinkLine> = {
            let mut out = Vec::new();
            while let Ok(v) = rx.try_recv() {
                out.push(v);
            }
            out
        };
        assert!(lines
            .iter()
            .any(|l| matches!(l, SinkLine::Assistant(s) if s.contains("Key facts"))));
        assert!(lines
            .iter()
            .any(|l| matches!(l, SinkLine::Assistant(s) if s.contains("fn main"))));
    }

    #[test]
    fn reasoning_deltas_extract_from_provider_specific_fields() {
        let deepseek: StreamDelta =
            serde_json::from_str(r#"{"reasoning_content":"step 1"}"#).unwrap();
        assert_eq!(delta_thought(&deepseek), Some("step 1"));
        let openrouter: StreamDelta = serde_json::from_str(r#"{"reasoning":"step 2"}"#).unwrap();
        assert_eq!(delta_thought(&openrouter), Some("step 2"));
        // Non-string shapes must not kill the chunk parse or yield text.
        let array: StreamDelta = serde_json::from_str(r#"{"reasoning":[{"a":1}]}"#).unwrap();
        assert_eq!(delta_thought(&array), None);
        let plain: StreamDelta = serde_json::from_str(r#"{"content":"hi"}"#).unwrap();
        assert_eq!(delta_thought(&plain), None);
    }

    /// Chat-completions nests cached tokens under `prompt_tokens_details`; a
    /// provider that omits the detail object must parse as None, not fail.
    #[test]
    fn stream_usage_parses_cached_tokens_with_and_without_detail() {
        let usage: StreamUsage = serde_json::from_str(
            r#"{"prompt_tokens":100,"completion_tokens":17,"prompt_tokens_details":{"cached_tokens":42}}"#,
        )
        .unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 17);
        assert_eq!(usage.prompt_details.map(|d| d.cached_tokens), Some(42));

        let usage: StreamUsage = serde_json::from_str(r#"{"prompt_tokens":100}"#).unwrap();
        assert!(usage.prompt_details.is_none());
        // Providers that omit completion_tokens parse as 0, not fail.
        assert_eq!(usage.completion_tokens, 0);
    }

    // ---- SSE parser tests: real reqwest::blocking::Response built from an
    // http::Response with a raw SSE body, so both wire parsers are exercised
    // end to end without a server. ----

    fn read_chat_lines(
        lines: &[&str],
        sink: Option<tokio::sync::mpsc::Sender<SinkLine>>,
        cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    ) -> Result<super::Turn, Box<dyn std::error::Error + Send + Sync>> {
        super::run_sse_lines(lines, sink, cancel, super::ChatCompletionsParser::default())
    }

    fn read_responses_lines(
        lines: &[&str],
        sink: Option<tokio::sync::mpsc::Sender<SinkLine>>,
        cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    ) -> Result<super::Turn, Box<dyn std::error::Error + Send + Sync>> {
        super::run_sse_lines(lines, sink, cancel, super::ResponsesParser::default())
    }

    fn read_anthropic_lines(
        lines: &[&str],
        sink: Option<tokio::sync::mpsc::Sender<SinkLine>>,
        cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    ) -> Result<super::Turn, Box<dyn std::error::Error + Send + Sync>> {
        super::run_sse_lines(lines, sink, cancel, super::AnthropicParser::default())
    }

    /// Anthropic Messages: text and tool_use blocks reassemble per index,
    /// usage merges message_start (prompt side) with message_delta
    /// (cumulative output), and `message_stop` terminates the stream.
    #[test]
    fn anthropic_stream_reassembles_blocks_and_terminates_on_message_stop() {
        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":25,"cache_read_input_tokens":5,"cache_creation_input_tokens":2}}}"#,
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello "}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"th\":\"a.rs\"}"}}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            r#"data: {"type":"ping"}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":9}}"#,
            r#"data: {"type":"message_stop"}"#,
            // Server keeps the connection open after message_stop — Done
            // must stop reading before this arrives.
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":99}}"#,
        ];
        let turn = read_anthropic_lines(lines, Some(tx), &CancellationToken::new()).unwrap();
        assert_eq!(turn.message.content.as_deref(), Some("hello "));
        let calls = turn.message.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].function.name, "read");
        assert_eq!(calls[0].function.arguments, r#"{"path":"a.rs"}"#);
        assert_eq!(turn.stop_reason, Some(StopReason::ToolUse));
        // Prompt side counts cache reads/writes too; output is the
        // message_delta cumulative value, not the trailing one.
        assert_eq!(
            turn.usage,
            Some(Usage {
                prompt_tokens: 32,
                completion_tokens: 9,
                cached_tokens: Some(5)
            })
        );
    }

    /// Thinking deltas land as Thinking sink lines; signed thinking blocks
    /// are captured for replay, unsigned ones are not.
    #[test]
    fn anthropic_stream_captures_replayable_thinking() {
        let (tx, mut rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"step "}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig1"}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"one"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"unsigned"}}"#,
            r#"data: {"type":"content_block_stop","index":1}"#,
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"redacted_thinking","data":"encrypted-blob"}}"#,
            r#"data: {"type":"content_block_stop","index":2}"#,
        ];
        let turn = read_anthropic_lines(lines, Some(tx), &CancellationToken::new()).unwrap();
        let items = turn.message.reasoning_items.unwrap();
        assert_eq!(items.len(), 2, "unsigned thinking must not replay");
        assert_eq!(items[0]["type"], "thinking");
        assert_eq!(items[0]["thinking"], "step one");
        assert_eq!(items[0]["signature"], "sig1");
        assert_eq!(items[1]["type"], "redacted_thinking");
        assert_eq!(items[1]["data"], "encrypted-blob");
        let lines: Vec<SinkLine> = {
            let mut out = Vec::new();
            while let Ok(v) = rx.try_recv() {
                out.push(v);
            }
            out
        };
        assert!(matches!(&lines[0], SinkLine::Thinking(s) if s == "step "));
        assert!(matches!(&lines[1], SinkLine::Thinking(s) if s == "one"));
    }

    /// Anthropic stop reasons map onto the normalized set; unknown reasons
    /// stay unset.
    #[test]
    fn anthropic_stream_maps_stop_reasons() {
        for (reason, expected) in [
            ("end_turn", StopReason::Stop),
            ("stop_sequence", StopReason::Stop),
            ("max_tokens", StopReason::Length),
            ("model_context_window", StopReason::Length),
            ("tool_use", StopReason::ToolUse),
            ("refusal", StopReason::ContentFilter),
            ("pause_turn", StopReason::Stop),
        ] {
            let (tx, _rx) = mpsc::channel(32);
            let lines: &[&str] = &[&format!(
                r#"data: {{"type":"message_delta","delta":{{"stop_reason":"{reason}"}}}}"#
            )];
            let turn = read_anthropic_lines(lines, Some(tx), &CancellationToken::new()).unwrap();
            assert_eq!(turn.stop_reason, Some(expected), "stop_reason={reason}");
        }
        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[r#"data: {"type":"message_delta","delta":{"stop_reason":"junk"}}"#];
        assert_eq!(
            read_anthropic_lines(lines, Some(tx), &CancellationToken::new())
                .unwrap()
                .stop_reason,
            None
        );
    }

    /// A terminal `error` event on a 200 body is a real failure: retryable
    /// before any output flowed, a mid-stream marker after it.
    #[test]
    fn anthropic_error_event_fails_turn_by_output_flow() {
        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        ];
        let err = read_anthropic_lines(lines, Some(tx), &CancellationToken::new())
            .unwrap_err()
            .to_string();
        assert_eq!(err, "overloaded_error: Overloaded");
        assert!(!is_mid_stream(
            &*read_anthropic_lines(lines, None, &CancellationToken::new()).unwrap_err()
        ));

        // After text has streamed, the failure must carry the marker so a
        // retry cannot duplicate the partial transcript.
        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#,
            r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        ];
        let err = read_anthropic_lines(lines, Some(tx), &CancellationToken::new()).unwrap_err();
        assert!(is_mid_stream(&*err));
        assert_eq!(err.to_string(), "overloaded_error: Overloaded");
    }

    /// Garbage, empty, and keep-alive lines are skipped; a stream with no
    /// output yields an empty assistant message (no spurious tool calls).
    #[test]
    fn anthropic_stream_tolerates_garbage() {
        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            "data: not json at all",
            "data: ",
            r#"data: {"type":"ping"}"#,
            r#"data: {"type":"message_stop"}"#,
        ];
        let turn = read_anthropic_lines(lines, Some(tx), &CancellationToken::new()).unwrap();
        assert_eq!(turn.message.content, None);
        assert!(turn.message.tool_calls.is_none());
        assert_eq!(turn.usage, None);
        // Ghost tool blocks (no id) are dropped at finish, like responses.
        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","name":"ghost","input":{}}}"#,
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            r#"data: {"type":"content_block_stop","index":0}"#,
        ];
        let turn = read_anthropic_lines(lines, Some(tx), &CancellationToken::new()).unwrap();
        assert!(turn.message.tool_calls.is_none());
    }

    /// A reasoning output item with encrypted_content is captured onto the
    /// message for stateless replay; summary-only items are not.
    #[test]
    fn responses_stream_captures_replayable_reasoning_items() {
        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"r1","summary":[],"encrypted_content":"blob1"}}"#,
            r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"r2","summary":[{"type":"summary_text","text":"visible"}]}}"#,
            r#"data: {"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"read","arguments":"{}"}}"#,
            "data: [DONE]",
        ];
        let msg = read_responses_lines(lines, Some(tx), &CancellationToken::new())
            .unwrap()
            .message;
        let items = msg.reasoning_items.expect("replayable items captured");
        assert_eq!(items.len(), 1, "summary-only item must be skipped");
        assert_eq!(items[0]["encrypted_content"], "blob1");
        assert!(msg.reasoning_content.is_none());
    }

    /// Null AND empty-string `encrypted_content` are both unreplayable:
    /// an empty blob would corrupt the thread if sent back.
    #[test]
    fn responses_stream_skips_null_and_empty_encrypted_content() {
        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"r1","summary":[],"encrypted_content":""}}"#,
            r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"r2","summary":[],"encrypted_content":null}}"#,
            "data: [DONE]",
        ];
        let msg = read_responses_lines(lines, Some(tx), &CancellationToken::new())
            .unwrap()
            .message;
        assert!(
            msg.reasoning_items.is_none(),
            "empty/null blobs must not be captured: {:?}",
            msg.reasoning_items
        );
    }

    /// DeepSeek-style chat-completions reasoning: `reasoning_content` deltas
    /// accumulate onto the message for replay; other reasoning keys are
    /// UI-only and must not leak into the replay field.
    #[test]
    fn chat_stream_captures_reasoning_content_for_replay() {
        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            r#"data: {"choices":[{"delta":{"reasoning_content":"step 1"}}]}"#,
            r#"data: {"choices":[{"delta":{"reasoning":"openrouter-style"}}]}"#,
            r#"data: {"choices":[{"delta":{"reasoning_content":" step 2"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"done"}}]}"#,
            "data: [DONE]",
        ];
        let msg = read_chat_lines(lines, Some(tx), &CancellationToken::new())
            .unwrap()
            .message;
        assert_eq!(msg.reasoning_content.as_deref(), Some("step 1 step 2"));
        assert!(msg.reasoning_items.is_none());
    }

    /// Chat-completions stream: content accumulates across chunks and splits
    /// into complete sink lines; fragmented tool-call deltas merge into one
    /// call; the usage chunk (with cache detail) surfaces as `Some(Usage)`.
    #[test]
    fn chat_stream_assembles_content_tools_and_usage() {
        let (tx, mut rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"hello "}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"world\n"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"second line"}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.rs\"}"}}]}}]}"#,
            r#"data: {"choices":[],"usage":{"prompt_tokens":123,"completion_tokens":45,"prompt_tokens_details":{"cached_tokens":7}}}"#,
            "data: [DONE]",
        ];
        let turn = read_chat_lines(lines, Some(tx), &CancellationToken::new()).unwrap();
        let usage = turn.usage;
        let msg = turn.message;
        assert_eq!(msg.content.as_deref(), Some("hello world\nsecond line"));
        let calls = msg.tool_calls.unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "c1");
        assert_eq!(calls[0].function.name, "read");
        assert_eq!(calls[0].function.arguments, r#"{"path":"a.rs"}"#);
        assert_eq!(
            usage,
            Some(Usage {
                prompt_tokens: 123,
                completion_tokens: 45,
                cached_tokens: Some(7)
            })
        );
        let lines: Vec<SinkLine> = {
            let mut out = Vec::new();
            while let Ok(v) = rx.try_recv() {
                out.push(v);
            }
            out
        };
        assert!(matches!(&lines[0], SinkLine::Assistant(s) if s == "hello world"));
        assert!(matches!(&lines[1], SinkLine::Assistant(s) if s == "second line"));
    }

    /// A code fence opened mid-stream is buffered and flushed as one block;
    /// prose before/after streams line by line.
    #[test]
    fn chat_stream_buffers_code_fences() {
        let (tx, mut rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            r#"data: {"choices":[{"delta":{"content":"```rust\n"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"fn main() {}\n"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"```\n"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"trailing prose"}}]}"#,
            "data: [DONE]",
        ];
        let msg = read_chat_lines(lines, Some(tx), &CancellationToken::new())
            .unwrap()
            .message;
        assert_eq!(
            msg.content.as_deref(),
            Some("```rust\nfn main() {}\n```\ntrailing prose")
        );
        let lines: Vec<SinkLine> = {
            let mut out = Vec::new();
            while let Ok(v) = rx.try_recv() {
                out.push(v);
            }
            out
        };
        // The fence body keeps its streamed trailing newline and the close
        // adds one more — current sink contract; consumers re-parse the block.
        assert!(
            matches!(&lines[0], SinkLine::Assistant(s)
              if s == "```rust:\nfn main() {}\n\n```"),
            "{lines:?}"
        );
        assert!(matches!(&lines[1], SinkLine::Assistant(s) if s == "trailing prose"));
    }

    /// `finish_reason` on the final chunk maps onto the normalized
    /// [`StopReason`] (length truncation, tool-call handoff, clean stop).
    #[test]
    fn chat_stream_maps_finish_reason_to_stop_reason() {
        for (finish, expected) in [
            ("stop", StopReason::Stop),
            ("length", StopReason::Length),
            ("tool_calls", StopReason::ToolUse),
            ("function_call", StopReason::ToolUse),
            ("content_filter", StopReason::ContentFilter),
        ] {
            let (tx, _rx) = mpsc::channel(32);
            let lines: &[&str] = &[
                r#"data: {"choices":[{"delta":{"content":"hi"}}]}"#,
                &format!(r#"data: {{"choices":[{{"delta":{{}},"finish_reason":"{finish}"}}]}}"#),
                "data: [DONE]",
            ];
            let turn = read_chat_lines(lines, Some(tx), &CancellationToken::new()).unwrap();
            assert_eq!(turn.stop_reason, Some(expected), "finish_reason={finish}");
            assert_eq!(turn.message.content.as_deref(), Some("hi"));
        }
        // Unrecognized reasons stay unset instead of being guessed.
        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            r#"data: {"choices":[{"delta":{},"finish_reason":"junk"}]}"#,
            "data: [DONE]",
        ];
        let turn = read_chat_lines(lines, Some(tx), &CancellationToken::new()).unwrap();
        assert_eq!(turn.stop_reason, None);
    }

    /// A pre-cancelled token unwinds before reading anything.
    #[test]
    fn chat_stream_returns_cancelled_error_when_token_set() {
        let token = CancellationToken::new();
        token.cancel();
        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[r#"data: {"choices":[{"delta":{"content":"x"}}]}"#];
        let err = read_chat_lines(lines, Some(tx), &token).unwrap_err();
        assert_eq!(err.to_string(), "cancelled");
    }

    /// Responses API: the function-call state machine must survive deltas
    /// that arrive BEFORE their item is added (buffered then flushed), and
    /// completed calls without an id must be dropped, not executed.
    #[test]
    fn responses_stream_reassembles_tool_calls_with_early_deltas() {
        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            r#"data: {"type":"response.output_text.delta","delta":"hello\n"}"#,
            r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"read","arguments":""}}"#,
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"item_2","delta":"EARLY"}"#,
            r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"item_2","call_id":"call_2","name":"bash","arguments":""}}"#,
            r#"data: {"type":"response.function_call_arguments.delta","item_id":"item_2","delta":"LATER"}"#,
            r#"data: {"type":"response.output_item.added","output_index":3,"item":{"type":"function_call","name":"ghost"}}"#,
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":50,"output_tokens":9,"input_tokens_details":{"cached_tokens":5}}}}"#,
            "data: [DONE]",
        ];
        let turn = read_responses_lines(lines, Some(tx), &CancellationToken::new()).unwrap();
        let usage = turn.usage;
        let msg = turn.message;
        assert_eq!(msg.content.as_deref(), Some("hello\n"));
        let calls = msg.tool_calls.unwrap();
        assert_eq!(
            calls.len(),
            2,
            "ghost call (no id) and padding must be filtered: {calls:?}"
        );
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "read");
        assert_eq!(calls[1].id, "call_2");
        assert_eq!(calls[1].function.arguments, "EARLYLATER");
        assert_eq!(
            usage,
            Some(Usage {
                prompt_tokens: 50,
                completion_tokens: 9,
                cached_tokens: Some(5)
            })
        );
    }

    /// Garbage, empty, and keep-alive data lines are skipped; reasoning
    /// deltas (both provider keys) land as Thinking sink lines; a stream
    /// with no output yields an empty assistant message.
    #[test]
    fn responses_stream_tolerates_garbage_and_extracts_reasoning() {
        let (tx, mut rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            "data: not json at all",
            "data: ",
            r#"data: {"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#,
            r#"data: {"type":"response.reasoning_text.delta","delta":" more"}"#,
            "data: [DONE]",
        ];
        let turn = read_responses_lines(lines, Some(tx), &CancellationToken::new()).unwrap();
        assert_eq!(turn.message.content, None);
        assert!(turn.message.tool_calls.is_none());
        assert_eq!(turn.usage, None);
        let lines: Vec<SinkLine> = {
            let mut out = Vec::new();
            while let Ok(v) = rx.try_recv() {
                out.push(v);
            }
            out
        };
        assert!(matches!(&lines[0], SinkLine::Thinking(s) if s == "thinking"));
        assert!(matches!(&lines[1], SinkLine::Thinking(s) if s == " more"));
    }

    /// `response.completed` normalizes to a clean stop; `response.incomplete`
    /// normalizes by reason (max_output_tokens → Length, content_filter →
    /// ContentFilter); an unrecognized reason stays unset.
    #[test]
    fn responses_stream_maps_terminal_events_to_stop_reasons() {
        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            r#"data: {"type":"response.output_text.delta","delta":"partial"}"#,
            r#"data: {"type":"response.incomplete","response":{"usage":{"input_tokens":10,"output_tokens":99},"incomplete_details":{"reason":"max_output_tokens"}}}"#,
            "data: [DONE]",
        ];
        let turn = read_responses_lines(lines, Some(tx), &CancellationToken::new()).unwrap();
        assert_eq!(turn.stop_reason, Some(StopReason::Length));
        assert_eq!(
            turn.usage,
            Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 99,
                cached_tokens: None
            })
        );

        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":4}}}"#,
            "data: [DONE]",
        ];
        let turn = read_responses_lines(lines, Some(tx), &CancellationToken::new()).unwrap();
        assert_eq!(turn.stop_reason, Some(StopReason::Stop));

        // Incomplete for a content filter is a partial reply too.
        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            r#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}"#,
            "data: [DONE]",
        ];
        let turn = read_responses_lines(lines, Some(tx), &CancellationToken::new()).unwrap();
        assert_eq!(turn.stop_reason, Some(StopReason::ContentFilter));

        // An unrecognized incomplete reason must not claim a clean stop.
        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            r#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"junk"}}}"#,
            "data: [DONE]",
        ];
        let turn = read_responses_lines(lines, Some(tx), &CancellationToken::new()).unwrap();
        assert_eq!(turn.stop_reason, None);
    }

    /// The mid-stream marker applies only once output has flowed; before
    /// that a failure stays retryable. Display passes the message through so
    /// `"cancelled"` matching keeps working.
    #[test]
    fn stream_error_marking_follows_output_flow() {
        assert!(!is_mid_stream(&*stream_err("boom", false)));
        assert!(is_mid_stream(&*stream_err("boom", true)));
        assert_eq!(stream_err("cancelled", true).to_string(), "cancelled");
    }

    /// A mid-stream failure on a headless run must close the open thinking
    /// line (the terminal would otherwise stay dimmed); double-close is a
    /// no-op, and retryability still follows output flow.
    #[test]
    fn driver_err_closes_open_thinking_line() {
        let mut driver = SseDriver::new(None);
        driver.thinking_open = true;
        driver.output_flowed = true;
        let err = driver_err(&mut driver, "boom");
        assert!(!driver.thinking_open);
        assert!(is_mid_stream(&*err));
        // Idempotent: ending an already-closed line writes nothing.
        driver.end_thinking();
        assert!(!driver.thinking_open);
        // Before any output, the same failure stays retryable.
        let mut fresh = SseDriver::new(None);
        fresh.thinking_open = true;
        assert!(!is_mid_stream(&*driver_err(&mut fresh, "boom")));
    }

    /// The async SSE driver must yield complete lines to the parser even
    /// when a provider splits one SSE line across `response.chunk()`
    /// boundaries, and must flush a final unterminated line at EOF (how the
    /// Responses API ends its body). The sync `run_sse_lines` tests above
    /// never exercise this: an in-memory body arrives as one giant chunk.
    /// A localhost server writes the body in three deliberately straddled
    /// writes; gaps between them keep each write a separate chunk.
    #[tokio::test]
    async fn run_sse_splits_lines_straddling_chunk_boundaries() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.set_nodelay(true).unwrap();
            // Drain the request head before responding.
            let mut buf = [0u8; 4096];
            let mut seen = Vec::new();
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                seen.extend_from_slice(&buf[..n]);
                if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let c1 = r#"data: {"type":"response.output_text.delta","delta":"hel"#;
            let c2 = r#"lo"}
data: {"type":"response.output_text.delta","delta":" wor"#;
            // Final line: no trailing newline — flushed at EOF.
            let c3 = r#"ld"}
data: {"type":"response.output_text.delta","delta":"!"}"#;
            let mut body = String::new();
            body.push_str(c1);
            body.push_str(c2);
            body.push_str(c3);
            sock.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            for chunk in [c1, c2, c3] {
                sock.write_all(chunk.as_bytes()).await.unwrap();
                sock.flush().await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        });
        let response = reqwest::Client::new()
            .get(format!("http://{addr}/v1/responses"))
            .send()
            .await
            .unwrap();
        let turn = super::read_responses_stream(
            response,
            None,
            &CancellationToken::new(),
            Some(Duration::from_secs(30)),
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(turn.message.content.as_deref(), Some("hello world!"));
    }

    #[test]
    fn idle_timeout_env_parses() {
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("DEX_STREAM_IDLE_TIMEOUT_SECS").ok();
        std::env::set_var("DEX_STREAM_IDLE_TIMEOUT_SECS", "7");
        assert_eq!(
            stream_idle_timeout_for("dex-test-no-such-model", false),
            Some(Duration::from_secs(7))
        );
        // 0 disables the watchdog entirely (no timer armed, not an
        // immediate timeout and not an infinite deadline).
        std::env::set_var("DEX_STREAM_IDLE_TIMEOUT_SECS", "0");
        assert_eq!(
            stream_idle_timeout_for("dex-test-no-such-model", false),
            None
        );
        // Garbage falls back to the default.
        std::env::set_var("DEX_STREAM_IDLE_TIMEOUT_SECS", "junk");
        assert_eq!(
            stream_idle_timeout_for("dex-test-no-such-model", false),
            Some(Duration::from_secs(super::DEFAULT_STREAM_IDLE_TIMEOUT_SECS))
        );
        match prev {
            Some(v) => std::env::set_var("DEX_STREAM_IDLE_TIMEOUT_SECS", v),
            None => std::env::remove_var("DEX_STREAM_IDLE_TIMEOUT_SECS"),
        }
    }

    #[test]
    fn idle_timeout_default_is_per_model() {
        // No env override (lock held so a parallel env-mutating test can't
        // leak in); an unknown model is never reasoning-capable, so the
        // default must not depend on whatever catalog cache the machine has.
        let _env = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("DEX_STREAM_IDLE_TIMEOUT_SECS").ok();
        std::env::remove_var("DEX_STREAM_IDLE_TIMEOUT_SECS");
        assert_eq!(
            stream_idle_timeout_for("dex-test-no-such-model", false),
            Some(Duration::from_secs(super::DEFAULT_STREAM_IDLE_TIMEOUT_SECS))
        );
        // Thinking enabled for the call earns the patient budget without any
        // catalog entry.
        assert_eq!(
            stream_idle_timeout_for("dex-test-no-such-model", true),
            Some(Duration::from_secs(
                super::REASONING_STREAM_IDLE_TIMEOUT_SECS
            ))
        );
        match prev {
            Some(v) => std::env::set_var("DEX_STREAM_IDLE_TIMEOUT_SECS", v),
            None => std::env::remove_var("DEX_STREAM_IDLE_TIMEOUT_SECS"),
        }
    }

    #[test]
    fn dropped_connection_matching() {
        assert!(is_dropped_connection(
            "error sending request: connection closed before message completed"
        ));
        assert!(is_dropped_connection(
            "connection reset by peer (os error 104)"
        ));
        assert!(!is_dropped_connection("API error: boom"));
        assert!(!is_dropped_connection(
            "stream idle for over 90s; the provider stalled"
        ));
    }

    /// A server that sends headers then drops the socket must surface a
    /// marked transport error pre-output (not a bare string): the retry gate
    /// matches on the `StreamTransportError` marker, so new transport
    /// wordings and `reqwest::Error` kinds need no matcher updates.
    #[tokio::test]
    async fn pre_output_drop_preserves_typed_transport_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            // Drain the request head, advertise a body, then die: the client
            // sees a mid-body EOF on its first `chunk()`.
            let mut buf = [0u8; 4096];
            let mut seen = Vec::new();
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    return;
                }
                seen.extend_from_slice(&buf[..n]);
                if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = sock
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 100\r\n\r\n",
                )
                .await;
            // Socket drops here with the body unsent.
        });
        let response = reqwest::Client::new()
            .get(format!("http://{addr}/v1/chat/completions"))
            .send()
            .await
            .unwrap();
        let err = read_stream(
            response,
            None,
            &CancellationToken::new(),
            Some(Duration::from_secs(30)),
        )
        .await
        .unwrap_err();
        // Provenance, not wording or kind: the same mid-body FIN reads as
        // `is_decode` on this reqwest version (`is_body` on others), so the
        // gate matches the marker `run_sse` attached, not the taxonomy.
        assert!(is_transport_error(&*err));
        assert!(!crate::llm::streaming::is_mid_stream(&*err));
        // The marker preserves the transport cause for logs and notices.
        let mut source = std::error::Error::source(&*err);
        let mut found_cause = false;
        while let Some(e) = source {
            if e.downcast_ref::<reqwest::Error>().is_some() {
                found_cause = true;
                break;
            }
            source = std::error::Error::source(e);
        }
        assert!(found_cause, "marker keeps the reqwest cause in-chain");
    }

    /// A provider that sends response headers and then never sends a byte
    /// must not park the turn forever: the idle watchdog fails the stream
    /// with a diagnosable error instead of an opaque hang.
    #[tokio::test]
    async fn stalled_stream_fails_after_idle_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = socket;
            // Valid headers with a body that never arrives: chunk() pends.
            use tokio::io::AsyncWriteExt as _;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n")
                .await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let client = reqwest::Client::new();
        let response = client
            .get(format!("http://{addr}/v1"))
            .send()
            .await
            .unwrap();
        let result = read_stream(
            response,
            None,
            &CancellationToken::new(),
            // Explicit budget, not the env var: this test holds no env lock
            // (a guard can't span `.await`) and must not race env-mutating
            // tests. Env parsing is covered by `idle_timeout_env_parses`.
            Some(Duration::from_secs(1)),
        )
        .await;
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("stream idle"),
            "watchdog must name the stall: {err}"
        );
    }
}
