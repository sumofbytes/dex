//! Process-wide async runtime + shared HTTP clients: the single owner of
//! "how dex talks HTTP".
//!
//! One shared tokio runtime (sync callers: one-shot CLI, repl, `dex run`)
//! and one shared reqwest client per behavior — `Client::new()` initializes
//! a TLS backend + connection pool (~tens of ms), so per-request
//! construction is pure overhead; clones are an atomic bump.
//!
//! Policy: a differing *total timeout* rides per-request (`.timeout(...)` on
//! the builder), not as a new client. A custom build is only for differing
//! *behavior* — like `net_client` (redirect `none`: confinement is checked
//! pre-redirect) or `LlmConfig::http_client` (explicit
//! `DEX_HTTP_*_TIMEOUT_SECS` overrides).

use std::sync::OnceLock;
use std::time::Duration;

/// `User-Agent` sent on every outbound HTTP request (provider generations,
/// catalog fetches, daemon/TUI traffic). Both reference agents identify
/// themselves on the wire; a missing UA reads as bot traffic to some
/// gateways. A per-request `User-Agent` header still overrides this default.
pub(crate) const USER_AGENT: &str = concat!("dex/", env!("CARGO_PKG_VERSION"));

/// Socket keepalive interval for long-lived SSE streams. One place so the
/// shared streaming client and the explicit-timeout client stay in sync.
pub(crate) const TCP_KEEPALIVE_SECS: u64 = 60;

/// Per-read bound on streaming connections (see `shared_streaming_client`).
/// Sits above the app-level idle watchdog (300s for reasoning models) so it
/// only ever fires on the waits that watchdog cannot see — chiefly the
/// response-header wait of a re-issued attempt after a stream retry.
pub(crate) const STREAM_READ_TIMEOUT_SECS: u64 = 330;

/// Shared tokio runtime for sync callers (one-shot CLI, repl, `dex run`).
/// Four workers: TUI boot overlaps config/session/skills fetches plus the
/// git/event pollers here, and two workers head-of-line blocked on that fan-out.
static SHARED_RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn shared_rt() -> &'static tokio::runtime::Runtime {
    SHARED_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("shared runtime")
    })
}

/// Block on an async future from sync code (CLI/one-shot/`dex run` paths).
/// No async CLI plumbing needed per plan §5.
pub(crate) fn block_on<F>(fut: F) -> F::Output
where
    F: std::future::Future,
{
    shared_rt().block_on(fut)
}

/// Spawn an async task from sync code (TUI workers) onto the shared runtime.
/// Detached on drop (like threads), so failed launches don't wait.
pub(crate) fn spawn_task<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    shared_rt().spawn(fut)
}

static SHARED_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// Process-wide shared async client: 10s connect / 300s total.
/// `wait_until_ready` overrides to 2s per poll.
/// Serves daemon traffic, catalog fetches, release checks, MCP transports
/// (their own shorter per-request timeout governs), and tests on local
/// servers.
pub(crate) fn shared_async_client() -> reqwest::Client {
    SHARED_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

static STREAMING_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// Process-wide shared client for long-lived SSE streams (TUI↔daemon chat,
/// daemon↔provider generations). Connect timeout only: reqwest's total
/// `.timeout()` covers the whole streaming body, so any value here kills
/// turns/generations that run longer than it (`error decoding response body`
/// at exactly N seconds). A stalled stream ends via server keep-alive/EOF
/// or user cancel instead.
pub(crate) fn shared_streaming_client() -> reqwest::Client {
    STREAMING_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(Duration::from_secs(10))
                // Per-read bound (resets on every frame — NOT a total timeout,
                // which would kill long streams). Closes the one unbounded wait
                // in the streaming path: reqwest races this timeout against the
                // send/header phase of every POST, so a wedged connection —
                // typically a NAT/middlebox that ACKs TCP keepalive probes but
                // drops the stream — errors out instead of parking a re-issued
                // attempt (post-retry) forever with no terminal event. Healthy
                // streams are unaffected: every chunk resets the timer, and
                // provider silence is already bounded first by the app-level
                // idle watchdog (90s / 300s, DEX_STREAM_IDLE_TIMEOUT_SECS).
                .read_timeout(Duration::from_secs(STREAM_READ_TIMEOUT_SECS))
                // Socket-level keepalives: periodic probes let a silently
                // dropped connection (dead middlebox, hung peer) surface at
                // the TCP layer instead of parking indefinitely. Detection
                // still takes a few missed probes — the app-level idle
                // watchdog bounds application silence. Healthy-but-slow
                // providers are unaffected — keepalive ACKs carry no body
                // bytes, so the SSE idle timer still governs silence.
                .tcp_keepalive(Duration::from_secs(TCP_KEEPALIVE_SECS))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}
