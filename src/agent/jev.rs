//! Jev-inspired verbatim compaction: drop or truncate stale tool outputs,
//! never rewrite.
//!
//! This ports the `tamaratran/fast-jev-compaction` idea (itself a port of
//! the Typesafe Jev `systemone` flow). Every `tool_use` is paired with its
//! `tool_result` by call id, and each pair in the archivable span is scored
//! as keep-result, keep-call, or drop. Everything kept stays verbatim, so
//! user and assistant text is never removed.
//!
//! The phase-1 scorer here is a deterministic heuristic over size,
//! re-runnability, and error evidence, which keeps the path offline with no
//! new dependency and hermetic in tests. The pairing, truncation, and
//! rebuild shape mirrors the upstream library, so a future
//! `TYPESAFE_API_KEY`-backed asker (two `noul` questions per call, batched
//! under 30k tokens) can replace the scorer without touching callers.
//!
//! The caller passes the exact archivable span (from `find_cut_point`, which
//! already excludes the keep-recent window): there is no internal
//! recency pin.
//!
//! Stack order with the other output shrinkers: the evidence reducer
//! compresses a result inline at execution time, observation pack projects
//! large results out of the provider's request view (recallable via
//! `obs_recall`), and this prune runs last at compaction time over the
//! intact session history pack never edits. A Jev drop is therefore
//! destructive where packing is recallable — contained by only ever
//! dropping re-runnable reads (re-run the tool to restore): errors,
//! plans, and mutating-tool evidence truncate instead of dropping.

use std::collections::{HashMap, HashSet};

use crate::llm::config::warn_once;
use crate::protocol::{ChatMessage, Role};

/// Value of [`COMPACTION_ENV`] /
/// [`crate::agent::online_compaction::ONLINE_COMPACTION_ENV`] selecting
/// verbatim pruning instead of summarization.
pub(crate) const JEV_VALUE: &str = "jev";

/// `DEX_COMPACTION` selects the threshold-compaction summarizer
/// (`llm` = model summary, `jev` = verbatim prune, default deterministic;
/// `1` still selects the LLM as an alias). Lives here — next to the mode
/// enum — so both readers share one parse.
pub(crate) const COMPACTION_ENV: &str = "DEX_COMPACTION";

/// Deprecated predecessor of [`COMPACTION_ENV`]: `DEX_COMPACTION_LLM=1`
/// selected the LLM summarizer. Honored as an alias for
/// `DEX_COMPACTION=llm` with a `warn_once` pointer so existing shells keep
/// working after the rename.
pub(crate) const LEGACY_COMPACTION_ENV: &str = "DEX_COMPACTION_LLM";

/// Characters of a truncated result kept as head context.
pub(crate) const JEV_TRUNCATE_HEAD_CHARS: usize = 300;

/// Minimum reduction for a prune to beat a summary (upstream
/// `reductionRatio < 0.25` falls back to summary).
pub(crate) const JEV_MIN_REDUCTION_RATIO: f64 = 0.25;

/// Memo estimate for the boundary economics when Jev prunes instead of
/// summarizing: a few truncated heads, not a 1k summary. A deliberate
/// pre-decision floor, not a measurement: each truncated head leaves behind
/// ~90 tokens (300 chars + trailer), so a prune with many truncations leaves
/// more than 200 behind and the saving (`archive - memo`) is overstated.
/// Bounded in practice: the worthwhile gate needs ≥25% reduction, so a
/// many-truncation prune always frees thousands of tokens and the few-hundred
/// memo error barely moves the breakeven — and the carried-debt gate prices
/// the next compaction against the same floor, so the error cannot compound
/// into a compaction spiral.
pub(crate) const JEV_MEMO_TOKEN_ESTIMATE: u64 = 200;

/// Medium results are truncated; huge ones from re-runnable tools are dropped.
const TRUNCATE_ABOVE_CHARS: usize = 2_000;
const DROP_ABOVE_CHARS: usize = 10_000;
/// Small results are cheap — keep verbatim.
const KEEP_BELOW_CHARS: usize = 500;

/// How `compact_history` handles the archivable span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SummaryMode {
    Deterministic,
    Llm,
    Jev,
}

impl SummaryMode {
    /// True when this mode prunes verbatim instead of summarizing.
    pub(crate) fn prunes_jev(self) -> bool {
        self == SummaryMode::Jev
    }
}

/// One knob shape shared by [`COMPACTION_ENV`] and
/// [`crate::agent::online_compaction::ONLINE_COMPACTION_ENV`]: `jev`
/// selects verbatim pruning, `llm` (or `1`) selects the LLM/summary
/// behavior, unset and explicit offs disable, anything else is a typo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompactionKnob {
    Jev,
    One,
    Off,
    Unrecognized,
}

/// Case-insensitive parse of the shared `llm`/`1`/`jev` knob shape.
/// Returns the selection without warning — callers warn with their own env
/// var name on [`CompactionKnob::Unrecognized`].
pub(crate) fn parse_compaction_knob(raw: &str) -> CompactionKnob {
    match raw.trim().to_ascii_lowercase().as_str() {
        JEV_VALUE => CompactionKnob::Jev,
        "llm" | "1" => CompactionKnob::One,
        "" | "0" | "false" | "off" | "no" => CompactionKnob::Off,
        _ => CompactionKnob::Unrecognized,
    }
}

/// Parse `DEX_COMPACTION`: `jev` (any case) → [`SummaryMode::Jev`],
/// `llm` (any case; `1` accepted as an alias) → [`SummaryMode::Llm`],
/// unset/explicit offs → [`SummaryMode::Deterministic`]. A non-empty
/// unrecognized value warns once (a typo like `=jve` must not silently
/// degrade to deterministic) and likewise falls back to deterministic.
/// The deprecated `DEX_COMPACTION_LLM=1` is honored as `llm` with a
/// `warn_once` pointer when `DEX_COMPACTION` itself is unset/empty.
pub(crate) fn summary_mode() -> SummaryMode {
    let raw = std::env::var(COMPACTION_ENV).unwrap_or_default();
    if !raw.trim().is_empty() {
        match parse_compaction_knob(&raw) {
            CompactionKnob::Jev => return SummaryMode::Jev,
            CompactionKnob::One => return SummaryMode::Llm,
            CompactionKnob::Off => return SummaryMode::Deterministic,
            CompactionKnob::Unrecognized => {
                let short: String = raw.trim().chars().take(32).collect();
                warn_once(
                    "env:DEX_COMPACTION",
                    &format!("ignoring DEX_COMPACTION={short:?} — expected 'llm', 'jev', or '0'"),
                );
                return SummaryMode::Deterministic;
            }
        }
    }
    if std::env::var(LEGACY_COMPACTION_ENV).as_deref() == Ok("1") {
        warn_once(
            "env:DEX_COMPACTION_LLM",
            "DEX_COMPACTION_LLM=1 is deprecated — use DEX_COMPACTION=llm",
        );
        return SummaryMode::Llm;
    }
    SummaryMode::Deterministic
}

/// `dex doctor` origin row for the threshold-compaction knob: the value
/// names the mode that runs, the source names the env var whenever it is
/// set (even to an explicit off or a warned-about typo — the value still
/// reports what runs), the deprecated `DEX_COMPACTION_LLM` when the mode
/// comes from that fallback, or the built-in default when unset/empty.
pub(crate) fn compaction_doctor() -> (String, String) {
    let value = match summary_mode() {
        SummaryMode::Jev => "verbatim prune (jev)",
        SummaryMode::Llm => "LLM summary",
        SummaryMode::Deterministic => "deterministic",
    }
    .to_string();
    let source = if std::env::var(COMPACTION_ENV)
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
    {
        COMPACTION_ENV.to_string()
    } else if std::env::var(LEGACY_COMPACTION_ENV).as_deref() == Ok("1") {
        format!("{LEGACY_COMPACTION_ENV} (deprecated, use {COMPACTION_ENV})")
    } else {
        "built-in default".to_string()
    };
    (value, source)
}

/// Outcome counts for one [`prune_span`] pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct JevStats {
    pub(crate) kept: usize,
    pub(crate) truncated: usize,
    pub(crate) dropped: usize,
    pub(crate) original_chars: usize,
    pub(crate) freed_chars: usize,
}

/// Reduction ratio (`freed / original`); `0.0` on an empty span.
pub(crate) fn reduction_ratio(stats: &JevStats) -> f64 {
    if stats.original_chars == 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let ratio = stats.freed_chars as f64 / stats.original_chars as f64;
    ratio.clamp(0.0, 1.0)
}

/// True when the prune freed enough to beat a summary.
pub(crate) fn is_worthwhile(stats: &JevStats) -> bool {
    stats.freed_chars > 0 && reduction_ratio(stats) >= JEV_MIN_REDUCTION_RATIO
}

/// Tools whose output can be reproduced by re-running them — safe to drop
/// verbatim when old and large. Single-sourced from the tool registry
/// (`ToolMetadata::read_only + idempotent`) instead of a second tool list:
/// pure reads (`read`, `ls`, the fff tools, `git`) drop when old and huge,
/// while mutating / delegating / external tools (`bash`, `write`, `edit`,
/// `chain`, `mcp__*`, extensions) only ever truncate, so file-op evidence
/// and child answers survive. Unknown tools have no registry row and stay
/// conservative: truncate, never drop.
fn is_rerunnable(tool: &str) -> bool {
    crate::tools::metadata(tool).is_some_and(|m| m.read_only && m.idempotent)
}

/// Anchors that must survive verbatim: plan snapshots orient the next plan
/// (`update_plan`, defined in `online_compaction::tool_defs`), `obs_recall`
/// pages are already the recall path (defined in `obs_pack`). Both are
/// read-only in the registry but never reach the scorer — `decide` keeps
/// them above.
fn is_anchor(tool: &str) -> bool {
    matches!(tool, "update_plan" | "obs_recall")
}

fn looks_like_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("error")
        || lower.contains("failed")
        || lower.contains("failure")
        || lower.contains("panic")
        || lower.contains("traceback")
}

/// Heuristic stand-in for Jev's two `noul` questions (call still matters?
/// result still needed verbatim?): returns `(keep_call, keep_result)`.
fn decide(tool: &str, result: &str) -> (bool, bool) {
    if is_anchor(tool) {
        return (true, true);
    }
    // Chars, not bytes: the thresholds are named `_CHARS` and the truncate
    // head is 300 chars, so a multibyte result must clear the same bar.
    let len = result.chars().count();
    if len < KEEP_BELOW_CHARS {
        return (true, true);
    }
    if len > DROP_ABOVE_CHARS {
        if is_rerunnable(tool) && !looks_like_error(result) {
            return (false, false);
        }
        return (true, false);
    }
    if len > TRUNCATE_ABOVE_CHARS {
        return (true, false);
    }
    (true, true)
}

/// Byte length of the span (content + tool-call framing). Bytes, not
/// chars, deliberately: both sides of the reduction ratio use the same
/// unit so it cancels, and it matches the estimator's byte basis — while
/// the per-result keep/truncate/drop gates above are true chars.
fn span_chars(messages: &[ChatMessage], start: usize, end: usize) -> usize {
    let mut total = 0;
    for msg in &messages[start..end] {
        total += msg.content.as_deref().map_or(0, str::len);
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                total += call.function.name.len() + call.function.arguments.len();
            }
        }
    }
    total
}

/// Verbatim prune of `messages[start..end]` in place: drop or head-truncate
/// stale tool results, preserving call→result pairing (never orphans a
/// result). Messages outside the span, user/assistant text, and anchors are
/// untouched. Returns per-reason counts plus char savings.
pub(crate) fn prune_span(messages: &mut Vec<ChatMessage>, start: usize, end: usize) -> JevStats {
    // Callers pass the archivable span from `find_cut_point`; an inverted
    // range is a caller bug — loud in tests, clamped to empty in release.
    debug_assert!(start <= end, "jev prune_span: start {start} > end {end}");
    let len = messages.len();
    let start = start.min(len);
    let end = end.min(len).max(start);
    if end - start == 0 {
        return JevStats::default();
    }
    let original_chars = span_chars(messages, start, end);

    // Owner lookup: call id → (assistant index, tool name). Scanned from
    // the transcript head so results whose call sits before the span still
    // resolve (they truncate, never drop — no orphaned results).
    let mut owners: HashMap<String, (usize, String)> = HashMap::new();
    for (idx, msg) in messages.iter().enumerate().take(end) {
        if msg.role != Role::Assistant {
            continue;
        }
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                owners
                    .entry(call.id.clone())
                    .or_insert((idx, call.function.name.clone()));
            }
        }
    }

    let mut drop_ids: HashSet<String> = HashSet::new();
    let mut truncate: HashMap<usize, (String, usize, String)> = HashMap::new();
    let mut kept = 0usize;
    let mut dropped = 0usize;
    let mut truncated = 0usize;

    for (idx, msg) in messages.iter().enumerate().take(end).skip(start) {
        if msg.role != Role::Tool {
            continue;
        }
        let call_id = msg.tool_call_id.clone().unwrap_or_default();
        if call_id.is_empty() {
            continue;
        }
        let Some((owner_idx, tool)) = owners.get(&call_id).cloned() else {
            continue;
        };
        // A call owned outside the span stays — only its in-span result may
        // be truncated, never dropped (no orphaned results).
        let owned_inside = owner_idx >= start && owner_idx < end;
        let result = msg.content.clone().unwrap_or_default();
        let (keep_call, keep_result) = decide(&tool, &result);
        if keep_result {
            kept += 1;
        } else if keep_call || !owned_inside {
            let head: String = result.chars().take(JEV_TRUNCATE_HEAD_CHARS).collect();
            truncate.insert(idx, (head, result.len(), tool));
            truncated += 1;
        } else {
            drop_ids.insert(call_id);
            dropped += 1;
        }
    }

    for (idx, (head, original_len, tool)) in &truncate {
        if let Some(msg) = messages.get_mut(*idx) {
            let omitted = original_len.saturating_sub(head.len());
            msg.content = Some(format!(
                "{head}\n[…jev: truncated {omitted} bytes, re-run `{tool}` to restore]"
            ));
        }
    }

    // Remove dropped calls from their assistants.
    for msg in messages.iter_mut().take(end).skip(start) {
        if msg.role != Role::Assistant {
            continue;
        }
        if let Some(calls) = msg.tool_calls.as_mut() {
            calls.retain(|c| !drop_ids.contains(&c.id));
        }
    }

    // Drop tool messages whose call was dropped, plus assistants emptied of
    // both text and calls (never the span head — it may own survivors).
    let mut remove: HashSet<usize> = HashSet::new();
    for (idx, msg) in messages.iter().enumerate().take(end).skip(start) {
        if msg.role == Role::Tool
            && msg
                .tool_call_id
                .as_deref()
                .is_some_and(|id| drop_ids.contains(id))
        {
            remove.insert(idx);
        }
    }
    for (idx, msg) in messages.iter().enumerate().take(end).skip(start) {
        if idx == start {
            continue;
        }
        let empty = msg.role == Role::Assistant
            && msg.content.as_deref().is_none_or(|c| c.trim().is_empty())
            && msg.tool_calls.as_deref().is_none_or(<[_]>::is_empty);
        if empty {
            remove.insert(idx);
        }
    }
    if !remove.is_empty() {
        let mut kept_msgs = Vec::with_capacity(messages.len() - remove.len());
        for (idx, msg) in messages.drain(..).enumerate() {
            if remove.contains(&idx) {
                continue;
            }
            kept_msgs.push(msg);
        }
        *messages = kept_msgs;
    }

    let removed_before_end = remove.iter().filter(|i| **i < end).count();
    let new_end = end.saturating_sub(removed_before_end);
    let new_start = start.min(messages.len());
    let new_chars = span_chars(messages, new_start, new_end.min(messages.len()));
    JevStats {
        kept,
        truncated,
        dropped,
        original_chars,
        freed_chars: original_chars.saturating_sub(new_chars),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{FunctionCall, LlmToolCall};

    fn call(id: &str, name: &str) -> LlmToolCall {
        LlmToolCall {
            id: id.into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: "{}".into(),
            },
        }
    }

    fn history() -> Vec<ChatMessage> {
        let mut msgs = vec![ChatMessage::system("sys")];
        // Old, large, re-runnable read → drop candidate.
        msgs.push(ChatMessage::assistant_calls(None, vec![call("c1", "read")]));
        msgs.push(ChatMessage::tool_result("c1", "x".repeat(12_000)));
        // Old, medium bash → truncate candidate.
        msgs.push(ChatMessage::assistant_calls(None, vec![call("c2", "bash")]));
        msgs.push(ChatMessage::tool_result("c2", "y".repeat(5_000)));
        msgs.push(ChatMessage::user("keep me"));
        msgs
    }

    #[test]
    fn drops_old_large_rerunnable_and_truncates_medium() {
        let mut msgs = history();
        let stats = prune_span(&mut msgs, 1, 5);
        assert_eq!(stats.dropped, 1);
        assert_eq!(stats.truncated, 1);
        assert!(is_worthwhile(&stats), "{stats:?}");
        // No orphaned result: c1's tool message is gone with its call.
        assert!(!msgs.iter().any(|m| m.tool_call_id.as_deref() == Some("c1")));
        let c2 = msgs
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("c2"))
            .expect("truncated result stays");
        assert!(c2.content_str().contains("jev: truncated"));
        // User text survives verbatim.
        assert!(msgs.iter().any(|m| m.content_str() == "keep me"));
    }

    #[test]
    fn large_bash_output_truncates_but_never_drops() {
        // `bash` is mutating: no re-run restores a side effect, so even a
        // huge result only truncates — the call and its evidence survive.
        let mut msgs = vec![ChatMessage::system("sys")];
        msgs.push(ChatMessage::assistant_calls(None, vec![call("b1", "bash")]));
        msgs.push(ChatMessage::tool_result("b1", "z".repeat(50_000)));
        msgs.push(ChatMessage::user("tail"));
        let stats = prune_span(&mut msgs, 1, 3);
        assert_eq!(stats.dropped, 0, "{stats:?}");
        assert_eq!(stats.truncated, 1, "{stats:?}");
        let kept = msgs
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("b1"))
            .expect("bash result stays");
        assert!(kept.content_str().contains("jev: truncated"));
    }

    #[test]
    fn anchors_and_errors_are_never_dropped() {
        let mut msgs = vec![ChatMessage::system("sys")];
        msgs.push(ChatMessage::assistant_calls(
            None,
            vec![call("p1", "update_plan"), call("e1", "bash")],
        ));
        msgs.push(ChatMessage::tool_result("p1", "plan ".repeat(3_000)));
        msgs.push(ChatMessage::tool_result(
            "e1",
            format!("ERROR boom {}", "e".repeat(11_000)),
        ));
        msgs.push(ChatMessage::user("tail"));
        let stats = prune_span(&mut msgs, 1, 4);
        assert_eq!(stats.dropped, 0, "{stats:?}");
        assert!(msgs.iter().any(|m| m.tool_call_id.as_deref() == Some("p1")));
        assert!(msgs.iter().any(|m| m.tool_call_id.as_deref() == Some("e1")));
    }

    #[test]
    fn cross_span_owner_truncates_without_orphaning() {
        let mut msgs = vec![ChatMessage::system("sys")];
        msgs.push(ChatMessage::assistant_calls(None, vec![call("c1", "read")]));
        msgs.push(ChatMessage::tool_result("c1", "x".repeat(12_000)));
        // Prune only the result; the owning call sits before the span.
        let stats = prune_span(&mut msgs, 2, 3);
        assert_eq!(stats.dropped, 0, "{stats:?}");
        assert_eq!(stats.truncated, 1, "{stats:?}");
        // The call survives, so the (truncated) result is not orphaned.
        assert!(msgs.iter().any(|m| m
            .tool_calls
            .as_deref()
            .is_some_and(|cs| cs.iter().any(|c| c.id == "c1"))));
    }

    #[test]
    fn tiny_span_is_not_worthwhile() {
        let mut msgs = vec![ChatMessage::system("sys"), ChatMessage::user("hi")];
        let stats = prune_span(&mut msgs, 1, 2);
        assert!(!is_worthwhile(&stats));
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn summary_mode_parses_compaction_knob() {
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os(COMPACTION_ENV);
        let legacy_prev = std::env::var_os(LEGACY_COMPACTION_ENV);
        std::env::remove_var(LEGACY_COMPACTION_ENV);
        std::env::set_var(COMPACTION_ENV, "jev");
        assert_eq!(summary_mode(), SummaryMode::Jev);
        assert!(summary_mode().prunes_jev());
        std::env::set_var(COMPACTION_ENV, "JEV");
        assert_eq!(summary_mode(), SummaryMode::Jev);
        std::env::set_var(COMPACTION_ENV, "llm");
        assert_eq!(summary_mode(), SummaryMode::Llm);
        assert!(!summary_mode().prunes_jev());
        std::env::set_var(COMPACTION_ENV, "LLM");
        assert_eq!(summary_mode(), SummaryMode::Llm);
        // `1` stays accepted as an alias for the LLM mode.
        std::env::set_var(COMPACTION_ENV, "1");
        assert_eq!(summary_mode(), SummaryMode::Llm);
        // Explicit offs stay silent and deterministic.
        for off in ["0", "false", "off", "no", ""] {
            std::env::set_var(COMPACTION_ENV, off);
            assert_eq!(summary_mode(), SummaryMode::Deterministic);
        }
        // Garbage warns once (see `warn_once`) and stays deterministic —
        // a typo must never silently disable the selected mode.
        std::env::set_var(COMPACTION_ENV, "jve");
        assert_eq!(summary_mode(), SummaryMode::Deterministic);
        assert!(!summary_mode().prunes_jev());
        std::env::remove_var(COMPACTION_ENV);
        assert_eq!(summary_mode(), SummaryMode::Deterministic);
        // Deprecated `DEX_COMPACTION_LLM=1` still selects the LLM mode
        // when the new knob is unset, and the new knob wins when set.
        std::env::remove_var(COMPACTION_ENV);
        std::env::set_var(LEGACY_COMPACTION_ENV, "1");
        assert_eq!(summary_mode(), SummaryMode::Llm);
        let (value, source) = compaction_doctor();
        assert_eq!(value, "LLM summary");
        assert!(source.contains(LEGACY_COMPACTION_ENV), "{source}");
        std::env::set_var(COMPACTION_ENV, "jev");
        assert_eq!(summary_mode(), SummaryMode::Jev);
        match prev {
            Some(v) => std::env::set_var(COMPACTION_ENV, v),
            None => std::env::remove_var(COMPACTION_ENV),
        }
        match legacy_prev {
            Some(v) => std::env::set_var(LEGACY_COMPACTION_ENV, v),
            None => std::env::remove_var(LEGACY_COMPACTION_ENV),
        }
    }
}
