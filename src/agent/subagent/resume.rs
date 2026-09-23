//! Manual re-entry (§24.3): [`ResumeHandle`] advertises a recoverable
//! ending, `delegate(resume_from)` re-enters it as generation + 1.
//! Resume is spawn-with-history, not a new operation.

use std::path::PathBuf;

use super::exit::{ExhaustKind, ExitReason};
use super::model::AgentId;

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
    /// Effective model override of the finished generation (`def.model` at
    /// spawn: per-spawn pick else frontmatter); `None` = inherited the
    /// parent model. A resume without its own `model` re-applies this so a
    /// per-spawn model survives generations; on-disk handles predate
    /// it and resume under the definition.
    pub(crate) model: Option<String>,
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
