use serde_json::json;
use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::agent::state::CancellationSource;
use crate::core::console::with_console;
use crate::core::types::{ChatMessage, ChatRequest, SinkLine, StreamOptions};
use crate::llm::config::{insert_extra_header, LlmConfig};
use crate::llm::protocol::{responses_input, responses_tools, tools_schema};
use crate::llm::stream::{read_responses_stream, read_stream, Turn};

pub(crate) fn error_chain_message(e: &(dyn std::error::Error + 'static)) -> String {
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    msg
}

pub(crate) trait ModelClient: Clone + Send + Sync {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        with_tools: bool,
        sink: Option<mpsc::Sender<SinkLine>>,
        cancel: &(dyn CancellationSource + Send + Sync),
    ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>>;
}

impl ModelClient for LlmConfig {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        with_tools: bool,
        sink: Option<mpsc::Sender<SinkLine>>,
        cancel: &(dyn CancellationSource + Send + Sync),
    ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
        crate::llm::streaming::complete(self, messages, with_tools, sink, cancel)
            .await
            .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(error_chain_message(&*e)))
    }
}

pub(crate) fn authenticated_request(
    request: reqwest::RequestBuilder,
    config: &LlmConfig,
) -> reqwest::RequestBuilder {
    let request =
        config
            .provider
            .auth_scheme()
            .apply(request, &config.api_key, config.account_id.as_deref());
    // Provider-scoped file headers apply first; the global/env/CLI extras
    // win on collision (explicit always beats file). `authorization` is
    // never overridable here — the api key owns it. Malformed names or
    // values are skipped so one bad header can't fail the turn.
    let mut merged = config.provider_headers.clone();
    for (name, value) in &config.extra_headers {
        insert_extra_header(&mut merged, name, value);
    }
    let mut request = request;
    for (name, value) in &merged {
        if name.eq_ignore_ascii_case("authorization") {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) {
            request = request.header(name, value);
        }
    }
    request
}

pub(crate) fn retryable_status(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error()
}

/// Provider wording for "you hit a rate limit, retry after a brief wait".
/// Matched on lowercase against error bodies/messages (HTTP error JSON like
/// `{"error":{"code":"rate_limit_exceeded",...}}` as well as stream-phase
/// `Fail` messages like `rate_limit_error: ...`). Gateways don't always use
/// a 429 status for these, so the body — not just the status — decides.
pub(crate) fn is_rate_limited(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    // `-`/`_` spellings (`rate_limit`, `rate-limit`) collapse to one phrase.
    let normalized = lower.replace(['-', '_'], " ");
    normalized.contains("rate limit")
        || normalized.contains("too many requests")
        || normalized.contains("overload")
        || contains_status_code(&lower, "429")
        || contains_status_code(&lower, "529")
}

/// Bare `{"code":429}` proxied with a non-429 status still counts — but only
/// as a standalone number, so `14290 tokens` or `429496` can't false-positive.
fn contains_status_code(haystack: &str, code: &str) -> bool {
    haystack.match_indices(code).any(|(i, _)| {
        let prev_ok = i == 0 || !haystack.as_bytes()[i - 1].is_ascii_digit();
        let next_ok = haystack[i + code.len()..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_digit());
        prev_ok && next_ok
    })
}

/// `Retry-After` header value → duration. Seconds form plus the HTTP-date
/// form (IMF-fixdate `... GMT`, parsed via a `+0000` rewrite); capped so a
/// hostile header cannot park the turn for hours. `None` when absent or
/// unparsable.
fn retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs.min(RETRY_AFTER_CAP_SECS)));
    }
    let date = parse_retry_after_date(value)?;
    let delta = date - chrono::Utc::now();
    Some(Duration::from_secs(
        delta.num_seconds().clamp(0, RETRY_AFTER_CAP_SECS as i64) as u64,
    ))
}

/// Parse seconds-form already handled; here HTTP-date. `chrono` parses
/// RFC 2822 (`...+0000`) but wire dates use IMF-fixdate (`... GMT`).
fn parse_retry_after_date(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Ok(d) = chrono::DateTime::parse_from_rfc2822(value) {
        return Some(d.with_timezone(&chrono::Utc));
    }
    // "Sun, 06 Nov 1994 08:49:37 GMT" → "Sun, 06 Nov 1994 08:49:37 +0000"
    let stripped = value.strip_suffix("GMT").map(str::trim_end)?;
    let rewritten = format!("{stripped} +0000");
    chrono::DateTime::parse_from_rfc2822(&rewritten)
        .ok()
        .map(|d| d.with_timezone(&chrono::Utc))
}

/// Backoff for attempt `attempt` (0-based): exponential base with ±25%
/// jitter, or the provider's `Retry-After` when it asks for longer (honored
/// exactly — the server asked for a specific delay). Jitter keeps a fleet of
/// concurrent dex processes (or one daemon running several sessions) from
/// retrying in lockstep and stampeding the endpoint again.
fn backoff_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    const BASE_MS: u64 = 500;
    let base = Duration::from_millis(BASE_MS.saturating_mul(1u64 << attempt.min(4)));
    let Some(requested) = retry_after.filter(|d| *d > base / 2) else {
        // Cheap jitter source: sub-second clock noise. Not cryptographic —
        // it only needs to decorrelate concurrent retry loops.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|t| u64::from(t.subsec_nanos()))
            .unwrap_or(0);
        let spread = (base.as_millis() as u64 / 4).max(1);
        let offset = (nanos % (2 * spread + 1)) as i64 - spread as i64;
        let jittered = base.as_millis() as i64 + offset;
        return Duration::from_millis(jittered.max(0) as u64);
    };
    requested
}

/// Ceiling for an honored `Retry-After`, so a pathological header cannot
/// park a turn for minutes.
const RETRY_AFTER_CAP_SECS: u64 = 120;

pub(crate) fn provider_log(event: &str, detail: &str) {
    let Some(base) = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    else {
        return;
    };
    let path = base.join("dex/provider.jsonl");
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let record =
            json!({"timestamp": chrono::Utc::now().to_rfc3339(), "event": event, "detail": detail});
        // Single write syscall so concurrent turns cannot interleave records.
        let mut line = record.to_string();
        line.push('\n');
        let _ = file.write_all(line.as_bytes());
    }
}
/// Send a provider request with shared retry/backoff, 401 credential refresh
/// (OpenAI Codex), and provider logging. The protocol-specific request body
/// and post-success reader are supplied by the caller.
async fn post_with_retry(
    config: &LlmConfig,
    url: &str,
    body: &impl serde::Serialize,
    sink: Option<&mpsc::Sender<SinkLine>>,
) -> Result<reqwest::Response, Box<dyn std::error::Error + Send + Sync>> {
    const MAX_HTTP_RETRIES: u32 = 3;
    let mut active_config = config.clone();
    for attempt in 0..=MAX_HTTP_RETRIES {
        let request = config.client.post(url);
        crate::log!(
            Debug,
            "POST {url} (model {}, attempt {attempt})",
            config.model
        );
        let started = std::time::Instant::now();
        let resp = match authenticated_request(request, &active_config)
            .json(body)
            .send()
            .await
        {
            Ok(resp) => {
                crate::log!(
                    Debug,
                    "HTTP {} {url} in {:?}",
                    resp.status(),
                    started.elapsed()
                );
                resp
            }
            Err(e) if attempt < MAX_HTTP_RETRIES => {
                let delay = backoff_delay(attempt, None);
                with_console(sink.is_some(), || {
                    eprintln!(
                        "[llm] request failed: {}; retrying in {:?}",
                        error_chain_message(&e),
                        delay
                    )
                });
                tokio::time::sleep(delay).await;
                continue;
            }
            Err(e) => {
                provider_log("request_failed", &error_chain_message(&e));
                crate::log!(Warn, "request failed: {}", error_chain_message(&e));
                return Err(Box::new(e));
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(retry_after);
            let body_text = resp.text().await.map_err(Box::new)?;
            if status == reqwest::StatusCode::UNAUTHORIZED
                && active_config.provider.credentials_refreshable()
                && attempt < MAX_HTTP_RETRIES
            {
                if let Ok((token, account)) = crate::llm::config::resolve_credentials(
                    &active_config.provider,
                    &active_config.provider_entries,
                ) {
                    active_config.api_key = token;
                    active_config.account_id = account;
                    continue;
                }
            }
            let retryable = retryable_status(status) || is_rate_limited(&body_text);
            if retryable && attempt < MAX_HTTP_RETRIES {
                let delay = backoff_delay(attempt, retry_after);
                with_console(sink.is_some(), || {
                    eprintln!(
                        "[llm] API error {}: retrying in {:?}{}",
                        status,
                        delay,
                        match retry_after {
                            Some(d) => format!(" (server asked for {d:?})"),
                            None => String::new(),
                        }
                    )
                });
                tokio::time::sleep(delay).await;
                continue;
            }
            provider_log("api_error", &format!("{}: {}", status, body_text));
            // Cap the logged body: some providers return whole HTML pages.
            let head: String = body_text.chars().take(400).collect();
            crate::log!(Warn, "api error {status}: {head}");
            // Opaque 5xx / missing route from `/responses` usually means the
            // model only speaks chat-completions (proven for e.g.
            // glm-5.3-flash on zen/go) — point at the per-model override
            // instead of a bare body. Auth and rate-limit failures say
            // nothing about the protocol, and a pinned `api:` means the user
            // already decided.
            if url.ends_with("/responses")
                && !config.api_pinned
                && (status == reqwest::StatusCode::NOT_FOUND || status.is_server_error())
            {
                return Err(format!(
                    "API error: {} (hint: {} may speak openai-completions; set DEX_MODEL_APIS={}=openai-completions)",
                    body_text, config.model, config.model
                )
                .into());
            }
            return Err(format!("API error: {}", body_text).into());
        }

        return Ok(resp);
    }

    unreachable!()
}

pub(crate) async fn call_chat_completions(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
    let req = ChatRequest {
        model: &config.model,
        messages,
        tools: if with_tools {
            tools_schema()
        } else {
            Vec::new()
        },
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
        reasoning_effort: &config.thinking_effort,
    };
    // Same-protocol retry for a stalled or dropped stream: the watchdog
    // fired without a chunk (or the socket died pre-output), so re-issuing
    // the request resumes the turn instead of failing it back to the user
    // for a manual `continue`. Bounded (`MAX` + 1 attempts); a persistently
    // silent provider still surfaces its error.
    let idle_timeout = crate::llm::stream::stream_idle_timeout_for(
        &config.model,
        config.thinking_effort.is_some(),
    );
    for attempt in 0..=MAX_IDLE_STREAM_RETRIES {
        let resp = post_with_retry(
            config,
            &format!("{}/chat/completions", config.base_url),
            &req,
            sink.as_ref(),
        )
        .await?;
        match read_stream(resp, sink.clone(), cancel, idle_timeout).await {
            Ok(turn) => return Ok(turn),
            Err(e) => {
                let msg = error_chain_message(&*e);
                if (should_retry_idle(&msg, attempt) || should_retry_dropped(&*e, attempt))
                    && !cancel.is_cancelled()
                {
                    note_idle_retry(&sink, attempt, &msg).await;
                    tokio::time::sleep(backoff_delay(attempt, None)).await;
                    continue;
                }
                return Err(e);
            }
        }
    }

    unreachable!()
}

pub(crate) async fn call_responses(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
    let (instructions, input) = responses_input(messages);
    let mut body = json!({
        "model": config.model,
        "input": input,
        "stream": true,
        "store": false,
        // State is never stored server-side, so ask for the encrypted
        // reasoning blobs — without them reasoning can't be replayed and
        // the model re-reasons from scratch on every tool call.
        "include": ["reasoning.encrypted_content"],
    });
    if let Some(instructions) = instructions {
        body["instructions"] = json!(instructions);
    }
    if with_tools {
        body["tools"] = json!(responses_tools());
    }
    if let Some(effort) = &config.thinking_effort {
        body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
    }
    let idle_timeout = crate::llm::stream::stream_idle_timeout_for(
        &config.model,
        config.thinking_effort.is_some(),
    );
    for attempt in 0..=MAX_IDLE_STREAM_RETRIES {
        // Mid-stream failures (output already flowed) are marked by the stream
        // driver itself, so the protocol-fallback gate won't re-run a turn whose
        // partial text is already on the transcript — while a drop before the
        // first delta stays retryable. Idle stalls retry same-protocol here
        // instead (explicit notice, no fallback), mid-stream included: the
        // alternative is a failed turn whose manual `continue` duplicates the
        // partial output anyway.
        let resp = post_with_retry(
            config,
            &format!("{}/responses", config.base_url),
            &body,
            sink.as_ref(),
        )
        .await?;
        match read_responses_stream(resp, sink.clone(), cancel, idle_timeout).await {
            Ok(turn) => return Ok(turn),
            Err(e) => {
                let msg = error_chain_message(&*e);
                if (should_retry_idle(&msg, attempt) || should_retry_dropped(&*e, attempt))
                    && !cancel.is_cancelled()
                {
                    note_idle_retry(&sink, attempt, &msg).await;
                    tokio::time::sleep(backoff_delay(attempt, None)).await;
                    continue;
                }
                return Err(e);
            }
        }
    }

    unreachable!()
}

pub(crate) async fn call_anthropic_messages(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
    let body = crate::llm::anthropic::messages_body(config, messages, with_tools);
    // Stream-phase budget only: each attempt re-issues the whole request via
    // `post_with_retry` (its own HTTP-phase budget), so the worst case is
    // (`MAX_STREAM_RETRIES` + 1) × (`MAX_HTTP_RETRIES` + 1) POSTs. In
    // practice this loop only runs after the inner one already returned a
    // 200, and a 200 body carries no `Retry-After` — hence
    // `backoff_delay(attempt, None)`, with the attempt index restarting here.
    const MAX_STREAM_RETRIES: u32 = 3;
    let idle_timeout = crate::llm::stream::stream_idle_timeout_for(
        &config.model,
        config.thinking_effort.is_some(),
    );
    for attempt in 0..=MAX_STREAM_RETRIES {
        let resp = post_with_retry(
            config,
            &crate::llm::anthropic::messages_url(&config.base_url),
            &body,
            sink.as_ref(),
        )
        .await?;
        // Same output-flowed marker semantics as the OpenAI protocols: a drop
        // before the first delta stays retryable, after it fails the turn.
        // Anthropic can also report rate limits as a terminal `error` event
        // on a 200 body (no HTTP status to trigger `post_with_retry`), so a
        // pre-output rate-limit failure re-issues the whole request here.
        // A stalled stream retries same-protocol too (mid-stream included,
        // with an explicit notice) instead of failing the turn for a manual
        // `continue`.
        match crate::llm::stream::read_anthropic_stream(resp, sink.clone(), cancel, idle_timeout)
            .await
        {
            Ok(turn) => return Ok(turn),
            Err(e) if should_retry_stream_error(&*e, attempt, MAX_STREAM_RETRIES) => {
                let delay = backoff_delay(attempt, None);
                with_console(sink.is_some(), || {
                    eprintln!("[llm] rate limited: retrying in {:?}", delay)
                });
                tokio::time::sleep(delay).await;
            }
            Err(e) => {
                let msg = error_chain_message(&*e);
                if (should_retry_idle(&msg, attempt) || should_retry_dropped(&*e, attempt))
                    && !cancel.is_cancelled()
                {
                    note_idle_retry(&sink, attempt, &msg).await;
                    tokio::time::sleep(backoff_delay(attempt, None)).await;
                    continue;
                }
                return Err(e);
            }
        }
    }

    unreachable!()
}

/// Bounded same-protocol retries for a stalled SSE stream (`stream idle for
/// over …`). A stall is transient transport, not a verdict on the request —
/// re-issuing resumes the turn where a failure would force a manual
/// `continue`. Mid-stream stalls retry too (with an explicit notice): the
/// failed-turn alternative duplicates the partial output on `continue`
/// anyway. Never retries cancellations.
const MAX_IDLE_STREAM_RETRIES: u32 = 2;

fn should_retry_idle(message: &str, attempt: u32) -> bool {
    attempt < MAX_IDLE_STREAM_RETRIES
        && crate::llm::stream::is_stream_idle_error(message)
        && !message.contains("cancelled")
}

/// Same-protocol retry for a dead socket: keepalives surface a dropped
/// connection as a transport error from `chunk()`. Pre-output it is a pure
/// re-issue — nothing flowed, nothing to duplicate — so unlike mid-stream
/// failures it stays retryable. Shares the idle-retry budget. Never retries
/// cancellations.
fn should_retry_dropped(err: &(dyn std::error::Error + 'static), attempt: u32) -> bool {
    let message = error_chain_message(err);
    attempt < MAX_IDLE_STREAM_RETRIES
        && !crate::llm::streaming::is_mid_stream(err)
        && crate::llm::stream::is_dropped_connection(&message)
        && !message.contains("cancelled")
}

/// Visible retry notice: headless logs to stderr, TUI gets a transcript
/// `System` line (console IO is suppressed under a sink).
async fn note_idle_retry(sink: &Option<mpsc::Sender<SinkLine>>, attempt: u32, message: &str) {
    let delay = backoff_delay(attempt, None);
    with_console(sink.is_some(), || {
        eprintln!(
            "[llm] stream interrupted ({message}): retrying in {:?} (attempt {}/{})",
            delay,
            attempt + 2,
            MAX_IDLE_STREAM_RETRIES + 1,
        )
    });
    if let Some(sink) = sink {
        let _ = sink
            .send(SinkLine::System(format!(
                "stream interrupted ({message}); retrying automatically (attempt {}/{})",
                attempt + 2,
                MAX_IDLE_STREAM_RETRIES + 1,
            )))
            .await;
    }
}

/// Stream-phase retry gate for a terminal `error` event on a 200 body: only
/// a pre-output rate limit within budget. Mid-stream failures may have
/// already put partial text on the transcript, and anything else says
/// nothing transient about capacity.
fn should_retry_stream_error(
    err: &(dyn std::error::Error + 'static),
    attempt: u32,
    max_retries: u32,
) -> bool {
    attempt < max_retries
        && !crate::llm::streaming::is_mid_stream(err)
        && is_rate_limited(&error_chain_message(err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::Usage;

    #[test]
    fn error_chain_message_walks_sources() {
        // reqwest-style: outer Display names the context, the cause lives in
        // `source()`. `to_string()` alone drops it; the chain keeps it.
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        let chained = std::io::Error::new(std::io::ErrorKind::TimedOut, io);
        let msg = error_chain_message(&chained);
        assert!(msg.contains("connection refused"), "chain kept: {msg}");
    }

    #[derive(Clone)]
    struct MockModel;

    impl ModelClient for MockModel {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _with_tools: bool,
            _sink: Option<mpsc::Sender<SinkLine>>,
            _cancel: &(dyn CancellationSource + Send + Sync),
        ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
            Ok(Turn {
                message: ChatMessage::assistant("mock response"),
                usage: Some(Usage {
                    prompt_tokens: 3,
                    completion_tokens: 0,
                    cached_tokens: None,
                }),
                stop_reason: None,
            })
        }
    }

    #[test]
    fn retry_after_honors_seconds_dates_and_caps() {
        assert_eq!(retry_after("2"), Some(Duration::from_secs(2)));
        // A hostile header cannot park the turn for hours.
        assert_eq!(retry_after("99999"), Some(Duration::from_secs(120)));
        assert_eq!(retry_after("junk"), None);
        // HTTP-date form.
        let soon = chrono::Utc::now() + chrono::Duration::seconds(10);
        let formatted = soon.to_rfc2822();
        let parsed = retry_after(&formatted).expect("rfc2822 retry-after parses");
        assert!(parsed >= Duration::from_secs(1) && parsed <= Duration::from_secs(10));
        // Wire IMF-fixdate form (`... GMT`), which `parse_from_rfc2822`
        // alone rejects.
        let gmt = soon.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let parsed_gmt = retry_after(&gmt).expect("IMF-fixdate retry-after parses");
        assert!(parsed_gmt <= Duration::from_secs(10));
    }

    #[test]
    fn backoff_exponential_with_jitter_and_retry_after() {
        // ±25% jitter around the exponential base.
        for attempt in 0..3u32 {
            let delay = backoff_delay(attempt, None);
            let base = 500u64 * (1 << attempt);
            let low = (base as f64 * 0.75) as u64;
            let high = (base as f64 * 1.25) as u64;
            let ms = delay.as_millis() as u64;
            assert!(
                (low..=high).contains(&ms),
                "attempt {attempt}: {ms}ms outside [{low}, {high}]"
            );
        }
        // A server asking for longer wins over the base backoff.
        assert_eq!(
            backoff_delay(0, Some(Duration::from_secs(30))),
            Duration::from_secs(30)
        );
        // A tiny Retry-After (≤ base/2) is ignored in favor of the base.
        let delay = backoff_delay(0, Some(Duration::from_millis(100)));
        assert!(delay.as_millis() >= 375);
    }

    #[test]
    fn rate_limit_matches_provider_phrasings() {
        // The reported gateway shape: OpenAI-style JSON body proxied with a
        // non-429 status, so the status alone never triggered a retry.
        let body = r#"{"model":"muse-spark-1.3-contributor","error":{"param":null,"code":"rate_limit_exceeded","type":"rate_limit_error","message":"Error from provider (Console Go): Upstream request failed: [rate_limit_exceeded] Rate limit exceeded. Please retry after a brief wait."}}"#;
        assert!(is_rate_limited(body));
        assert!(is_rate_limited("rate_limit_error: Overloaded"));
        assert!(is_rate_limited("overloaded_error: Overloaded"));
        assert!(is_rate_limited("overloading is temporary"));
        assert!(is_rate_limited("rate-limit exceeded, retry soon"));
        assert!(is_rate_limited("429 Too Many Requests"));
        assert!(is_rate_limited("API error: Too many requests, slow down"));
        // Bare numeric code proxied with a non-429 status.
        assert!(is_rate_limited(r#"{"code":429,"message":"slow down"}"#));
        assert!(is_rate_limited("error 529: overloaded"));
        // Real failures that must NOT retry as rate limits.
        assert!(!is_rate_limited("API error: invalid api key"));
        assert!(!is_rate_limited(
            "API error: This model's maximum context length is 8192 tokens"
        ));
        // Standalone-code matching is digit-boundaried: token counts and
        // large numbers containing the digits must not false-positive.
        assert!(!is_rate_limited("API error: 14290 tokens used"));
        assert!(!is_rate_limited("maximum context length 429496 tokens"));
        assert!(!is_rate_limited("cancelled"));
        assert!(!is_rate_limited(""));
    }

    #[test]
    fn stream_error_retry_gate() {
        // Pre-output rate limit within budget retries.
        let err: Box<dyn std::error::Error + Send + Sync> =
            "rate_limit_error: Rate limit exceeded".into();
        assert!(should_retry_stream_error(&*err, 0, 3));
        // Budget exhausted stops retrying.
        assert!(!should_retry_stream_error(&*err, 3, 3));
        // Mid-stream rate limits never retry — partial text is already on
        // the transcript and a re-issued call would duplicate it.
        let mid: Box<dyn std::error::Error + Send + Sync> = Box::new(
            crate::llm::stream::MidStreamError("rate_limit_error: Overloaded".into()),
        );
        assert!(!should_retry_stream_error(&*mid, 0, 3));
        // Non-rate-limit failures never retry through this gate.
        let other: Box<dyn std::error::Error + Send + Sync> = "API error: invalid api key".into();
        assert!(!should_retry_stream_error(&*other, 0, 3));
    }

    #[test]
    fn idle_stall_retries_same_protocol_within_budget() {
        // Pre- and mid-stream stalls retry (explicit notice, no fallback);
        // anything else never does through this gate.
        assert!(should_retry_idle(
            "stream idle for over 300s; the provider stalled",
            0
        ));
        assert!(should_retry_idle(
            "stream idle for over 300s; the provider stalled",
            1
        ));
        assert!(!should_retry_idle(
            "stream idle for over 300s; the provider stalled",
            MAX_IDLE_STREAM_RETRIES
        ));
        assert!(!should_retry_idle("API error: invalid api key", 0));
        assert!(!should_retry_idle("cancelled", 0));
        assert!(!should_retry_idle(
            "stream idle for over 300s; cancelled",
            0
        ));
        assert!(crate::llm::stream::is_stream_idle_error(
            "stream idle for over 300s; x"
        ));
        assert!(!crate::llm::stream::is_stream_idle_error("API error: boom"));
    }

    #[test]
    fn dropped_connection_retries_pre_output_only() {
        // A dead socket before any output is a pure re-issue within budget.
        let err: Box<dyn std::error::Error + Send + Sync> =
            "error sending request: connection closed before message completed".into();
        assert!(should_retry_dropped(&*err, 0));
        assert!(should_retry_dropped(&*err, 1));
        assert!(!should_retry_dropped(&*err, MAX_IDLE_STREAM_RETRIES));
        // Mid-stream drops never retry here — partial output may already be
        // on the transcript and a re-issued call would duplicate it.
        let mid: Box<dyn std::error::Error + Send + Sync> = Box::new(
            crate::llm::stream::MidStreamError("connection reset by peer".into()),
        );
        assert!(!should_retry_dropped(&*mid, 0));
        // Anything else never retries through this gate.
        let other: Box<dyn std::error::Error + Send + Sync> = "API error: invalid api key".into();
        assert!(!should_retry_dropped(&*other, 0));
        let cancelled: Box<dyn std::error::Error + Send + Sync> =
            "connection reset; cancelled".into();
        assert!(!should_retry_dropped(&*cancelled, 0));
    }

    #[test]
    fn authenticated_request_applies_custom_headers_and_protects_auth() {
        let mut config = crate::llm::config::tests::test_cfg();
        config.api_key = "secret".to_string();
        config
            .extra_headers
            .insert("X-Gateway-Key".to_string(), "abc".to_string());
        config
            .extra_headers
            .insert("Authorization".to_string(), "hacked".to_string());
        config
            .extra_headers
            .insert("not a header".to_string(), "bad".to_string());
        let req = authenticated_request(
            config.client.get("http://localhost/v1/chat/completions"),
            &config,
        )
        .build()
        .unwrap();
        assert_eq!(
            req.headers()
                .get("x-gateway-key")
                .map(|v| v.to_str().unwrap()),
            Some("abc")
        );
        // The api key owns `authorization`; a custom header can't hijack it.
        assert_eq!(
            req.headers()
                .get("authorization")
                .map(|v| v.to_str().unwrap()),
            Some("Bearer secret")
        );
        assert!(!req.headers().contains_key("not a header"));
    }

    #[test]
    fn provider_headers_apply_under_global_extras() {
        // Provider-scoped file headers ride along; an explicit global /
        // env / CLI extra wins on collision.
        let mut config = crate::llm::config::tests::test_cfg();
        config.api_key = "secret".to_string();
        config
            .provider_headers
            .insert("X-Prov".to_string(), "prov".to_string());
        config
            .provider_headers
            .insert("X-Both".to_string(), "prov".to_string());
        config
            .extra_headers
            .insert("X-Both".to_string(), "global".to_string());
        let req = authenticated_request(
            config.client.get("http://localhost/v1/chat/completions"),
            &config,
        )
        .build()
        .unwrap();
        assert_eq!(
            req.headers().get("x-prov").map(|v| v.to_str().unwrap()),
            Some("prov")
        );
        assert_eq!(
            req.headers().get("x-both").map(|v| v.to_str().unwrap()),
            Some("global")
        );
    }

    #[tokio::test]
    async fn model_boundary_supports_deterministic_mock() {
        let turn = MockModel
            .complete(&[], false, None, &crate::agent::state::GlobalCancellation)
            .await
            .unwrap();
        assert_eq!(turn.message.content.as_deref(), Some("mock response"));
        assert_eq!(
            turn.usage,
            Some(Usage {
                prompt_tokens: 3,
                completion_tokens: 0,
                cached_tokens: None
            })
        );
    }
}
