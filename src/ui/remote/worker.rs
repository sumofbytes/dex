use super::state::WorkerMessage;
use crate::client::http::ChatOptions;
use crate::client::http::ChatStream;
use crate::client::http::DaemonClient;
use crate::protocol::EventsResponse;
use crate::protocol::StreamEvent;
use tokio::sync::mpsc;

/// Outcome of a reconnect attempt after a transport failure mid-turn.
pub(crate) enum Reconnect {
    /// A live SSE stream for the same turn is open again.
    Resumed(ChatStream),
    /// The turn already finished while we were disconnected; its terminal
    /// event was replayed to the UI.
    Terminal,
    /// Unrecoverable; carries the reason to surface.
    Failed(String),
}

/// Recovery after a lost connection mid-turn: replay the daemon's journaled
/// events past our cursor (the terminal event arrives there if the turn
/// finished while we were gone), then re-POST the turn with the SAME
/// idempotency key. A completed turn replays its recorded terminal event
/// instead of running again; a still-running turn answers 409 CONFLICT, so
/// no second stacked turn is ever created; a turn that died with no terminal
/// re-executes (idempotency can't dedup what never finished). Approvals in
/// the replay are forwarded for visibility but not re-decided: parked
/// approvals die with their turn on the daemon.
pub(crate) async fn try_reconnect(
    client: &DaemonClient,
    session_id: &str,
    prompt: &str,
    options: &ChatOptions,
    last_seq: &mut u64,
    event_tx: &mpsc::Sender<WorkerMessage>,
) -> Reconnect {
    // The journal call's error is non-Send (plain `Box<dyn Error>`); match it
    // out immediately so the non-Send type never spans a later `.await`.
    let replayed = match client.events_async(session_id, *last_seq).await {
        Ok(resp) => resp,
        Err(_) => EventsResponse {
            events: Vec::new(),
            next_seq: *last_seq,
        },
    };
    {
        let mut terminal = false;
        for env in &replayed.events {
            if matches!(
                env.event,
                StreamEvent::TurnComplete { .. } | StreamEvent::TurnFailed { .. }
            ) {
                terminal = true;
            }
            if event_tx
                .send(WorkerMessage::Stream(env.event.clone()))
                .await
                .is_err()
            {
                return Reconnect::Failed("ui closed while replaying events".into());
            }
        }
        // Advance the resume cursor past everything just forwarded so a
        // second drop doesn't re-replay the same range.
        *last_seq = (*last_seq).max(replayed.next_seq);
        if terminal {
            return Reconnect::Terminal;
        }
    }
    match client
        .chat_stream(session_id, prompt, options.clone())
        .await
    {
        Ok(stream) => Reconnect::Resumed(stream),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("409") || msg.to_uppercase().contains("CONFLICT") {
                Reconnect::Failed(
                    "connection to the daemon was lost and the turn is still running; reopen it with `dex --reattach <session-id>` to follow".to_string(),
                )
            } else {
                Reconnect::Failed(format!("connection lost and reattach failed: {msg}"))
            }
        }
    }
}
