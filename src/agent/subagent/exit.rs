//! The exit taxonomy (§24.1): every terminal child path classifies to one
//! [`ExitReason`] at exactly one choke point, and the manager's `finish`
//! advertises a [`ResumeHandle`](super::resume::ResumeHandle) exactly when
//! the ending is recoverable *and* the child made progress. No per-failure
//! branches at failure sites — and no automatic re-entry: the model
//! re-enters by hand with `delegate(resume_from)`.
//!
//! Resume mechanics live in [`super::resume`]; the transcript-progress probe
//! stays here because it classifies wrapper-synthesized endings.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// Why a child ended. Classification is mechanical — the prose stays in
/// `AgentResult` for the parent model, but the policy machine never reads
/// prose. Orthogonal to `AgentState`, whose words are wire-stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExitReason {
    /// The child emitted its final assistant message.
    Normal,
    /// `delegate_stop`, parent-turn cancel, session close, daemon
    /// shutdown. Never resumable: stopping was the intent.
    ShutDown,
    /// Environment flake a re-entry could survive: body panic, transport
    /// or protocol death after output flowed (pre-output failures are
    /// already retried inside the LLM call and never surface here).
    Transient,
    /// The child ran out of a budget it could spend differently: turn
    /// budget (`max_tool_iterations`) or wall clock (`timeout_secs`).
    Exhausted(ExhaustKind),
    /// Bad seed, bad config, empty final message — re-running the same
    /// input cannot help. The model rewrites the seed instead.
    Permanent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExhaustKind {
    Budget,
    Timeout,
}

/// Classify a child-body `Err` string (§24.1 table). The turn-budget
/// message is matched by prefix — it is built in exactly one place
/// (`process_turn`, "turn budget exhausted after …"), pinned by the test
/// below. Unknown failures default to `Permanent`: manual
/// `delegate(resume_from)` still works off the transcript.
pub(crate) fn classify_body_error(message: &str) -> ExitReason {
    if message.starts_with("turn budget exhausted") {
        ExitReason::Exhausted(ExhaustKind::Budget)
    } else if message.contains("cancelled") {
        ExitReason::ShutDown
    } else {
        ExitReason::Permanent
    }
}

/// Best-effort progress check for wrapper-synthesized endings (panic,
/// timeout) whose in-memory counters died with the body: the transcript
/// holds more than its header line, so the child got past creation.
pub(crate) fn transcript_holds_progress(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    BufReader::new(file).lines().take(2).count() == 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_body_error_matches_the_budget_prefix() {
        assert_eq!(
            classify_body_error("turn budget exhausted after 50 tool rounds; partial progress preserved — send another prompt to continue"),
            ExitReason::Exhausted(ExhaustKind::Budget)
        );
        assert_eq!(classify_body_error("cancelled"), ExitReason::ShutDown);
        assert_eq!(
            classify_body_error("llm error: connection reset"),
            ExitReason::Permanent
        );
        assert_eq!(
            classify_body_error("child ended without a final message"),
            ExitReason::Permanent
        );
    }
}
