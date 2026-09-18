//! Child-agent approvals (§12 V1b): park requests, broadcast them,
//! and deny-after-timeout so no child strands forever.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::agent::subagent::{AgentId, AgentManager};
use crate::core::types::{ApprovalDecision, ApprovalRequest};
use crate::protocol::{StreamEnvelope, StreamEvent};

use super::{journal_event, lock_map, DaemonState, PendingApproval};

/// Five-minute silence denies a parked child-agent approval (§12 V1b): a
/// prompt nobody answers must never strand a child forever, and the denial
/// is recorded in the child's transcript like any other refusal.
const CHILD_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// §12 V1b: consume a child's approval requests. Each one parks in the
/// session's `pending_approvals` under the child's id, is journaled and
/// broadcast as a labeled `ApprovalRequired` (so the TUI renders "explorer
/// wants to run bash: …" and stays answerable after the parent turn ends),
/// and arms a five-minute deny timer.
pub(crate) async fn child_approval_bridge(
    state: Arc<DaemonState>,
    session_id: String,
    manager: AgentManager,
    mut rx: mpsc::Receiver<ApprovalRequest>,
) {
    while let Some(request) = rx.recv().await {
        // The session's manager was dropped (deleted/reset): deny so the
        // child's blocked tool call unwinds instead of parking forever.
        if !lock_map(&state.agents).contains_key(&session_id) {
            let _ = request.response.try_send(ApprovalDecision::Deny);
            continue;
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        // The label prefers the console-stamped name; fall back to the
        // manager registry (same definition, single source).
        let agent = request
            .agent
            .clone()
            .or_else(|| manager.definition_name(&AgentId(request.agent_id.clone()?)));
        let parked = PendingApproval {
            session_id: session_id.clone(),
            response: request.response,
            name: request.name.clone(),
            input: request.input.clone(),
            agent_id: request.agent_id.clone(),
            agent: agent.clone(),
        };
        let replaced = lock_map(&state.pending_approvals).insert(request_id.clone(), parked);
        if let Some(stale) = replaced {
            let _ = stale.response.try_send(ApprovalDecision::Deny);
        }
        // Journal + broadcast: with no turn streaming, the client's event
        // poll is the delivery path (§15: child events ride ?since=).
        let env = StreamEnvelope {
            seq: state.next_seq(&session_id),
            event: StreamEvent::ApprovalRequired {
                request_id: request_id.clone(),
                name: request.name.clone(),
                input: request.input.clone(),
                agent: agent.clone(),
            },
        };
        journal_event(&state, &session_id, env.seq, &env.event);
        state.broadcast_event(&session_id, &env);
        // The five-minute denial timer. Resolution removes the entry first,
        // so an answered prompt never double-denies.
        let timer_state = state.clone();
        let timer_sid = session_id.clone();
        let timer_id = request_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(CHILD_APPROVAL_TIMEOUT).await;
            let pending = {
                let mut pending = timeout_pending(&timer_state);
                pending.remove(&timer_id)
            };
            let Some(pending) = pending else {
                return;
            };
            if pending.session_id != timer_sid {
                // Restore: cross-session entries belong to their session.
                timeout_pending(&timer_state).insert(timer_id, pending);
                return;
            }
            write_approval_audit(
                &timer_sid,
                &timer_id,
                &pending.name,
                &pending.input,
                "deny",
                "timeout",
                pending.agent.as_deref(),
            );
            let _ = pending.response.send(ApprovalDecision::Deny).await;
        });
    }
}

/// Locked peek/remove helper for the timeout timer (keeps the borrow out of
/// the async block).
pub(crate) fn timeout_pending(
    state: &Arc<DaemonState>,
) -> std::sync::MutexGuard<'_, HashMap<String, PendingApproval>> {
    lock_map(&state.pending_approvals)
}

/// One audit row for an approval resolution (the same shape `approve`
/// writes; `actor` distinguishes the remote client from the timer).
pub(crate) fn write_approval_audit(
    session_id: &str,
    request_id: &str,
    tool: &str,
    input: &str,
    decision: &str,
    actor: &str,
    agent: Option<&str>,
) {
    let Some(base) = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })
    else {
        return;
    };
    let path = base.join("dex/audit.jsonl");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&input, &mut hasher);
    let input_hash = std::hash::Hasher::finish(&hasher);
    let mut record = serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "session_id": session_id,
        "request_id": request_id,
        "tool": tool,
        "input_hash": format!("{:016x}", input_hash),
        "decision": decision,
        "actor": actor,
    });
    if let Some(agent) = agent {
        record["agent"] = serde_json::json!(agent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let mut line = record.to_string();
        line.push('\n');
        let _ = std::io::Write::write_all(&mut file, line.as_bytes());
    }
}
