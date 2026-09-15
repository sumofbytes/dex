//! The exit taxonomy (§24.1): every terminal child path classifies to one
//! [`ExitReason`] at exactly one choke point, and the manager's `finish`
//! advertises a [`ResumeHandle`] exactly when the ending is recoverable
//! *and* the child made progress. No per-failure branches at failure
//! sites — and no automatic re-entry: the model re-enters by hand with
//! `delegate(resume_from)`.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use super::instance::AgentId;

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

/// Handle carried on a resumable result: everything
/// `delegate(resume_from)` needs to re-enter the child as generation + 1.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResumeHandle {
    /// The original child id (history + notice prose).
    pub(crate) agent_id: AgentId,
    /// The original generation's JSONL — the replay source.
    pub(crate) transcript: PathBuf,
    /// The original generation (resume spawns `generation + 1`).
    pub(crate) generation: u32,
    /// Turn budget left (`max_tool_iterations − tool_calls`); `None` =
    /// unlimited or unknown (wrapper-synthesized and on-disk handles).
    pub(crate) remaining_budget: Option<usize>,
    /// Why it died, in one line — becomes the interruption nudge.
    pub(crate) note: String,
}

/// A manual re-entry (§24.3): replay `handle.transcript` and append the
/// interruption nudge (+ `instruction`, + `file_hints`), run under
/// `handle.remaining_budget`. Resume is spawn-with-history, not a new
/// operation — the same body, one new `Option`.
#[derive(Clone, Debug)]
pub(crate) struct ResumeRequest {
    pub(crate) handle: ResumeHandle,
    pub(crate) instruction: Option<String>,
    pub(crate) file_hints: Vec<PathBuf>,
}

/// One-line death note for a reason + spend pair.
pub(crate) fn resume_note(reason: ExitReason, tool_calls: u32) -> String {
    let calls = match tool_calls {
        1 => "1 tool call".to_string(),
        n => format!("{n} tool calls"),
    };
    match reason {
        ExitReason::Transient => format!("interrupted after {calls}; retry from the transcript"),
        ExitReason::Exhausted(ExhaustKind::Budget) => {
            format!("turn budget exhausted after {calls}; continue from the transcript")
        }
        ExitReason::Exhausted(ExhaustKind::Timeout) => {
            format!("timed out after {calls}; continue from the transcript")
        }
        _ => format!("ended after {calls}"),
    }
}

/// Leftover tool rounds a resume handle may advertise: `allowance -
/// spent`, or `None` when nothing is left (or the lineage ran under no
/// cap — unknown spend resumes under the definition's full meter, like
/// interrupted runs). A `Some(0)` handle would promise "write your final
/// summary without tools" while the turn loop still runs one post-hoc
/// round and then hard-fails on "turn budget exhausted" — advertising
/// `None` keeps the nudge honest.
pub(crate) fn advertised_remaining(cap: Option<usize>, spent: usize) -> Option<usize> {
    cap.and_then(|cap| cap.checked_sub(spent))
        .filter(|left| *left > 0)
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
