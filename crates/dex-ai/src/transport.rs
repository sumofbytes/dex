//! Reusable HTTP authentication and retry policy for provider clients.
use crate::AuthScheme;
use std::time::Duration;

/// Char-boundaried head of an error body for logs and error values: some
/// providers return whole HTML pages. Byte slicing would panic on a
/// non-char boundary; `chars().take` never does.
pub fn error_head(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

pub fn authenticated_request(
    request: reqwest::RequestBuilder,
    scheme: AuthScheme,
    api_key: &str,
    account_id: Option<&str>,
    headers: &[(reqwest::header::HeaderName, reqwest::header::HeaderValue)],
) -> reqwest::RequestBuilder {
    let request = scheme.apply(request, api_key, account_id);
    let mut request = request;
    for (name, value) in headers {
        request = request.header(name.clone(), value.clone());
    }
    request
}

pub fn retryable_status(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 408 || status.as_u16() == 429 || status.is_server_error()
}

/// Provider wording for "you hit a rate limit, retry after a brief wait".
/// Matched on lowercase against error bodies/messages (HTTP error JSON like
/// `{"error":{"code":"rate_limit_exceeded",...}}` as well as stream-phase
/// `Fail` messages like `rate_limit_error: ...`). Gateways don't always use
/// a 429 status for these, so the body — not just the status — decides.
pub fn is_rate_limited(message: &str) -> bool {
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
pub fn retry_after(value: &str) -> Option<Duration> {
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
pub fn backoff_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
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
pub const RETRY_AFTER_CAP_SECS: u64 = 120;
