use super::parser::AnthropicParser;
use super::parser::ChatCompletionsParser;
use super::parser::ResponsesParser;
use super::parser::StreamEvent;
use super::parser::StreamParser;
use crate::protocol::ChatMessage;
use crate::protocol::Role;
use crate::protocol::SinkLine;
use crate::protocol::StopReason;
use crate::protocol::Usage;
use crate::runtime::console::with_console;
use crate::ui::theme::highlight::print_code_block;
use crate::ui::theme::highlight::print_markdown_text;
use std::io;
use std::io::Write;
use std::time::Duration;
use tokio::sync::mpsc;

/// Incremental markdown printer: prose is flushed as soon as a full line
/// arrives; code fences are buffered until closed so they can be highlighted
/// as one block. If a fence is still open when the stream ends, it is
/// flushed as-is.
pub(crate) struct StreamPrinter {
    pub(crate) in_code: bool,
    pub(crate) code_lang: String,
    pub(crate) code_body: String,
    pub(crate) sink: Option<mpsc::Sender<SinkLine>>,
    /// Headless (no-sink) gap state so plain-terminal output follows the same
    /// blanks-around-blocks rule as the TUI (MD022/MD031/MD032/MD058).
    /// `prev` is the last printed prose line (trimmed-start); `air` mirrors
    /// `markdown_leaves_air`; `empty` avoids doubling blanks / leading blank.
    pub(crate) headless_prev: String,
    pub(crate) headless_air: bool,
    pub(crate) headless_empty: bool,
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
        use crate::ui::theme::markdown as md;
        if line.trim().is_empty() {
            return false;
        }
        if md::is_continuation_lines(&self.headless_prev, line) {
            return false;
        }
        self.headless_air || md::needs_gap_before(self.headless_empty, line)
    }

    fn headless_note(&mut self, line: &str) {
        use crate::ui::theme::markdown as md;
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

    /// Sink writes back-pressure with `send().await`, so a full channel
    /// stalls the driver rather than silently dropping transcript lines.
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

/// Only a failure with no streamed output (HTTP status, connect failure,
/// drop before the first delta) qualifies for protocol fallback or a
/// same-protocol re-issue; a mid-stream failure may have already put partial
/// text on the transcript, and a retried call would duplicate it. Defined
/// here with the marker so the retry gates (`client`) and the fallback gate
/// (`dispatch`) share one owner instead of importing each other.
pub(crate) fn is_mid_stream(err: &(dyn std::error::Error + 'static)) -> bool {
    err.downcast_ref::<MidStreamError>().is_some()
}

/// Wrap a stream failure as [`MidStreamError`] only once output has flowed;
/// a failure before any output is retryable (protocol fallback may re-run
/// the turn without duplicating anything).
pub(crate) fn stream_err(
    message: &str,
    output_flowed: bool,
) -> Box<dyn std::error::Error + Send + Sync> {
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
pub(crate) fn driver_err(
    driver: &mut SseDriver,
    message: &str,
) -> Box<dyn std::error::Error + Send + Sync> {
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
pub(crate) struct SseDriver {
    pub(crate) content: String,
    pub(crate) pending: String,
    pub(crate) printer: StreamPrinter,
    pub(crate) usage: Option<Usage>,
    pub(crate) stop_reason: Option<StopReason>,
    pub(crate) output_flowed: bool,
    /// Headless stderr thinking line is open (dim, no closing newline yet).
    pub(crate) thinking_open: bool,
}

impl SseDriver {
    pub(crate) fn new(sink: Option<mpsc::Sender<SinkLine>>) -> Self {
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
            let _ = io::stderr().write_all(crate::runtime::console::DIM.as_bytes());
        }
        let _ = io::stderr().write_all(thought.as_bytes());
    }

    pub(crate) fn end_thinking(&mut self) {
        if self.thinking_open {
            self.thinking_open = false;
            let _ = io::stderr().write_all(b"\x1b[0m\n");
            let _ = io::stderr().flush();
        }
    }

    /// Feed one raw SSE line; `Ok(true)` when the parser signalled Done.
    /// A parser `Fail` becomes `Err` honoring the output-flowed rule.
    /// Sink sends back-pressure with `send().await`, so transcript lines
    /// are never dropped on a full channel.
    pub(crate) async fn feed_raw_async(
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

    pub(crate) async fn finish_turn_async(
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
