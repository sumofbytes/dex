//! Shared HTTP transport core for any provider-style call: the [`HttpCall`]
//! seam, auth + header merging, the retry/backoff taxonomy, provider logging,
//! and error-message helpers. The chat/streaming path (`client`, `dispatch`,
//! `sse`) builds on this, so a new scheme (e.g. an Azure-style `api-key`
//! header) lands in one place: [`Provider::auth_scheme`].

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::time::Duration;

use serde_json::json;
use tokio::sync::mpsc;

use crate::llm::config::{merge_header_layers, resolve_credentials, ProviderEntry};
use crate::llm::provider::AuthScheme;
use crate::protocol::{ModelEvent, Provider};
use crate::runtime::console::with_console;

/// Everything one provider HTTP call needs, resolved once from `LlmConfig`
/// so the retry core below never sees config resolution. This is the seam
/// that makes the transport generic: another caller (jev, MCP) with its own
/// `HttpCall` gets the same auth, header-merge, backoff, and 401-refresh
/// behavior without importing `LlmConfig`.
#[derive(Clone)]
pub(crate) struct HttpCall {
    pub(crate) url: String,
    /// Model name, for request logging and the `/responses` protocol hint.
    pub(crate) model: String,
    /// One client per call, not per attempt: clones are an atomic bump, and
    /// a fresh build per retry would re-init TLS + pool each time.
    pub(crate) http: reqwest::Client,
    pub(crate) scheme: AuthScheme,
    pub(crate) api_key: String,
    pub(crate) account_id: Option<String>,
    /// Merged + validated extra headers (file layers < env < CLI;
    /// `authorization` never overridable). Computed once per call, not once
    /// per HTTP attempt.
    pub(crate) headers: Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>,
    /// Credential refresh material, present only when the provider can
    /// re-read its credentials on a 401 (Codex file tokens). `None` means a
    /// 401 is terminal.
    pub(crate) refresh: Option<(Provider, BTreeMap<String, ProviderEntry>)>,
    /// `Some(model)` when a NOT_FOUND/5xx on this call usually means a wire
    /// mismatch rather than a real error — the `/responses` case. Auth and
    /// rate-limit failures say nothing about the protocol, and the hint is
    /// only built when nothing pinned the protocol (`api_pinned`).
    pub(crate) hint_model: Option<String>,
    /// Reasoning-effort knob, spelled per wire protocol in the request body.
    pub(crate) thinking_effort: Option<String>,
    /// Idle budget for the stream reader, computed once per call.
    pub(crate) idle_timeout: Option<Duration>,
}

pub(crate) use crate::runtime::error::chain_message as error_chain_message;

/// Single owner of the "was this a cancellation?" wording gate. Cancellation
/// surfaces as message text on several paths (stream driver, tool IO, agent
/// loop), and every retry/fallback gate must exclude it — matching in one
/// place keeps a new cancellation phrasing from silently becoming retryable.
pub(crate) fn is_cancelled_message(message: &str) -> bool {
    message.contains("cancelled")
}

pub(crate) use dex_ai::transport::{
    authenticated_request, backoff_delay, error_head, is_rate_limited, retry_after,
    retryable_status,
};

pub(crate) fn merged_headers(
    global: &BTreeMap<String, String>,
    scoped: &BTreeMap<String, String>,
    extra: &BTreeMap<String, String>,
) -> Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)> {
    merge_header_layers(global, scoped, extra)
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("authorization"))
        .filter_map(|(name, value)| {
            Some((
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).ok()?,
                reqwest::header::HeaderValue::from_str(value).ok()?,
            ))
        })
        .collect()
}

pub(crate) fn provider_log(event: &str, detail: &str) {
    let Some(base) = crate::runtime::logging::data_home() else {
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
pub(crate) async fn post_with_retry(
    call: &HttpCall,
    body: &impl serde::Serialize,
    sink: Option<&mpsc::Sender<ModelEvent>>,
) -> Result<reqwest::Response, Box<dyn std::error::Error + Send + Sync>> {
    const MAX_HTTP_RETRIES: u32 = 3;
    let url = &call.url;
    // 401-refresh scratch: `None` borrows `call` (no clone on the common
    // path — materialized only when a refreshable provider actually
    // returns 401).
    let mut refreshed: Option<HttpCall> = None;
    for attempt in 0..=MAX_HTTP_RETRIES {
        let active: &HttpCall = refreshed.as_ref().unwrap_or(call);
        let request = active.http.post(url);
        crate::log!(
            Debug,
            "POST {url} (model {}, attempt {attempt})",
            call.model
        );
        let started = std::time::Instant::now();
        let resp = match authenticated_request(
            request,
            active.scheme,
            &active.api_key,
            active.account_id.as_deref(),
            &active.headers,
        )
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
            if let (reqwest::StatusCode::UNAUTHORIZED, Some((provider, entries))) =
                (status, active.refresh.as_ref())
            {
                if attempt < MAX_HTTP_RETRIES {
                    if let Ok((token, account)) = resolve_credentials(provider, entries) {
                        let mut next = refreshed.take().unwrap_or_else(|| call.clone());
                        next.api_key = token;
                        next.account_id = account;
                        refreshed = Some(next);
                        continue;
                    }
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
            // glm-5.3-flash on opencode's go endpoint) — point at the per-model override
            // instead of a bare body. Auth and rate-limit failures say
            // nothing about the protocol, and a pinned `api:` means the user
            // already decided.
            if url.ends_with("/responses")
                && (status == reqwest::StatusCode::NOT_FOUND || status.is_server_error())
            {
                if let Some(model) = &call.hint_model {
                    return Err(format!(
                        "API error: {} (hint: {model} may speak openai-completions; set DEX_MODEL_APIS={model}=openai-completions)",
                        body_text
                    )
                    .into());
                }
            }
            return Err(format!("API error: {}", body_text).into());
        }

        return Ok(resp);
    }

    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_chain_message_walks_sources() {
        // reqwest-style: outer Display names the context, the cause lives in
        // `source()`. `to_string()` alone drops it; the chain keeps it.
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "connection refused");
        let chained = std::io::Error::new(std::io::ErrorKind::TimedOut, io);
        let msg = error_chain_message(&chained);
        assert!(msg.contains("connection refused"), "chain kept: {msg}");
    }

    #[test]
    fn cancelled_and_error_head_helpers() {
        assert!(is_cancelled_message("cancelled"));
        assert!(is_cancelled_message("stream idle for over 300s; cancelled"));
        assert!(!is_cancelled_message("API error: boom"));
        assert_eq!(error_head("abcdef", 4), "abcd");
        assert_eq!(error_head("abc", 10), "abc");
        // Byte slicing `&s[..4]` would panic on this boundary; chars don't.
        assert_eq!(error_head("aébc…def", 4), "aébc");
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
}
