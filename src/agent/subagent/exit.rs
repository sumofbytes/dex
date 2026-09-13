//! Phase 11 (§24.1) — the exit taxonomy: every terminal child path
//! classifies to one [`ExitReason`] at exactly one choke point, and the
//! single [`on_exit`] decision advertises a [`ResumeHandle`] exactly when
//! the ending is recoverable *and* the child made progress. No per-failure
//! branches at failure sites.
//!
//! Phase 12 grows `on_exit` into `on_exit(spec, reason, ledger)` with a
//! `Recover` arm; that signature change stays contained to the manager's
//! `finish` and these unit tests.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::instance::AgentId;

/// Why a child ended. Classification is mechanical — the prose stays in
/// `AgentResult` for the parent model, but the policy machine never reads
/// prose. Orthogonal to `AgentState`, whose words are wire-stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExitReason {
    /// The child emitted its final assistant message.
    Normal,
    /// `delegate_stop`, parent-turn cancel, session close, daemon
    /// shutdown, idle reaper. Never resumable: stopping was the intent.
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

/// How a definition re-enters a recoverable child (§24.2). A property of
/// the spec, never of the failure — no recover-for-timeout branches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum RecoverMode {
    /// No automatic re-entry: escalate immediately (OTP `temporary`).
    /// The default — V1a/V1b behavior is byte-identical under it.
    #[default]
    Never,
    /// Re-spawn the same seed with an empty transcript (true OTP restart:
    /// a child is a pure function of definition + seed, both retained).
    Fresh,
    /// Replay the transcript, append the interruption nudge, run under
    /// the remaining turn budget with a fresh wall-clock timeout.
    Resume,
}

/// Declarative supervision per definition (§24.2): frontmatter keys
/// `recover` / `recover_max` / `recover_window_secs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SupervisionSpec {
    pub(crate) recover: RecoverMode,
    /// Recovery actions allowed per window, supervisor-scoped (OTP
    /// intensity: siblings share the ledger). Parsed ≥ 1.
    pub(crate) max: u32,
    pub(crate) window: Duration,
}

/// Built-in intensity: two recoveries per minute per session before the
/// breaker opens (§24.5).
pub(crate) const DEFAULT_RECOVER_MAX: u32 = 2;
pub(crate) const DEFAULT_RECOVER_WINDOW: Duration = Duration::from_secs(60);

impl Default for SupervisionSpec {
    fn default() -> Self {
        Self {
            recover: RecoverMode::Never,
            max: DEFAULT_RECOVER_MAX,
            window: DEFAULT_RECOVER_WINDOW,
        }
    }
}

/// Classify a child-body `Err` string (§24.1 table). The turn-budget
/// message is matched by prefix — it is built in exactly one place
/// (`process_turn`, "turn budget exhausted after …"), pinned by the test
/// below. Unknown failures default to `Permanent`: manual
/// `delegate(resume_from)` still works off the transcript, but
/// *automatic* recovery (Phase 12) stays out until a reason earns it.
pub(crate) fn classify_body_error(message: &str) -> ExitReason {
    if message.starts_with("turn budget exhausted") {
        ExitReason::Exhausted(ExhaustKind::Budget)
    } else if message.contains("cancelled") {
        ExitReason::ShutDown
    } else {
        ExitReason::Permanent
    }
}

/// The single decision (§24): `Settle` keeps the terminal result and its
/// notice; `Recover` re-enters the child as generation + 1 without
/// notifying the parent; `Escalate` settles *and* advertises the resume
/// handle, carrying the recovery history for the model's rectification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Settle,
    Recover { mode: RecoverMode },
    Escalate,
}

pub(crate) fn decide(
    spec: &SupervisionSpec,
    reason: ExitReason,
    recoveries_in_window: usize,
    progress_made: bool,
) -> Action {
    let resumable =
        matches!(reason, ExitReason::Transient | ExitReason::Exhausted(_)) && progress_made;
    if !resumable {
        return Action::Settle;
    }
    match spec.recover {
        RecoverMode::Never => Action::Escalate,
        RecoverMode::Fresh | RecoverMode::Resume => {
            if recoveries_in_window >= spec.max as usize {
                Action::Escalate
            } else {
                Action::Recover { mode: spec.recover }
            }
        }
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
    /// Lineage of automatic recoveries ("resumed after timed out"),
    /// oldest first — the escalation record. Empty for first attempts.
    pub(crate) history: Vec<String>,
}

/// A re-entry, manual or automatic (§24.3): replay `handle.transcript`,
/// append the interruption nudge (+ `instruction`, + `file_hints`), run
/// under `handle.remaining_budget`. Resume is spawn-with-history, not a
/// new operation — the same body, one new `Option`.
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

/// One-word mode for §15-style lifecycle lines.
pub(crate) fn recover_mode_word(mode: RecoverMode) -> &'static str {
    match mode {
        RecoverMode::Never => "never",
        RecoverMode::Fresh => "restarted",
        RecoverMode::Resume => "resumed",
    }
}

/// One-word reason for §15-style lifecycle lines.
pub(crate) fn exit_reason_word(reason: ExitReason) -> &'static str {
    match reason {
        ExitReason::Normal => "finished",
        ExitReason::ShutDown => "cancelled",
        ExitReason::Transient => "interrupted",
        ExitReason::Exhausted(ExhaustKind::Budget) => "budget exhausted",
        ExitReason::Exhausted(ExhaustKind::Timeout) => "timed out",
        ExitReason::Permanent => "failed",
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

    #[test]
    fn decide_routes_recoverable_progress_by_spec_and_intensity() {
        let never = SupervisionSpec::default();
        assert_eq!(never.recover, RecoverMode::Never);
        let resume = SupervisionSpec {
            recover: RecoverMode::Resume,
            ..SupervisionSpec::default()
        };
        let fresh = SupervisionSpec {
            recover: RecoverMode::Fresh,
            ..SupervisionSpec::default()
        };
        // Recoverable reasons with progress: escalate under `never`,
        // recover within intensity, escalate once exhausted.
        for reason in [
            ExitReason::Transient,
            ExitReason::Exhausted(ExhaustKind::Budget),
            ExitReason::Exhausted(ExhaustKind::Timeout),
        ] {
            assert_eq!(
                decide(&never, reason, 0, true),
                Action::Escalate,
                "{reason:?}"
            );
            assert_eq!(
                decide(&resume, reason, 0, true),
                Action::Recover {
                    mode: RecoverMode::Resume
                },
                "{reason:?}"
            );
            assert_eq!(
                decide(&fresh, reason, 1, true),
                Action::Recover {
                    mode: RecoverMode::Fresh
                },
                "{reason:?}"
            );
            assert_eq!(
                decide(&resume, reason, DEFAULT_RECOVER_MAX as usize, true),
                Action::Escalate,
                "{reason:?} at intensity"
            );
        }
        // Recoverable reasons without progress: nothing to re-enter from.
        for reason in [
            ExitReason::Transient,
            ExitReason::Exhausted(ExhaustKind::Budget),
            ExitReason::Exhausted(ExhaustKind::Timeout),
        ] {
            assert_eq!(
                decide(&resume, reason, 0, false),
                Action::Settle,
                "{reason:?}"
            );
        }
        // Terminal-by-intent reasons settle, progress or not.
        for reason in [
            ExitReason::Normal,
            ExitReason::ShutDown,
            ExitReason::Permanent,
        ] {
            assert_eq!(
                decide(&resume, reason, 0, true),
                Action::Settle,
                "{reason:?}"
            );
            assert_eq!(
                decide(&never, reason, 0, false),
                Action::Settle,
                "{reason:?}"
            );
        }
    }
}
