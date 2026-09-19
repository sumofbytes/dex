//! Process-wide manager singleton plus the sync cache readers used by the
//! turn loop (`tools_schema`, compaction budget, approval paths).
//!
//! The global manager initializes once at daemon bootstrap with two
//! background tasks: a best-effort refresh and a 60s liveness sweeper.
//! Every read here is sync and never blocks: bounded `try_read` spins with
//! a safe fallback (`None`/empty means "unavailable", never "unconfigured").

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::RwLock;

use crate::agent::state::CancellationSource;
use crate::protocol::ToolDefinition;

use super::manager::{McpManager, ServerStatus};

static GLOBAL: OnceLock<Arc<McpManager>> = OnceLock::new();

/// Serialize tests that mutate the MCP environment: test threads share one
/// process-global env table, so a set_var window in one test can flip an
/// env read in another (observed: schema-cap test vs concurrent rebuilds).
/// Tokio mutex: guards are held across `.await` (handler calls that read
/// the env), which a std mutex forbids under clippy.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) fn global_manager() -> Arc<McpManager> {
    GLOBAL
        .get_or_init(|| {
            let mgr = McpManager::from_env();
            // Best-effort background connect; schema merges whatever is cached.
            let clone = Arc::clone(&mgr);
            crate::client::http::spawn_task(async move { clone.refresh().await });
            // Liveness sweeper: ping each client every 60s so a server that
            // died mid-session goes `down` before the next turn uses it.
            let sweep = Arc::clone(&mgr);
            crate::client::http::spawn_task(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    sweep.sweep_once().await;
                }
            });
            mgr
        })
        .clone()
}

/// Bounded spin on `try_read`: every writer holds its guard for a bare
/// swap (never across an await), so a contended first try succeeds within
/// a few yields. A single `try_read().unwrap_or_default()` undercounts the
/// compaction budget to 0 under contention and skips a needed compaction —
/// or drops the tools from one request's schema. The spin keeps the
/// never-block contract (bounded yields) while making the fallback
/// ~unreachable.
fn spin_read<T: Clone>(lock: &RwLock<T>) -> Option<T> {
    for _ in 0..16 {
        if let Ok(guard) = lock.try_read() {
            return Some(guard.clone());
        }
        std::thread::yield_now();
    }
    None
}

/// Cached MCP tools for `tools_schema()` — never blocks, never fails.
/// Clones the `Arc`, not the defs.
pub(crate) fn cached_tools() -> Arc<[ToolDefinition]> {
    GLOBAL
        .get()
        .and_then(|m| spin_read(&m.cached_tools))
        .unwrap_or_else(|| Arc::new([]))
}

/// Token cost of the cached MCP schema slice, for the compaction budget.
/// Precomputed at cache-swap time — a cached load, never a re-serialize.
/// Never blocks the loop; contention spins (see `spin_read`) instead of
/// returning 0 and skipping a needed compaction.
pub(crate) fn cached_schema_tokens() -> u64 {
    GLOBAL
        .get()
        .and_then(|m| spin_read(&m.cached_schema_tokens))
        .unwrap_or_default()
}

/// Tools dropped from the schema by the cap (0 when everything fits).
pub(crate) fn cached_truncated() -> usize {
    GLOBAL
        .get()
        .and_then(|m| spin_read(&m.cached_truncated))
        .unwrap_or_default()
}

/// Ephemeral MCP status line for the turn-loop compaction budget: priced,
/// never stored. `None` when the manager was never initialized (no MCP
/// tools in the schema then either) or a lock is contended — the budget
/// probe must never block the loop or spawn the background refresh.
/// One-line MCP status summary (`MCP servers: gh (3 tools), db (down) …`)
/// plus the schema-cap drop count. Presentation lives with the owner; the
/// budget probe in `agent` counts the line without storing it.
pub(crate) fn status_line(statuses: &[ServerStatus]) -> Option<String> {
    if statuses.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = statuses
        .iter()
        .map(|s| {
            if s.state.as_str() == "up" {
                format!("{} ({} tools)", s.name, s.tools)
            } else {
                format!("{} (down)", s.name)
            }
        })
        .collect();
    let dropped = cached_truncated();
    if dropped > 0 {
        parts.push(format!("{dropped} tools omitted (schema cap)"));
    }
    Some(format!("MCP servers: {}", parts.join(", ")))
}

pub(crate) fn ephemeral_line() -> Option<String> {
    let mgr = GLOBAL.get()?;
    // Zero-config managers have no line to print (the cached-status path
    // below still yields `Some(vec![])` there); keep this gate here only.
    if mgr.configs.is_empty() {
        return None;
    }
    try_snapshot(mgr).and_then(|st| status_line(&st))
}

pub(crate) async fn call_global(
    name: &str,
    args: &serde_json::Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> Result<String, String> {
    global_manager().call_tool(name, args, cancel).await
}

/// Sync snapshot of per-server status for the `/mcp` slash command: never
/// blocks, never initializes the manager (a slash handler must not spawn
/// the background refresh). `None` when uninitialized or contended — the
/// caller renders that as "unavailable" rather than an empty server list,
/// which would wrongly imply no MCP is configured.
pub(crate) fn cached_statuses() -> Option<Vec<ServerStatus>> {
    let mgr = GLOBAL.get()?;
    try_snapshot(mgr)
}

/// Lock-free snapshot of `status_list` via `try_read`: `None` when a lock is
/// contended — callers must treat that as "unavailable", never as an empty
/// server list (which would wrongly imply no MCP is configured).
fn try_snapshot(mgr: &McpManager) -> Option<Vec<ServerStatus>> {
    let clients = spin_read(&mgr.clients)?;
    let tools = spin_read(&mgr.cached_tools)?;
    let down = spin_read(&mgr.down)?;
    Some(McpManager::status_list(
        &mgr.configs,
        &clients,
        &tools,
        &down,
    ))
}
