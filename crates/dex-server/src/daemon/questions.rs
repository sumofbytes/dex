//! `ask_user` questions (§ ask-user spec): park batches, broadcast them to
//! the active surface, and dismiss-after-timeout for children so no blocked
//! call strands forever. The counterpart of `approvals.rs` for the
//! `ask_user` tool.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use dex_protocol::{Question, QuestionAnswer};

use crate::agent::delegate::{AgentId, AgentManager};
use crate::protocol::QuestionRequest;
use crate::protocol::{StreamEnvelope, StreamEvent};

use super::approvals::write_approval_audit;
use super::{journal_event, lock_map, DaemonState, PendingQuestion};

/// Five-minute silence dismisses a parked child-agent question (the
/// child-approval timeout rule): a prompt nobody answers must never strand
/// a child forever. Parent turns get no timer — user attention is the
/// point of the tool.
const CHILD_QUESTION_TIMEOUT: Duration = Duration::from_secs(300);

/// All-`Dismiss` answers for a batch: the teardown/timeout response.
pub(crate) fn dismissed(count: usize) -> Vec<QuestionAnswer> {
    vec![QuestionAnswer::Dismiss; count]
}

/// Parent-turn bridge: park each batch under a fresh `request_id` and
/// surface it through the turn's SSE pump (the same channel that journals
/// and forwards every other event, so ordering stays intact). Journal +
/// direct-send ONLY — the turn's stream is registered in `active_streams`,
/// so a broadcast here would deliver the event twice (mirrors the parent
/// approval bridge in `turn.rs`). No timer — the turn's `TurnGuard`
/// dismisses parent questions on teardown.
pub(crate) async fn parent_question_bridge(
    state: Arc<DaemonState>,
    session_id: String,
    stream_tx: mpsc::Sender<StreamEnvelope>,
    mut rx: mpsc::Receiver<QuestionRequest>,
    cancel: crate::runtime::console::CancellationToken,
) {
    while let Some(request) = rx.recv().await {
        // A cancellation was requested: don't surface new questions,
        // dismiss them so the agent task can unwind.
        if cancel.is_cancelled() {
            let _ = request
                .response
                .try_send(dismissed(request.questions.len()));
            continue;
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        let env = park(&state, &session_id, request_id, request);
        let _ = stream_tx.send(env).await;
    }
}

/// §12 V1b counterpart for children: consume a child's question batches,
/// park them labeled in the session's `pending_questions`, and dismiss them
/// after a five-minute silence.
pub(crate) async fn child_question_bridge(
    state: Arc<DaemonState>,
    session_id: String,
    manager: AgentManager,
    mut rx: mpsc::Receiver<QuestionRequest>,
) {
    while let Some(request) = rx.recv().await {
        // The session's manager was dropped (deleted/reset): dismiss so the
        // child's blocked call unwinds instead of parking forever.
        if !lock_map(&state.agents).contains_key(&session_id) {
            let _ = request
                .response
                .try_send(dismissed(request.questions.len()));
            continue;
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        let agent = request
            .agent
            .clone()
            .or_else(|| manager.definition_name(&AgentId(request.agent_id.clone()?)));
        let env = park(
            &state,
            &session_id,
            request_id.clone(),
            QuestionRequest {
                questions: request.questions,
                agent_id: request.agent_id.clone(),
                agent: agent.clone(),
                response: request.response,
            },
        );
        // With no turn streaming for children, the client's event poll is
        // the delivery path (§15: child events ride ?since=).
        state.broadcast_event(&session_id, &env);
        // The five-minute dismissal timer. Resolution removes the entry
        // first, so an answered prompt never double-dismisses.
        let timer_state = state.clone();
        let timer_sid = session_id.clone();
        let timer_id = request_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(CHILD_QUESTION_TIMEOUT).await;
            let pending = {
                let mut pending = lock_map(&timer_state.pending_questions);
                pending.remove(&timer_id)
            };
            let Some(pending) = pending else {
                return;
            };
            if pending.session_id != timer_sid {
                // Restore: cross-session entries belong to their session.
                lock_map(&timer_state.pending_questions).insert(timer_id, pending);
                return;
            }
            write_approval_audit(
                &timer_sid,
                &timer_id,
                "ask_user",
                &pending.questions_json,
                "dismiss",
                "timeout",
                pending.agent.as_deref(),
            );
            let _ = pending
                .response
                .send(dismissed(pending.question_count))
                .await;
        });
    }
}

/// Park one batch and journal the wire event. Returns the envelope; the
/// caller owns the delivery path — parent: direct-send on the turn's
/// stream, child: `broadcast_event` (children have no turn stream).
fn park(
    state: &Arc<DaemonState>,
    session_id: &str,
    request_id: String,
    request: QuestionRequest,
) -> StreamEnvelope {
    let questions_json =
        serde_json::to_string(&request.questions).unwrap_or_else(|_| "[]".to_string());
    let parked = PendingQuestion {
        session_id: session_id.to_string(),
        response: request.response,
        question_count: request.questions.len(),
        questions_json: questions_json.clone(),
        agent_id: request.agent_id,
        agent: request.agent.clone(),
    };
    let replaced = lock_map(&state.pending_questions).insert(request_id.clone(), parked);
    if let Some(stale) = replaced {
        // Should not happen (request_ids are unique); dismiss to avoid a
        // deadlock in a stray agent thread.
        let _ = stale.response.try_send(dismissed(stale.question_count));
    }
    let env = StreamEnvelope {
        seq: state.next_seq(session_id),
        event: StreamEvent::QuestionRequired {
            request_id,
            questions: request.questions,
            agent: request.agent,
        },
    };
    journal_event(state, session_id, env.seq, &env.event);
    env
}

/// Clamp a client-supplied answer batch server-side, mirroring the
/// executor's `parse_questions` clamping (a mismatched client must not be
/// able to mis-align `answers[i]` with `questions[i]`): the slot count is
/// truncated/padded to the parked batch size, and out-of-range
/// `Choice`/`Multi` indices collapse to `Dismiss` (the executor renders
/// those rows as "no answer"). Selection order is preserved.
pub(crate) fn sanitize_answers(
    questions: &[Question],
    answers: Vec<QuestionAnswer>,
) -> Vec<QuestionAnswer> {
    let mut answers = clamp_len(answers, questions.len());
    for (answer, question) in answers.iter_mut().zip(questions) {
        match answer {
            QuestionAnswer::Choice(idx) if *idx >= question.options.len() => {
                *answer = QuestionAnswer::Dismiss;
            }
            QuestionAnswer::Multi(idxs) => {
                idxs.retain(|idx| *idx < question.options.len());
                if idxs.is_empty() {
                    *answer = QuestionAnswer::Dismiss;
                }
            }
            _ => {}
        }
    }
    answers
}

/// Truncate/pad an answer batch to exactly `count` slots (`Dismiss` fills
/// the tail, matching the executor's missing-slot rendering).
fn clamp_len(mut answers: Vec<QuestionAnswer>, count: usize) -> Vec<QuestionAnswer> {
    answers.truncate(count);
    while answers.len() < count {
        answers.push(QuestionAnswer::Dismiss);
    }
    answers
}
