//! `POST /api/sessions/{id}/shell` — run a shell command directly in the
//! daemon workspace (`!`/`!!` prefix in the TUI). Bypasses the agent loop
//! and approvals: the explicit `!` is the approval (even in `read-only`,
//! which constrains the model, not your own typing). The run is saved to
//! session history: `!` feeds the next turn as a user message, `!!`
//! (`exclude_from_context`) is saved too but filtered out of the
//! model-bound history at load.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;

use crate::core::types::ChatMessage;
use crate::protocol::StreamEvent;
use crate::session::Session;

use super::lookup::session_path;
use super::{lock_map, DaemonState};
use crate::runtime::console::CancellationToken;

/// `POST /api/sessions/{id}/shell` with `{"command": ...}` — run a shell
/// command directly in the daemon workspace (`!`/`!!` prefix in the TUI).
/// Bypasses the agent loop and approvals: the explicit `!` is
/// the approval (even in `read-only`, which constrains the model, not your
/// own typing). The run is saved to session history: `!` feeds the
/// next turn as a user message, `!!` (`exclude_from_context`) is saved too
/// but filtered out of the model-bound history at load. Empty commands are
/// a 400, unknown sessions a 404, and a second run while one is in flight
/// for the session is a 409 (the TUI refuses it first; this guards direct
/// API callers). A concurrent agent turn is allowed — `!` may run alongside
/// a turn and the result folds into context afterwards; both append to the
/// append-only journal, ordered by completion.
pub(crate) async fn session_shell(
    State(state): State<Arc<DaemonState>>,
    Path(session_id): Path<String>,
    Json(req): Json<crate::protocol::ShellRequest>,
) -> Result<Json<crate::protocol::ShellResponse>, StatusCode> {
    let command = req.command.trim().to_string();
    if command.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    // Session must exist — the workspace is resolved from the daemon cwd,
    // but the lookup guards against typos/stale ids like every other route.
    let session_file = session_path(&state, &session_id).await?;
    let shell_cancel = CancellationToken::new();
    {
        let mut running = lock_map(&state.shell_tokens);
        if running.contains_key(&session_id) {
            return Err(StatusCode::CONFLICT);
        }
        running.insert(session_id.clone(), shell_cancel.clone());
    }
    // Frees the per-session slot even when the run panics, so one bad run
    // can't wedge `!` for the session until a daemon restart.
    struct ShellGuard {
        state: Arc<DaemonState>,
        session_id: String,
    }
    impl Drop for ShellGuard {
        fn drop(&mut self) {
            lock_map(&self.state.shell_tokens).remove(&self.session_id);
        }
    }
    let _guard = ShellGuard {
        state: state.clone(),
        session_id: session_id.clone(),
    };
    let started = Instant::now();
    let mut args = serde_json::Map::new();
    args.insert(
        "command".to_string(),
        serde_json::Value::String(command.clone()),
    );
    // Explicit user `!` invocation: the `!` itself is the approval, so this
    // runs trusted (same rationale as `execute_sync` for `dex run`).
    // Unfiltered: explicit invocations never run under a child allowlist.
    let (output, success, code) = match crate::tools::execute(
        "bash",
        &args,
        &shell_cancel,
        &crate::tools::Policy::trusted(),
        None,
    )
    .await
    {
        Ok(output) => (output, true, Some(0)),
        Err(error) => {
            // Same `Error: …` shape `execute_outcome` gives the agent loop
            // (Display appends `[exit N]` for non-zero exits), plus the raw
            // code for the client.
            let code = match &error {
                crate::tools::ToolError::Shell { code, .. } => *code,
                _ => None,
            };
            (format!("Error: {error}"), false, code)
        }
    };
    let duration = started.elapsed().as_secs_f64();
    let cancelled = shell_cancel.is_cancelled();
    // The slot guard stays alive through the journal write below: freeing it
    // before persisting would let a second `!` start while the first is still
    // appending, interleaving the two runs' message + event writes and
    // letting a concurrent turn's seq land inside this run's call/result
    // pair. A slow disk serializing back-to-back `!` runs is the cheaper
    // failure mode.
    let persist = if req.exclude_from_context {
        ChatMessage::user_named(
            crate::core::format::bash_context_text(&command, &output, success, code, cancelled),
            crate::core::types::BASH_EXCLUDED_NAME,
        )
    } else {
        ChatMessage::user(crate::core::format::bash_context_text(
            &command, &output, success, code, cancelled,
        ))
    };
    // ToolCall/ToolResult pair mirroring the live TUI block, so a
    // true-remote reattach (events-journal replay) renders the same block
    // the co-located transcript rebuild draws from the message above.
    let input_json = serde_json::json!({"command": command}).to_string();
    let short = crate::core::format::short_arg("bash", &input_json);
    let summary =
        crate::core::format::tool_result_summary("bash", &input_json, &output, success, None);
    let preview = crate::core::format::tool_preview("bash", success, None, &output, true);
    let (call_seq, result_seq) = state.next_seq_pair(&session_id);
    // Unique id shared by the pair so concurrent runs can't steal each
    // other's half even if the two pairs interleave in the journal. The
    // journal seq is already unique per session, so reuse it as the block
    // id instead of minting a separate counter.
    let block_id = format!("shell-{call_seq}");
    let call_event = serde_json::to_string(&StreamEvent::ToolCall {
        name: "bash".to_string(),
        args: serde_json::Value::String(short),
        id: block_id.clone(),
    })
    .unwrap_or_default();
    let result_event = serde_json::to_string(&StreamEvent::ToolResult {
        name: "bash".to_string(),
        summary,
        success,
        preview,
        duration,
        id: block_id,
    })
    .unwrap_or_default();
    // Best-effort history: a failed journal write must not fail a run whose
    // output is already in hand (the TUI renders the response regardless).
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(mut session) = Session::from_path(&session_file) {
            let _ = session.append_message(&persist);
            let _ = session.append_event(call_seq, &call_event);
            let _ = session.append_event(result_seq, &result_event);
        }
    })
    .await;
    Ok(Json(crate::protocol::ShellResponse {
        output,
        success,
        code,
    }))
}
