//! Idle wake (§10b V1b): when a background-agent completion notice lands
//! while no turn is live, run the notice drain as a real journaled turn.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::llm::config::agent_wake_enabled;
use crate::protocol::{ChatRequest, StreamEnvelope};

use super::turn::run_agent_turn;
use super::{lock_map, DaemonState};

/// Wake scheduling (§10b V1b): debounce bursts, retry around a live user
/// turn a bounded number of times, and gate on recent client presence.
const WAKE_DEBOUNCE: Duration = Duration::from_secs(2);
const WAKE_RETRY: Duration = Duration::from_secs(5);
const WAKE_RETRIES: usize = 12;
/// A client reading the journal within this window counts as an audience.
const WAKE_PRESENCE_WINDOW: Duration = Duration::from_secs(30);

/// The idle wake turn's prompt: the drained notices themselves ride the
/// agent-notifications user message (the same seam a user turn uses), so
/// the wake only needs to point the main agent at them.
const WAKE_PROMPT: &str = "A background agent finished while this session was idle. Review the agent-notifications below and continue the work they point at; if nothing needs doing, reply with one short line and stop.";

/// §10b V1b: idle wake. Fired when a completion notice is queued while no
/// turn is live and a client is plausibly listening (presence = a recent
/// `GET /events` read). Runs the notice drain as a REAL journaled turn —
/// same pipeline, guard rails, and teardown as a user turn — so the main
/// agent acts on background completions without waiting for the user.
/// Chat wins: the chat handler steals the wake before registering, so no
/// user-visible 409 ever loses a race with a background notice.
pub(crate) fn schedule_idle_wake(state: Arc<DaemonState>, session_id: String) {
    tokio::spawn(async move {
        // Debounce: children finishing in a burst wake once, not per child.
        tokio::time::sleep(WAKE_DEBOUNCE).await;
        let mut attempts = 0usize;
        loop {
            attempts += 1;
            if attempts > WAKE_RETRIES || !agent_wake_enabled() {
                return;
            }
            // Presence gate: no client reading the journal → no audience.
            // The notices wait in the queue for the next real turn.
            if !state.client_seen_fresh(&session_id, WAKE_PRESENCE_WINDOW) {
                return;
            }
            if lock_map(&state.active_turns).contains(&session_id) {
                // A user turn is live: it drains at its boundary. Re-check
                // after it ends so a notice landing mid-turn still wakes.
                tokio::time::sleep(WAKE_RETRY).await;
                continue;
            }
            let manager = state.manager_for(&session_id);
            if !manager.has_notices() {
                return;
            }
            let Some(wake_cancel) = state.claim_wake(&session_id) else {
                return; // one wake at a time per session
            };
            // Re-check idle after claiming (the claim raced a user turn).
            if lock_map(&state.active_turns).contains(&session_id) {
                state.cancel_wake(&session_id);
                continue;
            }
            // The wake holds the session's turn slot, so the append-only log
            // stays serialized; the user chat POST steals instead of 409ing.
            lock_map(&state.active_turns).insert(session_id.clone());
            lock_map(&state.cancel_tokens).insert(session_id.clone(), wake_cancel.clone());
            // Same model the session's last turn used (stored per session);
            // permission resolves from the daemon's own ceiling. Served
            // from the registry entry (stashed at turn start); the file
            // scan is the fallback for entries registered before their
            // first turn. Provider + base URL ride along so a qualified
            // `provider/model` turn wakes on the same endpoint.
            let (model, base_url) = lock_map(&state.sessions)
                .get(&session_id)
                .map(|entry| {
                    let file_state = if entry.model.is_some() {
                        None
                    } else {
                        crate::session::load_session_state(&entry.path).ok()
                    };
                    let m = entry
                        .model
                        .clone()
                        .filter(|m| !m.trim().is_empty())
                        .or_else(|| {
                            file_state
                                .as_ref()
                                .and_then(|map| map.get("model").cloned())
                        })
                        .filter(|s| !s.trim().is_empty());
                    // Re-qualify a bare model with its provider so `from_env`
                    // resolves the same endpoint the turn ran on.
                    let provider = entry.wake_provider.clone().or_else(|| {
                        file_state
                            .as_ref()
                            .and_then(|map| map.get("provider").cloned())
                    });
                    let m = match (m, provider) {
                        (Some(m), Some(p)) if !m.contains('/') && !p.is_empty() => {
                            Some(format!("{p}/{m}"))
                        }
                        (m, _) => m,
                    };
                    (m, entry.wake_base_url.clone())
                })
                .unwrap_or((None, None));
            let request = ChatRequest {
                prompt: WAKE_PROMPT.to_string(),
                skill_dirs: Vec::new(),
                base_url,
                model,
                permission: None,
                headers: None,
                plan: None,
                system_prompt: None,
                thinking_effort: None,
            };
            // The wake's stream has no attached client; the journal is the
            // delivery path and the terminal event's send is best effort.
            let (wake_tx, _wake_rx) = mpsc::channel::<StreamEnvelope>(256);
            run_agent_turn(
                state.clone(),
                session_id.clone(),
                request,
                wake_cancel,
                wake_tx,
                None,
                0,
                None,
                None,
            )
            .await;
            // More completions may have landed while the wake ran; re-arm
            // (bounded) instead of dropping them.
            if !state.manager_for(&session_id).has_notices() {
                return;
            }
            tokio::time::sleep(WAKE_DEBOUNCE).await;
        }
    });
}
