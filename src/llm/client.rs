use serde_json::json;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::agent::state::CancellationSource;
use crate::core::console::with_console;
use crate::core::types::{ChatMessage, ChatRequest, SinkLine, StreamOptions};
use crate::llm::config::LlmConfig;
use crate::llm::http::{
    authenticated_request, backoff_delay, error_chain_message, error_head, is_cancelled_message,
    is_rate_limited, merged_headers, provider_log, retry_after, retryable_status,
};
use crate::llm::protocol::{
    chat_completions_messages, responses_input, responses_tools, tools_schema,
};
use crate::llm::sse::{read_anthropic_stream, read_responses_stream, read_stream, Turn};

/// Agent-loop model seam: `process_turn` is generic over this so tests run
/// deterministic doubles; the single production impl is `LlmConfig` (via
/// `dispatch::complete`). New provider behavior lands in the `call_*`
/// functions below, not behind this trait.
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
        crate::llm::dispatch::complete(self, messages, with_tools, sink, cancel)
            .await
            .map_err(|e| Box::<dyn std::error::Error + Send + Sync>::from(error_chain_message(&*e)))
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
    let headers = merged_headers(config);
    // One client per call, not per attempt: clones are an atomic bump, and a
    // fresh build per retry would re-init TLS + pool each time.
    let http = config.http_client();
    // 401-refresh scratch: `None` borrows `config` (no clone on the common
    // path — the old code cloned the whole config per call even though only
    // the 401 path mutates). Materialized only when a refreshable provider
    // actually returns 401.
    let mut refreshed: Option<LlmConfig> = None;
    for attempt in 0..=MAX_HTTP_RETRIES {
        let active: &LlmConfig = refreshed.as_ref().unwrap_or(config);
        let request = http.post(url);
        crate::log!(
            Debug,
            "POST {url} (model {}, attempt {attempt})",
            config.model
        );
        let started = std::time::Instant::now();
        let resp = match authenticated_request(request, active, &headers)
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
                && active.provider.credentials_refreshable()
                && attempt < MAX_HTTP_RETRIES
            {
                if let Ok((token, account)) = crate::llm::auth::resolve_credentials(
                    &active.provider,
                    &active.provider_entries,
                ) {
                    let mut next = refreshed.take().unwrap_or_else(|| config.clone());
                    next.api_key = token;
                    next.account_id = account;
                    refreshed = Some(next);
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
            crate::log!(Warn, "api error {status}: {}", error_head(&body_text, 400));
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

/// One streaming POST + stream-phase retry core shared by all three wire
/// protocols. Each attempt re-issues the whole request via `post_with_retry`
/// (its own HTTP-phase budget); this loop owns only the stream-phase budget:
/// a stall or drop before the first delta re-issues same-protocol (with an
/// explicit notice) instead of failing the turn back for a manual
/// `continue`, while a mid-stream failure (marked by the driver itself, so
/// the protocol-fallback gate won't duplicate partial text) fails the turn.
/// `stream_rate_limit_retries` re-issues a pre-output rate limit reported as
/// a terminal `error` event on a 200 body (Anthropic's shape — no HTTP status
/// for `post_with_retry` to see); the OpenAI protocols pass 0 because their
/// rate limits arrive as HTTP statuses handled one layer down. Stall/drop
/// recovery draws from its own `stall_attempt` budget, not the rate-limit
/// index, so mixed failures starve neither budget.
async fn run_streaming_call(
    config: &LlmConfig,
    url: &str,
    body: &impl serde::Serialize,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &(dyn CancellationSource + Send + Sync),
    stream_rate_limit_retries: u32,
    read: impl for<'a> Fn(
        reqwest::Response,
        Option<mpsc::Sender<SinkLine>>,
        &'a (dyn CancellationSource + Send + Sync),
        Option<Duration>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Turn, Box<dyn std::error::Error + Send + Sync>>>
                + Send
                + 'a,
        >,
    >,
) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
    let idle_timeout = idle_timeout(config);
    let mut rate_attempt = 0u32;
    let mut stall_attempt = 0u32;
    loop {
        let resp = post_with_retry(config, url, body, sink.as_ref()).await?;
        match read(resp, sink.clone(), cancel, idle_timeout).await {
            Ok(turn) => return Ok(turn),
            Err(e) if should_retry_stream_error(&*e, rate_attempt, stream_rate_limit_retries) => {
                let delay = backoff_delay(rate_attempt, None);
                with_console(sink.is_some(), || {
                    eprintln!("[llm] rate limited: retrying in {:?}", delay)
                });
                rate_attempt += 1;
                tokio::time::sleep(delay).await;
            }
            Err(e) => {
                let msg = error_chain_message(&*e);
                if (should_retry_idle(&*e, stall_attempt)
                    || should_retry_dropped(&*e, stall_attempt))
                    && !cancel.is_cancelled()
                {
                    note_idle_retry(&sink, stall_attempt, &msg).await;
                    tokio::time::sleep(backoff_delay(stall_attempt, None)).await;
                    stall_attempt += 1;
                    continue;
                }
                return Err(e);
            }
        }
    }
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
        messages: chat_completions_messages(messages),
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
    run_streaming_call(
        config,
        &format!("{}/chat/completions", config.base_url),
        &req,
        sink,
        cancel,
        0, // rate limits arrive as HTTP statuses, retried in `post_with_retry`
        |resp, sink, cancel, idle| Box::pin(read_stream(resp, sink, cancel, idle)),
    )
    .await
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
    run_streaming_call(
        config,
        &format!("{}/responses", config.base_url),
        &body,
        sink,
        cancel,
        0, // rate limits arrive as HTTP statuses, retried in `post_with_retry`
        |resp, sink, cancel, idle| Box::pin(read_responses_stream(resp, sink, cancel, idle)),
    )
    .await
}

pub(crate) async fn call_anthropic_messages(
    config: &LlmConfig,
    messages: &[ChatMessage],
    with_tools: bool,
    sink: Option<mpsc::Sender<SinkLine>>,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
    let body = crate::llm::anthropic::messages_body(config, messages, with_tools);
    // Anthropic can report rate limits as a terminal `error` event on a 200
    // body (no HTTP status to trigger `post_with_retry`), so a pre-output
    // rate-limit failure re-issues the whole request here. In practice the
    // loop only runs after the inner one already returned a 200, and a 200
    // body carries no `Retry-After` — hence `backoff_delay(_, None)` in the
    // shared core.
    const MAX_STREAM_RETRIES: u32 = 3;
    run_streaming_call(
        config,
        &crate::llm::anthropic::messages_url(&config.base_url),
        &body,
        sink,
        cancel,
        MAX_STREAM_RETRIES,
        |resp, sink, cancel, idle| Box::pin(read_anthropic_stream(resp, sink, cancel, idle)),
    )
    .await
}

/// Bounded same-protocol retries for a stalled or dropped stream before any
/// output flowed (`stream idle for over …` / transport marker). A stall is
/// transient transport, not a verdict on the request — re-issuing resumes
/// the turn where a failure would force a manual `continue`. Only pre-output
/// failures retry: once output has flowed the partial text is already on the
/// transcript and a re-issue would duplicate it, so mid-stream failures fail
/// the turn for a manual `continue`. Never retries cancellations.
const MAX_IDLE_STREAM_RETRIES: u32 = 2;

/// Idle budget for this turn, computed once per call: the explicit env
/// override wins, otherwise reasoning-capable models get the patient budget
/// (see `stream_idle_timeout_for`). One helper so the three protocol entry
/// points share the catalog lookup instead of repeating it.
fn idle_timeout(config: &LlmConfig) -> Option<Duration> {
    crate::llm::sse::stream_idle_timeout_for(&config.model, config.thinking_effort.is_some())
}

fn should_retry_idle(err: &(dyn std::error::Error + 'static), attempt: u32) -> bool {
    attempt < MAX_IDLE_STREAM_RETRIES
        && !crate::llm::sse::is_mid_stream(err)
        && crate::llm::sse::is_stream_idle_error(&error_chain_message(err))
        && !is_cancelled_message(&error_chain_message(err))
}

/// Same-protocol retry for a dead socket: keepalives surface a dropped
/// connection as a transport error from `chunk()`, which `run_sse` marks
/// with [`StreamTransportError`] pre-output. A marked failure is a pure
/// re-issue — nothing flowed, nothing to duplicate — so unlike mid-stream
/// failures it stays retryable. Detection is provenance, not wording: no
/// message wording is matched, so provider error text can never trip this
/// gate. Shares the idle-retry budget, not the rate-limit one. Never retries
/// cancellations.
fn should_retry_dropped(err: &(dyn std::error::Error + 'static), attempt: u32) -> bool {
    attempt < MAX_IDLE_STREAM_RETRIES
        && !crate::llm::sse::is_mid_stream(err)
        && crate::llm::sse::is_transport_error(err)
        && !is_cancelled_message(&error_chain_message(err))
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
        && !crate::llm::sse::is_mid_stream(err)
        && is_rate_limited(&error_chain_message(err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::Usage;

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
            crate::llm::sse::MidStreamError("rate_limit_error: Overloaded".into()),
        );
        assert!(!should_retry_stream_error(&*mid, 0, 3));
        // Non-rate-limit failures never retry through this gate.
        let other: Box<dyn std::error::Error + Send + Sync> = "API error: invalid api key".into();
        assert!(!should_retry_stream_error(&*other, 0, 3));
    }

    #[test]
    fn idle_stall_retries_same_protocol_within_budget() {
        // Pre-output stalls retry (explicit notice, no fallback); mid-stream
        // stalls fail the turn — partial text is already on the transcript
        // and a re-issue would duplicate it. Anything else never retries
        // through this gate.
        let stall: Box<dyn std::error::Error + Send + Sync> =
            "stream idle for over 300s; the provider stalled".into();
        assert!(should_retry_idle(&*stall, 0));
        assert!(should_retry_idle(&*stall, 1));
        assert!(!should_retry_idle(&*stall, MAX_IDLE_STREAM_RETRIES));
        let other: Box<dyn std::error::Error + Send + Sync> = "API error: invalid api key".into();
        assert!(!should_retry_idle(&*other, 0));
        let cancelled: Box<dyn std::error::Error + Send + Sync> = "cancelled".into();
        assert!(!should_retry_idle(&*cancelled, 0));
        let idle_cancelled: Box<dyn std::error::Error + Send + Sync> =
            "stream idle for over 300s; cancelled".into();
        assert!(!should_retry_idle(&*idle_cancelled, 0));
        // Mid-stream stall: output already flowed, so no retry.
        let mid: Box<dyn std::error::Error + Send + Sync> = Box::new(
            crate::llm::sse::MidStreamError("stream idle for over 300s; stalled".into()),
        );
        assert!(!should_retry_idle(&*mid, 0));
        assert!(crate::llm::sse::is_stream_idle_error(
            "stream idle for over 300s; x"
        ));
        assert!(!crate::llm::sse::is_stream_idle_error("API error: boom"));
    }

    #[test]
    fn dropped_connection_retries_pre_output_only() {
        use crate::llm::sse::StreamTransportError;
        // A marked transport failure before any output is a pure re-issue
        // within budget — detection is provenance, not wording.
        let err: Box<dyn std::error::Error + Send + Sync> =
            Box::new(StreamTransportError("connection reset by peer".into()));
        assert!(should_retry_dropped(&*err, 0));
        assert!(should_retry_dropped(&*err, 1));
        assert!(!should_retry_dropped(&*err, MAX_IDLE_STREAM_RETRIES));
        // Mid-stream drops never retry here — partial output may already be
        // on the transcript and a re-issued call would duplicate it.
        let mid: Box<dyn std::error::Error + Send + Sync> = Box::new(
            crate::llm::sse::MidStreamError("connection reset by peer".into()),
        );
        assert!(!should_retry_dropped(&*mid, 0));
        // Anything else never retries through this gate — including the bare
        // wording with no transport marker (provider error text is not a
        // dead socket).
        let other: Box<dyn std::error::Error + Send + Sync> = "API error: invalid api key".into();
        assert!(!should_retry_dropped(&*other, 0));
        let worded: Box<dyn std::error::Error + Send + Sync> = "connection reset by peer".into();
        assert!(!should_retry_dropped(&*worded, 0));
        let cancelled: Box<dyn std::error::Error + Send + Sync> =
            Box::new(StreamTransportError("connection reset; cancelled".into()));
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
        let headers = merged_headers(&config);
        let req = authenticated_request(
            config
                .http_client()
                .get("http://localhost/v1/chat/completions"),
            &config,
            &headers,
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
    fn provider_headers_beat_global_file_but_not_env_extras() {
        // AGENTS.md precedence on the wire: provider-scoped file headers
        // beat the global file table per key, env/CLI extras beat both.
        let mut config = crate::llm::config::tests::test_cfg();
        config.api_key = "secret".to_string();
        config
            .provider_headers
            .insert("X-Prov".to_string(), "prov".to_string());
        config
            .provider_headers
            .insert("X-Both".to_string(), "prov".to_string());
        config
            .global_headers
            .insert("X-Both".to_string(), "global".to_string());
        config
            .extra_headers
            .insert("X-Prov".to_string(), "env".to_string());
        let headers = merged_headers(&config);
        let req = authenticated_request(
            config
                .http_client()
                .get("http://localhost/v1/chat/completions"),
            &config,
            &headers,
        )
        .build()
        .unwrap();
        // Provider-scoped file beats global-file on collision…
        assert_eq!(
            req.headers().get("x-both").map(|v| v.to_str().unwrap()),
            Some("prov")
        );
        // …but an env/CLI/per-request extra beats the provider-scoped one.
        assert_eq!(
            req.headers().get("x-prov").map(|v| v.to_str().unwrap()),
            Some("env")
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
