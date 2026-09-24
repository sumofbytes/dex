//! Shared async runtime and HTTP clients for synchronous and async callers.

use std::sync::OnceLock;
use std::time::Duration;

static SHARED_RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn shared_rt() -> &'static tokio::runtime::Runtime {
    SHARED_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("shared client runtime")
    })
}

pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    shared_rt().block_on(future)
}

static SHARED_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

pub fn shared_async_client() -> reqwest::Client {
    SHARED_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(concat!("dex-client/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

static STREAMING_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn stream_read_timeout() -> Option<Duration> {
    const DEFAULT_SECS: u64 = 330;
    const GRACE_SECS: u64 = 30;
    match std::env::var("DEX_STREAM_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
    {
        Some(0) => None,
        Some(seconds) => Some(Duration::from_secs(seconds + GRACE_SECS)),
        None => Some(Duration::from_secs(DEFAULT_SECS)),
    }
}

pub fn shared_streaming_client() -> reqwest::Client {
    STREAMING_CLIENT
        .get_or_init(|| {
            let mut builder = reqwest::Client::builder()
                .user_agent(concat!("dex-client/", env!("CARGO_PKG_VERSION")))
                .connect_timeout(Duration::from_secs(10));
            if let Some(timeout) = stream_read_timeout() {
                builder = builder.read_timeout(timeout);
            }
            builder
                .tcp_keepalive(Duration::from_secs(60))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}
