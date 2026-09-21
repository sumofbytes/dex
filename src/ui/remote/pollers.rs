use super::state::RemoteApp;
use super::state::WorkerMessage;
use crate::client::http::DaemonClient;
use crate::protocol::ApprovalDecision;

use crate::protocol::StreamEvent;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// How often an idle TUI re-polls `GET /api/git` for the footer. The daemon
/// caches branch/dirty for 5s, so a 2s poll costs at most one `git` spawn
/// per 5s — cheap enough to catch an external `git checkout` within seconds
/// without the per-frame (~10-30ms) cost of `git branch + git status`.
pub(crate) const GIT_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) fn apply_git_info(remote: &mut RemoteApp, info: &crate::protocol::GitInfo) -> bool {
    let changed =
        remote.app.git_branch != info.git_branch || remote.app.git_dirty != info.git_dirty;
    if changed {
        remote.app.git_branch = info.git_branch.clone();
        remote.app.git_dirty = info.git_dirty;
    }
    changed
}

/// Background git poll off the UI thread (Phase 5): task polls every 2s via
/// `get_git_async`, pushes into the worker channel; UI loop only applies.
/// Daemon 5s cache stays; per-frame cost zero even when busy.
/// How often the idle TUI polls the event journal while child agents may be
/// live (§15 V1b). Same cost class as the git poller: one HTTP fetch per
/// interval, zero per-frame work.
const EVENTS_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Idle journal poll (§15 V1b): child-agent lifecycle lines, labeled child
/// approvals, and wake turns are journaled outside any turn's SSE stream, so
/// the client learns about them through `GET /events?since=<cursor>`. The
/// poller starts only after the session shows child activity and pauses
/// while a turn streams (that stream carries the live rows; its final
/// cursor arrives via `WorkerMessage::Cursor`).
pub(crate) fn spawn_events_poller(
    client: DaemonClient,
    session_id: String,
    tx: mpsc::Sender<WorkerMessage>,
    live: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    cursor: Arc<AtomicU64>,
) {
    crate::runtime::http::spawn_task(async move {
        // No tip seed here: the boot flow seeds the cursor (§4 — the replay
        // drain advances it per page, the local-JSONL path from the local
        // journal tip), so this task only advances it past rows it serves.
        // Replay holds `busy` so a slow drain can't race the first polls.
        loop {
            tokio::time::sleep(EVENTS_POLL_INTERVAL).await;
            if busy.load(Ordering::SeqCst) || !live.load(Ordering::SeqCst) {
                continue;
            }
            let since = cursor.load(Ordering::SeqCst);
            let Ok(resp) = client.events_async(&session_id, since).await else {
                continue;
            };
            for env in resp.events {
                // A parked parent-turn approval is dead (denied at its
                // turn's teardown); a child approval stays answerable and
                // must surface. Replays skip the dead ones the same way.
                let parked_parent = matches!(
                    &env.event,
                    StreamEvent::ApprovalRequired { agent: None, .. }
                );
                if parked_parent {
                    continue;
                }
                if tx.send(WorkerMessage::Stream(env.event)).await.is_err() {
                    return;
                }
            }
            // next_seq counts raw journal rows (even unknown types), so the
            // cursor keeps moving past anything this client skips.
            cursor.fetch_max(resp.next_seq, Ordering::SeqCst);
        }
    });
}

pub(crate) fn spawn_git_poller(client: DaemonClient, tx: tokio::sync::mpsc::Sender<WorkerMessage>) {
    crate::runtime::http::spawn_task(async move {
        loop {
            tokio::time::sleep(GIT_REFRESH_INTERVAL).await;
            match client.get_git_async().await {
                Ok(info) => {
                    let _ = tx.send(WorkerMessage::Git(info)).await;
                }
                Err(_) => continue,
            }
        }
    });
}

/// One-off footer refresh without blocking the UI thread: fetches
/// `GET /api/git` on the shared runtime and forwards the result through the
/// worker channel (daemon 5s cache keeps it cheap). Used at turn end so
/// tool mutations show up immediately.
pub(crate) fn refresh_git_async(client: DaemonClient, tx: mpsc::Sender<WorkerMessage>) {
    crate::runtime::http::spawn_task(async move {
        if let Ok(info) = client.get_git_async().await {
            let _ = tx.send(WorkerMessage::Git(info)).await;
        }
    });
}

/// Maps a UI overlay decision to the wire protocol. Pure so approval routing
/// is unit-testable without a daemon or TUI.
/// One decision courier per queued approval (V1b): the overlay resolves the
/// front entry through its own sender and this task POSTs the decision for
/// that request_id. Child approvals can be queued while the parent turn's
/// worker is busy elsewhere, so decisions can no longer ride one shared
/// per-turn channel.
pub(crate) fn spawn_approval_poster(
    client: DaemonClient,
    session_id: String,
    request_id: String,
    decision_rx: mpsc::Receiver<ApprovalDecision>,
) {
    crate::runtime::http::spawn_task(async move {
        let mut decision_rx = decision_rx;
        // A closed channel (the TUI went away) resolves to deny: the parked
        // approval must never strand the requesting agent thread.
        let decision = decision_rx.recv().await.unwrap_or(ApprovalDecision::Deny);
        if let Err(e) = client
            .approve_async(&session_id, &request_id, decision)
            .await
        {
            crate::llm::http::provider_log(
                "approval_delivery_failed",
                &crate::llm::http::error_chain_message(&*e),
            );
        }
    });
}

/// Terminal-close classification for the worker task. The daemon always ends
/// a turn with `TurnComplete` / `TurnFailed`; closing without one is a
/// transport failure and must surface as an error, not silent success.
pub(crate) fn premature_close_error(saw_terminal: bool) -> Option<String> {
    (!saw_terminal).then(|| "connection closed before turn completed".to_string())
}
