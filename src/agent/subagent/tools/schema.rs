use super::super::manager::AgentManager;
use super::super::model::AgentState;
use crate::llm::config::LlmConfig;
use crate::protocol::ApprovalRequest;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// The four model-facing delegation tools (§10, §24.3). Background is the
/// only spawn mode — no `run_in_background` flag to forget.
pub(crate) const DELEGATION_TOOLS: [&str; 4] = [
    "delegate",
    "delegate_output",
    "delegate_stop",
    "delegate_list",
];

/// `delegate_output`'s wait ceiling (§10.2): a bounded poll-wait, never an
/// unbounded block.
pub(crate) const MAX_WAIT_SECONDS: u64 = 120;

/// Max delegation nesting (Codex `DEFAULT_AGENT_MAX_DEPTH` parity, CC nests
/// to 3): a turn at this depth spawns no further children. Top-level runs
/// at depth 0; each delegation runs at parent depth + 1.
pub(crate) const MAX_AGENT_DEPTH: u32 = 3;

/// Sleep quantum of the `delegate_output` wait loop: steering sent during a
/// wait is acted on at most one interval after the wait returns (§10.2 — the
/// documented latency, not a claimed interrupt that cannot exist).
pub(crate) const WAIT_SLEEP: Duration = Duration::from_millis(250);

pub(crate) fn is_delegation(name: &str) -> bool {
    DELEGATION_TOOLS.contains(&name)
}

/// Lowercase status word for the §15 lifecycle lines and tool results
/// (`finished completed`, `finished timed out`).
pub(crate) fn status_word(state: AgentState) -> &'static str {
    match state {
        AgentState::Running => "running",
        AgentState::Completed => "completed",
        AgentState::Failed => "failed",
        AgentState::Cancelled => "cancelled",
        AgentState::TimedOut => "timed out",
    }
}

/// Set once at startup from the resolved [`crate::cli::Mode`]: only
/// daemon-backed processes (Serve, and Default which spawns one) link the
/// delegation tools into the schema. OneShot/no-daemon modes unregister
/// them at registration time (§10 — not a prompt hack); dispatch rejects
/// them anyway (no manager to spawn into).
static DAEMON_LINKED: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_daemon_linked(linked: bool) {
    DAEMON_LINKED.store(linked, Ordering::Relaxed);
}

/// Schema-level gate (§19 kill switch + §10 OneShot rule). Dispatch checks
/// the live turn context too — registration and enforcement stay separate.
pub(crate) fn delegation_enabled() -> bool {
    DAEMON_LINKED.load(Ordering::Relaxed) && std::env::var("DEX_SUBAGENTS").as_deref() != Ok("0")
}

/// The daemon-backed turn context a `delegate` call needs (the thin-client
/// side of the manager, §10.1). Built once per parent turn in
/// `run_turn_inner`; `None`-shaped absence is what makes delegation
/// impossible in OneShot/direct tool runs. Children under the depth cap get
/// a context of their own at depth + 1; at the cap they get none, so a
/// further `delegate` rejects at dispatch.
pub(crate) struct AgentTurnContext {
    /// Nesting depth: 0 for top-level turns, parent depth + 1 per delegation.
    pub(crate) depth: u32,
    pub(crate) session_id: String,
    /// The parent session's JSONL — the child file lands beside it (§16).
    pub(crate) session_path: PathBuf,
    /// The parent's resolved workspace (the child inherits it, §5).
    pub(crate) cwd: String,
    /// The parent's resolved model config; the child clones it and applies
    /// its definition's model override, if any (§13).
    pub(crate) config: Arc<LlmConfig>,
    pub(crate) manager: AgentManager,
    /// Snapshot of the parent session's "allow for session" keys (same
    /// scope as [`Console::approval_key`]) — children outlive the turn that
    /// spawned them, so the set they inherit is seeded at spawn (§12; the
    /// live-consulted variant is V1b).
    pub(crate) session_approvals: HashSet<String>,
    /// §12 V1b: the per-parent-turn channel that routes a child's
    /// [`ApprovalRequest`] into the daemon's `pending_approvals` as a
    /// labeled prompt (the parent turn's own tools use their own channel).
    /// `None` outside the daemon: the child console then carries a closed
    /// channel and `enforce_policy` fails closed (V1a detached auto-deny).
    pub(crate) child_approvals: Option<tokio::sync::mpsc::Sender<ApprovalRequest>>,
    /// §12 V1b: live "allow for session" lookup against the daemon's map,
    /// so a decision granted after a child spawned still applies to it.
    /// `None` outside the daemon.
    pub(crate) live_approvals: Option<crate::runtime::console::LiveApprovalCheck>,
}
