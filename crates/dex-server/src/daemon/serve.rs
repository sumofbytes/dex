//! Daemon bootstrap: start the HTTP server on a pre-bound listener.

use std::net::TcpListener;
use std::time::Duration;

use super::state::DaemonState;

/// Start the daemon HTTP server on an already-bound listener.
pub async fn run_daemon(listener: TcpListener) -> Result<(), Box<dyn std::error::Error>> {
    // MCP bootstrap: connects servers in the background and merges their
    // tools into the schema cache (plus the 60s liveness sweeper). Without
    // this the manager stays uninitialized and `mcp__*` tools never exist.
    crate::mcp::global_manager();
    // Extension bootstrap: loads Lua extensions in the background and merges
    // their tools into the schema cache (plan §11 P0).
    crate::extensions::global_manager();
    let state = std::sync::Arc::new(DaemonState::new());
    // Rebuild in-memory state from the persisted JSONL on a background thread:
    // scanning every session (headers, event seqs, turn state) costs ~0.5s
    // with a few thousand sessions and would delay /health and TUI first
    // paint. Fresh sessions use uuid ids so they never collide with rebuilt
    // ones; `seed_seq` takes the max so a racing turn can't rewind a counter.
    // A panicking rebuild must not fail silent (the registry would stay
    // partial behind an `ok` health check): log it, and `rebuild_complete`
    // in `/health` stays false.
    let warm = state.clone();
    tokio::spawn(async move {
        warm.rebuild_async().await;
    });

    // Fresh installs have no models.dev catalog until `dex update --models`
    // runs, which silently degrades context windows and `/model` autocomplete.
    // Best-effort background fetch on first start; never blocks or fails the
    // daemon. One retry after 5 minutes covers a laptop waking offline, since
    // a daemon can outlive the outage; `dex update --models` always works too.
    if crate::llm::config::catalog_cache_missing() {
        tokio::spawn(async {
            // The error type is Box<dyn Error>, which is not Send: report it
            // and drop it before any further await so the future stays Send.
            let failed = match crate::llm::config::refresh_models_cache_async().await {
                Ok(()) => false,
                Err(e) => {
                    eprintln!("note: models.dev catalog fetch failed ({e}); retrying in 5 minutes");
                    true
                }
            };
            if failed {
                tokio::time::sleep(Duration::from_secs(5 * 60)).await;
                if let Err(e) = crate::llm::config::refresh_models_cache_async().await {
                    eprintln!(
                        "note: models.dev catalog still missing ({e}); run `dex update --models`"
                    );
                }
            }
        });
    }

    let app = super::server::router(state.clone());

    // Note: no startup announcement here. The headless `dex serve` caller
    // prints one; the embedded (`dex` default) daemon shares the process
    // with the TUI and must stay silent — anything printed before the
    // alt-screen is entered lingers in scrollback after quit and reads as
    // if a daemon were still listening.

    // tokio refuses blocking fds; the std listener must be non-blocking
    // before registration.
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;

    // Headless exit path: ctrl-C stops serving, cancels + joins every
    // session's children (§14: no orphaned tokio task survives this exit),
    // then exits. A spawned watcher — not `with_graceful_shutdown` — so a
    // still-connected SSE client cannot hold the process open while it
    // drains; once children are joined nothing is lost by exiting hard.
    {
        let state_for_exit = state.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                crate::extensions::fire_lifecycle_event(
                    "runtime.stop",
                    serde_json::json!({}),
                    &crate::runtime::cancel::GlobalCancellation,
                )
                .await;
                state_for_exit.shutdown_agents().await;
                std::process::exit(0);
            }
        });
    }

    axum::serve(listener, app).await?;

    Ok(())
}
