use tokio::sync::mpsc;

mod deterministic;
pub(crate) mod verbatim;

use deterministic::{attach_file_section, extract_file_ops_from_message};
pub(crate) use deterministic::{deterministic_summary, FileOps};

use crate::agent::state::CancellationSource;
use crate::agent::tokens::{message_char_len, PER_MESSAGE_OVERHEAD};
use crate::llm::config::LlmConfig;
use crate::llm::dispatch::complete as call_llm;
use crate::protocol::{ChatMessage, Role, Usage};

// The compaction tests share the pure estimator from the protocol layer.
#[cfg(test)]
pub(crate) use super::tokens::estimate_tokens;

/// Compaction settings — token-based keep-recent with a message-count fallback.
/// `keepRecentTokens=20000` (token-based); dex keeps 12 messages as
/// fallback when total tokens < keep_recent (dex compat for many short msgs).
pub(crate) const KEEP_RECENT_MESSAGES: usize = 12;
pub(crate) const MIN_MESSAGES_TO_SUMMARIZE: usize = 8;

fn is_cut_point_message(msg: &ChatMessage) -> bool {
    matches!(msg.role, Role::User | Role::Assistant)
}

fn is_turn_start_message(msg: &ChatMessage) -> bool {
    msg.role == Role::User
}

fn find_valid_cut_points(messages: &[ChatMessage], start: usize, end: usize) -> Vec<usize> {
    (start..end)
        .filter(|&i| is_cut_point_message(&messages[i]))
        .collect()
}

fn find_turn_start_index(
    messages: &[ChatMessage],
    entry_index: usize,
    start: usize,
) -> Option<usize> {
    (start..=entry_index)
        .rev()
        .find(|&i| is_turn_start_message(&messages[i]))
}

#[derive(Debug)]
struct CutPoint {
    first_kept_index: usize,
    turn_start_index: Option<usize>,
    is_split_turn: bool,
}

/// Build the `CutPoint` for keeping from `idx`: a turn start cuts exactly
/// there; otherwise record the turn start (searching back to `start`) and
/// flag the split turn.
fn cut_point_at(messages: &[ChatMessage], idx: usize, start: usize) -> CutPoint {
    let starts_turn = is_turn_start_message(&messages[idx]);
    let turn_start = if starts_turn {
        None
    } else {
        find_turn_start_index(messages, idx, start)
    };
    CutPoint {
        first_kept_index: idx,
        turn_start_index: turn_start,
        is_split_turn: !starts_turn && turn_start.is_some(),
    }
}

/// Back `idx` up over tool messages so a cut never orphans a tool result
/// from its request; never crosses `start`.
fn back_up_over_tools(messages: &[ChatMessage], idx: usize, start: usize) -> usize {
    let mut idx = idx;
    while idx > start && messages[idx].role == Role::Tool {
        idx -= 1;
    }
    idx
}

/// Find the cut point — walk backwards until `keepRecentTokens`, cut at
/// next valid user/assistant boundary, handle split turns.
/// `start` is boundaryStart (after previous compaction), `end` is messages.len().
/// `min_keep_messages` is the fallback recent-window size (12 normally,
/// 4 when an emergency compaction must cut below the comfort floor).
fn find_cut_point(
    messages: &[ChatMessage],
    start: usize,
    end: usize,
    keep_recent_tokens: u64,
    min_keep_messages: usize,
) -> Option<CutPoint> {
    let cut_points = find_valid_cut_points(messages, start, end);
    if cut_points.is_empty() {
        return None;
    }
    let mut recent_chars: u64 = 0;
    let mut recent_count: u64 = 0;
    let mut cut_index = cut_points[0];
    let mut hit_budget = false;
    for i in (start..end).rev() {
        // Same sum-then-divide formula as `TokenLedger` (chars/4 +
        // count*OVERHEAD over the whole recent window): divide-then-sum per
        // message drifts up to 3/4 token per message from the gate's ledger.
        let len = message_char_len(&messages[i]) as u64;
        if len == 0 && PER_MESSAGE_OVERHEAD == 0 {
            continue;
        }
        recent_chars += len;
        recent_count += 1;
        let recent = recent_chars / 4 + recent_count * PER_MESSAGE_OVERHEAD;
        if recent >= keep_recent_tokens {
            // closest valid cut at or after i
            for &cp in &cut_points {
                if cp >= i {
                    cut_index = cp;
                    break;
                }
            }
            hit_budget = true;
            break;
        }
    }
    if !hit_budget {
        // Nothing token-worthy to summarize — keep a fallback window
        // so many short messages still compact (dex compat).
        if end - start <= 1 + min_keep_messages {
            return None;
        }
        cut_index = end - min_keep_messages;
        // snap to valid cut point at or after
        let mut snapped = None;
        for &cp in &cut_points {
            if cp >= cut_index {
                snapped = Some(cp);
                break;
            }
        }
        cut_index = snapped.unwrap_or(cut_index);
        // ensure snap didn't land on tool (tool not in cut_points, so safe)
    }

    // Never orphan tool: if cut lands on tool, back up (shouldn't happen via cut_points)
    let cut_index = back_up_over_tools(messages, cut_index, start);

    // ponytail: never evict the most recent real user prompt — compaction
    // inside a turn can push it out of the keep_recent window and the model
    // then re-answers an old goal with similar output.
    if let Some(last_user) = messages[start..end]
        .iter()
        .rposition(|m| m.role == Role::User && m.name.as_deref() != Some("summary"))
        .map(|p| p + start)
    {
        if cut_index > last_user {
            // Would evict last user — keep from last_user instead, or abort if too small.
            let adjusted = back_up_over_tools(messages, last_user, start);
            // If adjusting would make summarized span too small, skip compaction.
            if adjusted <= start + MIN_MESSAGES_TO_SUMMARIZE {
                return None;
            }
            return Some(cut_point_at(messages, adjusted, start));
        }
    }

    Some(cut_point_at(messages, cut_index, start))
}

/// Serialize the conversation for the summarizer — prevents it from continuing the conversation.
/// Tool results truncated to 2000 chars.
fn serialize_conversation(messages: &[ChatMessage]) -> String {
    const TOOL_RESULT_MAX: usize = 2000;
    let mut parts = Vec::new();
    for msg in messages {
        match msg.role {
            Role::User => {
                if let Some(c) = &msg.content {
                    if !c.trim().is_empty() {
                        parts.push(format!("[User]: {}", c.trim()));
                    }
                }
            }
            Role::Assistant => {
                if let Some(calls) = &msg.tool_calls {
                    let calls_str: Vec<String> = calls
                        .iter()
                        .map(|call| {
                            let args = &call.function.arguments;
                            format!("{}({})", call.function.name, args)
                        })
                        .collect();
                    if !calls_str.is_empty() {
                        parts.push(format!("[Assistant tool calls]: {}", calls_str.join("; ")));
                    }
                }
                if let Some(c) = &msg.content {
                    if !c.trim().is_empty() {
                        parts.push(format!("[Assistant]: {}", c.trim()));
                    }
                }
            }
            Role::Tool => {
                if let Some(c) = &msg.content {
                    let truncated = if c.len() > TOOL_RESULT_MAX {
                        format!(
                            "{}[... {} more characters truncated]",
                            &c[..TOOL_RESULT_MAX],
                            c.len() - TOOL_RESULT_MAX
                        )
                    } else {
                        c.clone()
                    };
                    if !truncated.trim().is_empty() {
                        parts.push(format!("[Tool result]: {}", truncated.trim()));
                    }
                }
            }
            Role::System => {}
        }
    }
    parts.join("\n\n")
}

const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse this EXACT format:\n\n## Goal\n[Preserve existing goals, add new ones if the task expanded]\n\n## Constraints & Preferences\n- [Preserve existing, add new ones discovered]\n\n## Progress\n### Done\n- [x] [Include previously done items AND newly completed items]\n\n### In Progress\n- [ ] [Current work - update based on progress]\n\n### Blocked\n- [Current blockers - remove if resolved]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale] (preserve all previous, add new)\n\n## Next Steps\n1. [Update based on current state]\n\n## Critical Context\n- [Preserve important context, add new if needed]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const TURN_PREFIX_SUMMARIZATION_PROMPT: &str = "This is the PREFIX of a turn that was too large to keep. The SUFFIX (recent work) is retained.\n\nSummarize the prefix to provide context for the retained suffix:\n\n## Original Request\n[What did the user ask for in this turn?]\n\n## Early Progress\n- [Key decisions and work done in the prefix]\n\n## Context for Suffix\n- [Information needed to understand the retained recent work]\n\nBe concise. Focus on what's needed to understand the kept suffix.";

pub(crate) async fn summarize_old_messages(
    config: &LlmConfig,
    old: &[ChatMessage],
    cancel: &(dyn CancellationSource + Send + Sync),
    hook_instructions: &[String],
) -> Result<(String, Option<Usage>), Box<dyn std::error::Error + Send + Sync>> {
    // Serialize via `serialize_conversation`,
    // handle previousSummary iterative, file ops via prompt, and custom instructions.
    // Preserve orientation anchors verbatim so compaction never erases the task.
    let has_plan = old.iter().any(|m| m.name.as_deref() == Some("plan"));
    let has_verify = old.iter().any(|m| m.name.as_deref() == Some("verify"));
    let extra = if has_plan || has_verify {
        " Preserve the current goal and plan verbatim and any recent verification failures."
    } else {
        ""
    };
    let previous_summary = old
        .iter()
        .find(|m| m.name.as_deref() == Some("summary"))
        .and_then(|m| m.content.clone());
    let base_prompt = if previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT
    } else {
        SUMMARIZATION_PROMPT
    };
    let mut prompt_text = base_prompt.to_string();
    if !extra.is_empty() {
        prompt_text.push_str(extra);
    }
    // `session.before_compact` instructions: extension guidance rides the
    // summarizer prompt verbatim.
    if !hook_instructions.is_empty() {
        prompt_text.push_str(&format!(
            "\n\nAdditional instructions from session hooks: {}",
            hook_instructions.join(" ")
        ));
    }
    let conversation_text = serialize_conversation(old);
    let full_prompt = if let Some(prev) = &previous_summary {
        format!(
            "<conversation>\n{}\n</conversation>\n\n<previous-summary>\n{}\n</previous-summary>\n\n{}",
            conversation_text, prev, prompt_text
        )
    } else {
        format!(
            "<conversation>\n{}\n</conversation>\n\n{}",
            conversation_text, prompt_text
        )
    };
    let prompt = vec![
        ChatMessage::system("You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified. Do NOT continue the conversation. ONLY output the structured summary."),
        ChatMessage::user(full_prompt),
    ];
    // Dead-drop sink: with sink=None, StreamPrinter prints streamed deltas
    // raw to stdout, which inside the TUI process is the alternate screen —
    // the internal summary then ghosts over the UI until the next resize
    // repaint. A dropped receiver makes every send fail silently instead.
    let (sink, rx) = mpsc::channel(16);
    drop(rx);
    let turn = call_llm(config, &prompt, &[], Some(sink), cancel).await?;
    Ok((turn.message.content.unwrap_or_default(), turn.usage))
}

/// Offline fallback summary: the harness override when set, otherwise the
/// deterministic structured checkpoint.
fn summarize_fallback(
    fallback: Option<&dyn dex_agent_core::Summarizer>,
    old: &[ChatMessage],
    turn_prefix: &[ChatMessage],
    previous_summary: Option<&str>,
    file_ops: &FileOps,
) -> String {
    if let Some(summarizer) = fallback {
        return summarizer.summarize(old, turn_prefix, previous_summary, file_ops);
    }
    deterministic_summary(old, turn_prefix, previous_summary, file_ops)
}

/// Fold one call's usage into an accumulator (summing prompt, completion,
/// and cached counts; cache detail is dropped when any call omits it).
fn merge_usage(acc: &mut Option<Usage>, u: Option<Usage>) {
    let Some(u) = u else { return };
    *acc = Some(match acc.take() {
        None => u,
        Some(a) => Usage {
            prompt_tokens: a.prompt_tokens + u.prompt_tokens,
            completion_tokens: a.completion_tokens + u.completion_tokens,
            cached_tokens: match (a.cached_tokens, u.cached_tokens) {
                (Some(x), Some(y)) => Some(x + y),
                _ => None,
            },
        },
    });
}

/// Find the splice point that keeps the recent window intact and never
/// orphans a tool_call/tool pair: if the window would start on a `tool`
/// message, back up to the assistant that issued the batch. Anything at or
/// after the cutoff is kept whole, so an assistant-with-tool_calls at the
/// cutoff is fine (its results follow it). Returns None if there is nothing
/// large enough to summarize.
/// Walk back until keepRecentTokens (20000) is reached; fall back
/// to KEEP_RECENT_MESSAGES (12) when total tokens < keep_recent.
#[cfg(test)]
pub(crate) fn find_cutoff_by_tokens(
    messages: &[ChatMessage],
    keep_recent_tokens: u64,
) -> Option<usize> {
    let total = messages.len();
    if total <= 1 {
        return None;
    }
    // Find valid cut points and walk backwards
    // Check for a previous summary to set the boundary start
    let boundary_start = messages
        .iter()
        .rposition(|m| m.name.as_deref() == Some("summary"))
        .map(|idx| idx + 1)
        .unwrap_or(1);
    if boundary_start >= total {
        return None;
    }
    if let Some(cp) = find_cut_point(
        messages,
        boundary_start,
        total,
        keep_recent_tokens,
        KEEP_RECENT_MESSAGES,
    ) {
        if cp.first_kept_index <= boundary_start + MIN_MESSAGES_TO_SUMMARIZE {
            return None;
        }
        // Return first_kept_index as cutoff (dex's splice point)
        // Note: caller splices 1..cutoff or boundary_start..first_kept; for dex compat
        // we return first_kept_index, but if boundary_start !=1, need to handle.
        // For now, if boundary_start !=1, we still return first_kept_index
        // and caller will splice boundary_start..first_kept (handled in compact_history).
        // For simple cases boundary_start==1, this matches old behavior.
        Some(cp.first_kept_index)
    } else {
        None
    }
}

/// LLM summarization path (`DEX_COMPACTION=llm`): summarize the history
/// and — for split turns — the turn prefix with the configured model,
/// folding the summarizer spend into `usage_total`. Empty or failed replies
/// fall back to the deterministic summary; a cancelled summarizer fails the
/// compaction with the exact wording the turn loop matches
/// (`e.contains("cancelled")`).
#[allow(clippy::too_many_arguments)]
async fn llm_summary(
    config: &LlmConfig,
    cancel: &(dyn CancellationSource + Send + Sync),
    messages_to_summarize: &[ChatMessage],
    turn_prefix_messages: &[ChatMessage],
    is_split_turn: bool,
    previous_summary: Option<&str>,
    file_ops: &FileOps,
    usage_total: &mut Option<Usage>,
    hook_instructions: &[String],
    fallback: Option<&dyn dex_agent_core::Summarizer>,
) -> Result<String, String> {
    let history_summary = if !messages_to_summarize.is_empty() {
        match summarize_old_messages(config, messages_to_summarize, cancel, hook_instructions).await
        {
            Ok((s, u)) if !s.trim().is_empty() => {
                merge_usage(usage_total, u);
                s
            }
            Ok((_, u)) => {
                merge_usage(usage_total, u);
                summarize_fallback(
                    fallback,
                    messages_to_summarize,
                    &[],
                    previous_summary,
                    file_ops,
                )
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("cancelled") || msg.contains("cancellation") {
                    return Err(format!("history compaction cancelled: {msg}"));
                }
                summarize_fallback(
                    fallback,
                    messages_to_summarize,
                    &[],
                    previous_summary,
                    file_ops,
                )
            }
        }
    } else {
        "No prior history.".to_string()
    };
    if is_split_turn && !turn_prefix_messages.is_empty() {
        // Turn prefix summary with smaller budget prompt
        let prefix_conversation = serialize_conversation(turn_prefix_messages);
        let prefix_prompt = vec![
            ChatMessage::system(
                "You are a context summarization assistant. ONLY output the structured summary.",
            ),
            ChatMessage::user(format!(
                "<conversation>\n{}\n</conversation>\n\n{}",
                prefix_conversation, TURN_PREFIX_SUMMARIZATION_PROMPT
            )),
        ];
        let (sink, rx) = mpsc::channel(16);
        drop(rx);
        let prefix_summary = match call_llm(config, &prefix_prompt, &[], Some(sink), cancel).await {
            Ok(turn) => {
                merge_usage(usage_total, turn.usage);
                turn.message.content.unwrap_or_default()
            }
            Err(_) => deterministic_summary(&[], turn_prefix_messages, None, &FileOps::default()),
        };
        // Merge: history + "---" + turn context
        let file_section = attach_file_section(file_ops);
        Ok(format!(
            "{}\n\n---\n\n**Turn Context (split turn):**\n\n{}{}",
            history_summary.trim(),
            prefix_summary.trim(),
            file_section
        ))
    } else {
        let file_section = attach_file_section(file_ops);
        if file_section.is_empty() {
            Ok(history_summary)
        } else {
            Ok(format!("{}{}", history_summary.trim(), file_section))
        }
    }
}

pub(crate) async fn compact_history(
    _config: &LlmConfig,
    messages: &mut Vec<ChatMessage>,
    _cancel: &(dyn CancellationSource + Send + Sync),
    emergency: bool,
    // Whether a worthwhile verbatim prune beats the summary. The threshold
    // path derives both from `summary_mode()` (`DEX_COMPACTION`).
    jev_prune: bool,
    // Fallback summarizer when no hook summary applies and the prune is
    // skipped or does not pay — always the threshold knob's choice, parsed
    // once by the caller instead of re-read here.
    summarizer: crate::agent::compaction::verbatim::SummaryMode,
    // Overwritable harness decisions: cut window, prune scorer, fallback
    // summarizer override, and the prune-payoff ratio.
    harness: &crate::agent::composable::DexHarness,
) -> Result<(bool, Option<Usage>), String> {
    let total = messages.len();
    if total <= 1 {
        return Ok((false, None));
    }
    // `session.before_compact` (plan §7): hooks may cancel the compaction
    // (same wording the turn loop matches), hand the summarizer extra
    // instructions, or replace the summary outright (P3 surface, wired
    // here because the envelope key already exists).
    let hook = crate::extensions::apply_before_compact(emergency, total, _cancel).await;
    if hook.cancel {
        return Err(
            "history compaction cancelled: cancelled by session.before_compact hook".to_string(),
        );
    }
    let boundary_start = messages
        .iter()
        .rposition(|m| m.name.as_deref() == Some("summary"))
        .map(|idx| idx + 1)
        .unwrap_or(1);
    // Emergency cut: the provider has already declared the input over its
    // real limit, so the comfort floors that normally protect recency are
    // what stop the retry. Shrink the keep window and the minimum-summarize
    // guard to a floor that still leaves a coherent transcript. The window
    // comes from the harness cut config (tokens from `LlmConfig`), so one
    // override point covers both paths.
    let cut = {
        let base = harness.config.cut_for(_config);
        if emergency {
            base.emergency()
        } else {
            base
        }
    };
    let keep_recent_tokens = cut.keep_recent_tokens;
    let min_keep_messages = cut.min_keep_messages;
    let min_to_summarize = cut.min_to_summarize;
    let cp = match find_cut_point(
        messages,
        boundary_start,
        total,
        keep_recent_tokens,
        min_keep_messages,
    ) {
        Some(c) => c,
        None => return Ok((false, None)),
    };
    let first_kept = cp.first_kept_index;
    if first_kept <= boundary_start + min_to_summarize {
        return Ok((false, None));
    }
    if first_kept >= total {
        return Ok((false, None));
    }

    // messages_to_summarize = boundary_start..history_end; turn_prefix = turn_start..first_kept if split
    let history_end = if cp.is_split_turn {
        cp.turn_start_index.unwrap_or(first_kept)
    } else {
        first_kept
    };
    let turn_prefix_start = cp.turn_start_index.unwrap_or(first_kept);
    let messages_to_summarize: Vec<ChatMessage> = messages[boundary_start..history_end].to_vec();
    let turn_prefix_messages: Vec<ChatMessage> = if cp.is_split_turn {
        messages[turn_prefix_start..first_kept].to_vec()
    } else {
        Vec::new()
    };
    if messages_to_summarize.is_empty() && turn_prefix_messages.is_empty() {
        return Ok((false, None));
    }

    // File ops are cumulative — extracted from the previous compaction + messages
    // Merge base is the LAST summary: each compaction folds the previous
    // checkpoint into the new one, so the latest subsumes every earlier
    // span. Merging from the first would drop spans only intermediate
    // summaries cover when healing an already-stacked transcript.
    let previous_summary = messages
        .iter()
        .rposition(|m| m.name.as_deref() == Some("summary"))
        .and_then(|idx| messages[idx].content.clone());
    let mut file_ops = FileOps::default();
    // Previous compaction's file lists are embedded in previous summary's <read-files> etc,
    // but we parse naively: re-extract from old messages that are being summarized
    // and keep them. For true cumulative, we'd parse previous summary's tags — ponytail: skip parse, add when needed.
    for msg in &messages_to_summarize {
        extract_file_ops_from_message(msg, &mut file_ops);
    }
    for msg in &turn_prefix_messages {
        extract_file_ops_from_message(msg, &mut file_ops);
    }

    // Jev verbatim prune: drop/truncate stale tool outputs in place, no
    // summary message, no LLM spend. Tried on a clone so an insufficient
    // prune discards cleanly and the summarizer below still sees the
    // original span. An explicit hook summary always wins. With
    // `TYPESAFE_API_KEY` + a config `jev:` table, the real Typesafe Jev
    // scorer replaces the heuristic; any live failure falls back to the
    // heuristic inside `prune_span`.
    if hook.summary.is_none() && jev_prune {
        let creds = crate::agent::compaction::verbatim::live_credentials();
        let scorer = if creds.is_some() {
            crate::agent::compaction::verbatim::Scorer::Jev
        } else {
            crate::agent::compaction::verbatim::Scorer::Heuristic
        };
        let live =
            creds.as_ref().map(
                |(endpoint, key)| crate::agent::compaction::verbatim::LiveScorer {
                    endpoint,
                    api_key: key,
                },
            );
        let mut candidate = messages.clone();
        let stats = crate::agent::compaction::verbatim::prune_span(
            &mut candidate,
            boundary_start,
            first_kept,
            scorer,
            &*harness.scorer,
            live.as_ref(),
        )
        .await;
        if crate::agent::compaction::verbatim::meets_reduction(
            &stats,
            harness.config.limits.reduction_ratio(),
        ) {
            dex_runtime::log!(
                Info,
                "jev compaction: dropped {} truncated {} kept {} (freed {} chars, ratio {:.2})",
                stats.dropped,
                stats.truncated,
                stats.kept,
                stats.freed_chars,
                crate::agent::compaction::verbatim::reduction_ratio(&stats)
            );
            *messages = candidate;
            return Ok((true, None));
        }
    }

    // Generate summary — merge two summaries for split turns
    let mut usage_total: Option<Usage> = None;
    let fallback = harness.summarizer.as_deref();
    let summarized = if let Some(summary) = &hook.summary {
        summary.clone()
    } else if summarizer == crate::agent::compaction::verbatim::SummaryMode::Llm {
        llm_summary(
            _config,
            _cancel,
            &messages_to_summarize,
            &turn_prefix_messages,
            cp.is_split_turn,
            previous_summary.as_deref(),
            &file_ops,
            &mut usage_total,
            &hook.instructions,
            fallback,
        )
        .await?
    } else {
        summarize_fallback(
            fallback,
            &messages_to_summarize,
            &turn_prefix_messages,
            previous_summary.as_deref(),
            &file_ops,
        )
    };

    // The kept boundary; dex splices from
    // boundary_start..first_kept. Repeated compactions previously spliced
    // from index 1, which left BOTH the old and the new summary in the
    // transcript — the model then re-read a stale checkpoint (and the old
    // one won on recency in some providers). The new summary subsumes the
    // previous one, so the splice starts at the FIRST summary's position
    // when one exists, keeping exactly one summary entry in history. First
    // (not last) also heals transcripts already stacked by the old bug.
    let summary_msg = ChatMessage::user_named(summarized.trim().to_string(), "summary");
    let splice_start = messages
        .iter()
        .position(|m| m.name.as_deref() == Some("summary"))
        .unwrap_or(1);
    messages.splice(splice_start..first_kept, std::iter::once(summary_msg));
    Ok((true, usage_total))
}

#[cfg(test)]
mod tests {
    use super::{
        compact_history, deterministic_summary, estimate_tokens, extract_file_ops_from_message,
        find_cutoff_by_tokens, ChatMessage, FileOps, KEEP_RECENT_MESSAGES,
    };
    use crate::protocol::{FunctionCall, LlmToolCall, Role};

    /// Save/restore process env around tests that flip dex env vars.
    struct EnvRestore {
        vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvRestore {
        fn take(keys: &[&'static str]) -> Self {
            Self {
                vars: keys.iter().map(|k| (*k, std::env::var_os(k))).collect(),
            }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (key, prev) in self.vars.drain(..) {
                match prev {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn msg(role: Role, content: &str) -> ChatMessage {
        let mut m = ChatMessage::user(content);
        m.role = role;
        m
    }

    #[test]
    fn cutoff_never_lands_on_a_tool_message() {
        let mut messages = vec![msg(Role::System, "sys")];
        for i in 0..20 {
            messages.push(msg(Role::User, &format!("u{i}")));
            messages.push(msg(Role::Assistant, &format!("a{i}")));
        }
        // Force the naive window to start on a tool message: assistant with
        // calls, then tools, right at total - KEEP_RECENT.
        let call = LlmToolCall {
            id: "c1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "read".into(),
                arguments: "{}".into(),
            },
        };
        messages.push(ChatMessage::assistant_calls(None, vec![call]));
        for i in 0..KEEP_RECENT_MESSAGES - 1 {
            messages.push(ChatMessage::tool_result(format!("c1-{i}"), format!("t{i}")));
        }
        let cutoff = find_cutoff_by_tokens(&messages, 20_000).expect("should have a cutoff");
        assert_ne!(
            messages[cutoff].role,
            Role::Tool,
            "cutoff would orphan tool calls"
        );
        // Everything from cutoff on must be self-contained: first kept
        // message is either a user/assistant text message or an assistant
        // that owns the tool calls that follow it.
        let kept = &messages[cutoff..];
        assert!(
            kept[0].role != Role::Tool
                && (kept[0].tool_calls.is_some()
                    || kept
                        .iter()
                        .all(|m| m.tool_call_id.is_none()
                            || kept.iter().any(|a| a.tool_calls.is_some()))),
            "kept segment must start a coherent turn"
        );
    }

    #[test]
    fn cutoff_is_none_when_history_is_small() {
        let messages: Vec<ChatMessage> = std::iter::once(msg(Role::System, "sys"))
            .chain((0..KEEP_RECENT_MESSAGES).map(|i| msg(Role::User, &format!("m{i}"))))
            .collect();
        assert!(find_cutoff_by_tokens(&messages, 20_000).is_none());
    }

    #[test]
    fn estimator_counts_tool_calls_and_message_overhead() {
        let call = LlmToolCall {
            id: "abc".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "read".into(),
                arguments: "{\"path\":\"src/main.rs\"}".into(),
            },
        };
        let messages = vec![
            msg(Role::System, "hello"),
            ChatMessage::assistant_calls(None, vec![call]),
        ];
        // 5 content chars + 30 tool-call chars + framing, /4 + overhead.
        let est = estimate_tokens(&messages);
        assert!(est >= 2, "estimator must not undercount to zero: {est}");
        // Empty history estimates to zero, not garbage.
        assert_eq!(estimate_tokens(&[]), 0);
    }

    #[test]
    fn estimator_counts_replayed_reasoning() {
        let plain = vec![msg(Role::Assistant, "hello")];
        let mut with_reasoning = ChatMessage::assistant("hello");
        with_reasoning.reasoning_items = Some(vec![serde_json::json!({
            "type": "reasoning",
            "id": "r1",
            "encrypted_content": "blob1",
        })]);
        with_reasoning.reasoning_content = Some("step 1".to_string());
        let with_reasoning = vec![with_reasoning];
        assert!(
            estimate_tokens(&with_reasoning) > estimate_tokens(&plain),
            "replayed reasoning must count toward the window"
        );
    }

    #[test]
    fn reasoning_item_writer_length_matches_materialized_json() {
        // Pin the `ByteCounter` equivalence: `to_writer` on a `Value` emits
        // the same compact bytes as `to_string`, so the writer-based length
        // used by `message_char_len` must equal the materialized length
        // exactly — a mismatch skews the compaction cut point.
        let items = vec![
            serde_json::json!({"type":"reasoning","id":"r1","encrypted_content":"blob1"}),
            serde_json::json!({
                "type":"reasoning","id":"r2",
                "summary":[{"type":"summary_text","text":"visible"}],
                "a":[1,2,3],"nested":{"k":"v"}
            }),
        ];
        let mut with_items = ChatMessage::assistant("hello");
        with_items.reasoning_items = Some(items.clone());
        let base = ChatMessage::assistant("hello");
        let item_chars: usize = items.iter().map(|i| i.to_string().len()).sum();
        assert_eq!(
            crate::agent::tokens::message_char_len(&with_items)
                - crate::agent::tokens::message_char_len(&base),
            item_chars,
            "to_writer byte length must equal the to_string length"
        );
    }

    #[test]
    fn cutoff_preserves_last_user_prompt() {
        // Compaction inside a turn with many tool calls must not evict the prompt.
        let mut messages = vec![msg(Role::System, "sys")];
        messages.push(msg(Role::User, "first goal: build foo"));
        for i in 0..5 {
            messages.push(msg(Role::Assistant, &format!("a{i}")));
            messages.push(msg(Role::Tool, &format!("t{i}")));
        }
        messages.push(msg(Role::User, "second prompt that must stay"));
        // Add many tool messages to push the second prompt out of keep_recent window
        let call = LlmToolCall {
            id: "c1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "read".into(),
                arguments: "{}".into(),
            },
        };
        messages.push(ChatMessage::assistant_calls(None, vec![call]));
        for i in 0..KEEP_RECENT_MESSAGES {
            messages.push(ChatMessage::tool_result(
                format!("c1-{i}"),
                format!("tool result {i}"),
            ));
        }
        let cutoff = find_cutoff_by_tokens(&messages, 20_000).expect("should have cutoff");
        // Last user is at index where second prompt lives; cutoff must not be after it
        let last_user = messages
            .iter()
            .rposition(|m| m.role == Role::User && m.name.as_deref() != Some("summary"))
            .unwrap();
        assert!(
            cutoff <= last_user,
            "cutoff {} evicted last_user {} (must not happen)",
            cutoff,
            last_user
        );
        assert_ne!(messages[cutoff].role, Role::Tool);
    }

    #[test]
    fn deterministic_summary_is_structured() {
        let mut old = vec![
            msg(Role::User, "Goal: fix compaction"),
            msg(Role::Assistant, "did read src/foo.rs"),
        ];
        old[0].name = None;
        // Simulate a read tool call to test file ops
        let call = LlmToolCall {
            id: "c1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "read".into(),
                arguments: r#"{"path":"src/foo.rs"}"#.into(),
            },
        };
        let mut ass = msg(Role::Assistant, "");
        ass.tool_calls = Some(vec![call]);
        old.push(ass);
        let mut ops = FileOps::default();
        for m in &old {
            extract_file_ops_from_message(m, &mut ops);
        }
        let summary = deterministic_summary(&old, &[], None, &ops);
        assert!(
            summary.contains("## Goal"),
            "summary structure missing Goal"
        );
        assert!(
            summary.contains("## Progress"),
            "summary structure missing Progress"
        );
        assert!(summary.contains("<read-files>"), "file ops missing");
        assert!(summary.contains("src/foo.rs"));
    }

    /// Repeated compactions must keep exactly ONE summary entry: the new
    /// checkpoint subsumes the previous one. The old splice (from index 1)
    /// stacked summaries, so the model kept re-reading a stale checkpoint.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env var must stay unset across the compaction awaits
    async fn repeated_compaction_keeps_a_single_summary() {
        let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&["DEX_COMPACTION"]);
        std::env::remove_var("DEX_COMPACTION");
        let config = crate::llm::config::tests::test_cfg();
        let mut messages = vec![msg(Role::System, "sys")];
        messages.push(msg(Role::User, "goal: build the thing"));
        for i in 0..20 {
            messages.push(msg(Role::User, &format!("u{i}: {}", "x".repeat(200))));
            messages.push(msg(Role::Assistant, &format!("a{i}: {}", "y".repeat(200))));
        }
        let (compacted, _) = compact_history(
            &config,
            &mut messages,
            &crate::agent::state::GlobalCancellation,
            false,
            false,
            crate::agent::compaction::verbatim::summary_mode(),
            &crate::agent::composable::DexHarness::default(),
        )
        .await
        .unwrap();
        assert!(compacted, "first compaction must run");
        // Grow the history again and compact a second time.
        for i in 20..40 {
            messages.push(msg(Role::User, &format!("u{i}: {}", "x".repeat(200))));
            messages.push(msg(Role::Assistant, &format!("a{i}: {}", "y".repeat(200))));
        }
        let (compacted, _) = compact_history(
            &config,
            &mut messages,
            &crate::agent::state::GlobalCancellation,
            false,
            false,
            crate::agent::compaction::verbatim::summary_mode(),
            &crate::agent::composable::DexHarness::default(),
        )
        .await
        .unwrap();
        assert!(compacted, "second compaction must run");
        let summaries: Vec<_> = messages
            .iter()
            .filter(|m| m.name.as_deref() == Some("summary"))
            .collect();
        assert_eq!(summaries.len(), 1, "one checkpoint, not a stack");
        // The system prompt stays at the head.
        assert_eq!(messages[0].role, Role::System);
        assert_eq!(messages[1].name.as_deref(), Some("summary"));
    }

    /// Transcripts already stacked by the old bug (two summaries) heal to
    /// one: the splice starts at the FIRST summary, not the last.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env var must stay unset across the compaction awaits
    async fn stacked_summaries_heal_to_a_single_summary() {
        let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&["DEX_COMPACTION"]);
        std::env::remove_var("DEX_COMPACTION");
        let config = crate::llm::config::tests::test_cfg();
        let mut messages = vec![msg(Role::System, "sys")];
        let mut s1 = msg(Role::User, "old checkpoint");
        s1.name = Some("summary".into());
        let mut s2 = msg(Role::User, "stale checkpoint");
        s2.name = Some("summary".into());
        messages.push(s1);
        messages.push(msg(Role::User, "goal: build the thing"));
        messages.push(s2);
        for i in 0..20 {
            messages.push(msg(Role::User, &format!("u{i}: {}", "x".repeat(200))));
            messages.push(msg(Role::Assistant, &format!("a{i}: {}", "y".repeat(200))));
        }
        let (compacted, _) = compact_history(
            &config,
            &mut messages,
            &crate::agent::state::GlobalCancellation,
            false,
            false,
            crate::agent::compaction::verbatim::summary_mode(),
            &crate::agent::composable::DexHarness::default(),
        )
        .await
        .unwrap();
        assert!(compacted, "compaction must run");
        let summaries = messages
            .iter()
            .filter(|m| m.name.as_deref() == Some("summary"))
            .count();
        assert_eq!(summaries, 1, "stacked checkpoints must heal to one");
        assert_eq!(messages[0].role, Role::System);
        assert_eq!(messages[1].name.as_deref(), Some("summary"));
        // The merge base is the LAST checkpoint (it subsumes the earlier
        // spans): the surviving summary builds on the newest checkpoint,
        // not the stale first one.
        let text = messages[1].content.as_deref().unwrap_or_default();
        assert!(
            text.contains("stale checkpoint"),
            "new summary must build on the latest checkpoint, got: {text}"
        );
    }

    /// `session.before_compact`: a hook can cancel the compaction (exact
    /// wording the turn loop matches) or replace the summary outright.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env vars must stay put across the compaction awaits
    async fn before_compact_hook_cancels_or_replaces() {
        // Global env-guarded locks, taken in the shared order
        // sessions -> turn -> manager (see the extensions tests) so no
        // two tests can acquire them in opposite order and deadlock.
        let _env_lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // The fixture loads into the process-global extension manager, which
        // every concurrent compaction reads — serialize against the
        // process_turn tests (their emergency compactions would see this
        // hook otherwise).
        let _turn_lock = crate::agent::turn_loop::tests::TEST_TURN_ENV_LOCK
            .lock()
            .await;
        // Same lock the extensions tests use: two concurrent global-manager
        // fixtures would unload each other via `reset_for_tests`.
        let _ext_lock = crate::extensions::tests::TEST_GLOBAL_MANAGER_LOCK
            .lock()
            .await;
        let _guard = EnvRestore::take(&["DEX_COMPACTION"]);
        std::env::remove_var("DEX_COMPACTION");
        let config = crate::llm::config::tests::test_cfg();
        let mut messages = vec![msg(Role::System, "sys")];
        messages.push(msg(Role::User, "goal: build the thing"));
        for i in 0..20 {
            messages.push(msg(Role::User, &format!("u{i}: {}", "x".repeat(200))));
            messages.push(msg(Role::Assistant, &format!("a{i}: {}", "y".repeat(200))));
        }
        // One fixture, both behaviors: emergency compactions cancel, plain
        // ones get their summary replaced. (The global manager keeps loaded
        // extensions for the whole test process, so the two cases share it.)
        let lua = "return function(dex)\n  dex.events.on(\"session.before_compact\", function(ctx, ev)\n    if ev.emergency and ev.messages == 42 then return { cancel = true } end\n    if not ev.emergency and ev.messages == 42 then return { summary = \"HOOK CHECKPOINT\" } end\n  end)\nend\n";
        let manifest = "manifest_version: 1\nid: compact-hook\nversion: 0.1.0\ncapabilities: []\n";
        let root = crate::extensions::tests::fixture_exts(&[("compact-hook", manifest, lua)]);
        crate::extensions::global_manager()
            .refresh_with(std::slice::from_ref(&root))
            .await;
        std::fs::remove_dir_all(&root).ok();
        let err = compact_history(
            &config,
            &mut messages.clone(),
            &crate::agent::state::GlobalCancellation,
            true,
            false,
            crate::agent::compaction::verbatim::summary_mode(),
            &crate::agent::composable::DexHarness::default(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("history compaction cancelled"), "got: {err}");
        std::fs::remove_dir_all(&root).ok();

        let (compacted, _) = compact_history(
            &config,
            &mut messages,
            &crate::agent::state::GlobalCancellation,
            false,
            false,
            crate::agent::compaction::verbatim::summary_mode(),
            &crate::agent::composable::DexHarness::default(),
        )
        .await
        .unwrap();
        assert!(compacted);
        let summary = messages
            .iter()
            .find(|m| m.name.as_deref() == Some("summary"))
            .unwrap();
        assert_eq!(summary.content.as_deref(), Some("HOOK CHECKPOINT"));
        // Drop the fixture before the lock releases: the process-global
        // manager would otherwise keep the `session.before_compact` hook
        // (its `ev.messages == 42` cancel matches unrelated tests' 42-message
        // histories) loaded for every later compaction in this process.
        crate::extensions::global_manager().reset_for_tests().await;
    }

    /// Emergency compaction must be able to cut below the comfort floor:
    /// after the provider has declared the input over-limit, the normal
    /// 20k keep-window and 8-message minimum would refuse to shrink at all.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env var must stay unset across the compaction awaits
    async fn emergency_compaction_cuts_below_the_comfort_floor() {
        let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _guard = EnvRestore::take(&["DEX_COMPACTION"]);
        std::env::remove_var("DEX_COMPACTION");
        let config = crate::llm::config::tests::test_cfg();
        // A history the normal path would refuse to cut: 12 messages, under
        // the fallback window (1 + 12 kept) it requires.
        let mut messages = vec![msg(Role::System, "sys")];
        messages.push(msg(Role::User, "goal"));
        for i in 0..5 {
            messages.push(msg(Role::User, &format!("u{i}: {}", "x".repeat(80))));
            messages.push(msg(Role::Assistant, &format!("a{i}: {}", "y".repeat(80))));
        }
        let (normal, _) = compact_history(
            &config,
            &mut messages.clone(),
            &crate::agent::state::GlobalCancellation,
            false,
            false,
            crate::agent::compaction::verbatim::summary_mode(),
            &crate::agent::composable::DexHarness::default(),
        )
        .await
        .unwrap();
        assert!(!normal, "normal path declines a short history");
        let (forced, _) = compact_history(
            &config,
            &mut messages,
            &crate::agent::state::GlobalCancellation,
            true,
            false,
            crate::agent::compaction::verbatim::summary_mode(),
            &crate::agent::composable::DexHarness::default(),
        )
        .await
        .unwrap();
        assert!(forced, "emergency path must cut");
        assert!(
            messages.len() < 12,
            "emergency keeps only a small recent window: {}",
            messages.len()
        );
        // Transcript coherence: system first, then the summary.
        assert_eq!(messages[0].role, Role::System);
        assert_eq!(messages[1].name.as_deref(), Some("summary"));
    }

    fn jev_call(id: &str) -> LlmToolCall {
        LlmToolCall {
            id: id.into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "read".into(),
                arguments: "{\"path\":\"src/main.rs\"}".into(),
            },
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env must stay `=jev` across the compaction await
    async fn jev_prunes_tool_heavy_history_without_a_summary() {
        let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = EnvRestore::take(&["DEX_COMPACTION"]);
        std::env::set_var("DEX_COMPACTION", "jev");
        let config = crate::llm::config::tests::test_cfg();
        let mut messages = vec![msg(Role::System, "sys")];
        messages.push(msg(Role::User, "goal: build the thing"));
        for i in 0..10 {
            messages.push(ChatMessage::assistant_calls(
                None,
                vec![jev_call(&format!("c{i}"))],
            ));
            messages.push(ChatMessage::tool_result(
                format!("c{i}"),
                "x".repeat(12_000),
            ));
        }
        for i in 0..KEEP_RECENT_MESSAGES {
            messages.push(msg(Role::User, &format!("recent {i}")));
        }
        let before = messages.len();
        // Mirrors the threshold caller in `loop.rs`: one parse selects
        // both the prune and the fallback summarizer.
        let summarizer = crate::agent::compaction::verbatim::summary_mode();
        let (compacted, usage) = compact_history(
            &config,
            &mut messages,
            &crate::agent::state::GlobalCancellation,
            false,
            summarizer.prunes_jev(),
            summarizer,
            &crate::agent::composable::DexHarness::default(),
        )
        .await
        .unwrap();
        assert!(compacted, "jev must prune a tool-heavy span");
        assert!(usage.is_none(), "pruning spends no summarizer tokens");
        assert!(
            !messages
                .iter()
                .any(|m| m.name.as_deref() == Some("summary")),
            "jev leaves no summary message"
        );
        // User text survives verbatim; stale pairs are gone.
        assert!(messages
            .iter()
            .any(|m| m.content_str() == "goal: build the thing"));
        assert!(messages.len() < before, "pruning must shrink history");
        assert!(
            !messages
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some("c0")),
            "oldest dropped pair must be gone with its call"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env must stay `=jev` across the compaction await
    async fn jev_falls_back_to_deterministic_when_pruning_does_not_pay() {
        let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = EnvRestore::take(&["DEX_COMPACTION"]);
        std::env::set_var("DEX_COMPACTION", "jev");
        let config = crate::llm::config::tests::test_cfg();
        // Tool-light history the normal path still cuts: nothing to prune,
        // so the deterministic summary must cover the span instead.
        let mut messages = vec![msg(Role::System, "sys")];
        messages.push(msg(Role::User, "goal: build the thing"));
        for i in 0..20 {
            messages.push(msg(Role::User, &format!("u{i}: {}", "x".repeat(200))));
            messages.push(msg(Role::Assistant, &format!("a{i}: {}", "y".repeat(200))));
        }
        // Mirrors the threshold caller in `loop.rs`: one parse selects
        // both the prune and the fallback summarizer.
        let summarizer = crate::agent::compaction::verbatim::summary_mode();
        let (compacted, _) = compact_history(
            &config,
            &mut messages,
            &crate::agent::state::GlobalCancellation,
            false,
            summarizer.prunes_jev(),
            summarizer,
            &crate::agent::composable::DexHarness::default(),
        )
        .await
        .unwrap();
        assert!(compacted, "fallback summary must run");
        assert!(
            messages
                .iter()
                .any(|m| m.name.as_deref() == Some("summary")),
            "insufficient prune must fall back to a summary"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env must stay set across the compaction await
    async fn threshold_compaction_summarizes_by_default() {
        // The threshold path follows `DEX_COMPACTION`: unset means the
        // deterministic summary covers a tool-heavy span (no verbatim prune).
        let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = EnvRestore::take(&["DEX_COMPACTION"]);
        std::env::remove_var("DEX_COMPACTION");
        let config = crate::llm::config::tests::test_cfg();
        let mut messages = vec![msg(Role::System, "sys")];
        messages.push(msg(Role::User, "goal: build the thing"));
        for i in 0..10 {
            messages.push(ChatMessage::assistant_calls(
                None,
                vec![jev_call(&format!("c{i}"))],
            ));
            messages.push(ChatMessage::tool_result(
                format!("c{i}"),
                "x".repeat(12_000),
            ));
        }
        for i in 0..KEEP_RECENT_MESSAGES {
            messages.push(msg(Role::User, &format!("recent {i}")));
        }
        // Mirrors the threshold caller in `loop.rs`: one parse selects
        // both the prune and the fallback summarizer.
        let summarizer = crate::agent::compaction::verbatim::summary_mode();
        let (compacted, _) = compact_history(
            &config,
            &mut messages,
            &crate::agent::state::GlobalCancellation,
            false,
            summarizer.prunes_jev(),
            summarizer,
            &crate::agent::composable::DexHarness::default(),
        )
        .await
        .unwrap();
        assert!(compacted, "threshold compaction must still run");
        assert!(
            messages
                .iter()
                .any(|m| m.name.as_deref() == Some("summary")),
            "threshold without DEX_COMPACTION=jev must summarize, not prune"
        );
    }

    /// Live end-to-end: `DEX_COMPACTION=jev` + `TYPESAFE_API_KEY` + a
    /// config `jev:` table sends the span's pairs to the (mock) systemone
    /// endpoint and prunes by the noul verdicts — no summary, no LLM
    /// summarizer usage.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env must stay set across the compaction await
    async fn live_jev_compaction_uses_the_typesafe_api() {
        use std::sync::{Arc, Mutex};

        let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _env = EnvRestore::take(&["DEX_COMPACTION"]);

        // Mock systemone: `read` pairs (even offsets) → drop everything;
        // `bash` pairs (odd) → keep call, drop result. The endpoint's
        // verdicts drive the prune, not the heuristic's tool registry.
        let log: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let app_log = log.clone();
        let handler = move |axum::extract::State(log): axum::extract::State<
            Arc<Mutex<Vec<serde_json::Value>>>,
        >,
                            body: String| async move {
            let request: serde_json::Value = serde_json::from_str(&body).unwrap();
            log.lock().unwrap().push(request.clone());
            let mut answers = serde_json::Map::new();
            for id in request["questions"].as_object().unwrap().keys() {
                // Even pairs (read): noul 0 → drop both halves. Odd pairs
                // (bash): keep the call (1.0), drop the result (0.0).
                let even_pair = ["p0_", "p2_", "p4_"].iter().any(|p| id.starts_with(p));
                let noul = if even_pair || id.ends_with("_result_needed_verbatim") {
                    0.0
                } else {
                    1.0
                };
                answers.insert(
                    id.clone(),
                    serde_json::json!({"type": "noul", "noul": noul}),
                );
            }
            axum::Json(serde_json::json!({"model": "jev-test", "answers": answers}))
        };
        let app = axum::Router::new()
            .route("/v1/systemone", axum::routing::post(handler))
            .with_state(app_log);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let endpoint = format!("http://{addr}/v1/systemone");

        // Config file with the `jev:` table + key env: the opt-in gate.
        let dir = std::env::temp_dir().join("dex-jev-e2e-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("config.yaml");
        std::fs::write(&cfg_path, format!("jev:\n  url: {endpoint}\n")).unwrap();
        crate::llm::config::invalidate_config_cache();
        let _cfg_env = EnvRestore::take(&["DEX_CONFIG"]);
        std::env::set_var("DEX_CONFIG", &cfg_path);
        let _key_env = EnvRestore::take(&["TYPESAFE_API_KEY"]);
        std::env::set_var("TYPESAFE_API_KEY", "sk-test");

        std::env::set_var("DEX_COMPACTION", "jev");
        let config = crate::llm::config::tests::test_cfg();
        let mut messages = vec![msg(Role::System, "sys")];
        messages.push(msg(Role::User, "goal: build the thing"));
        for i in 0..6 {
            let tool = if i % 2 == 0 { "read" } else { "bash" };
            messages.push(ChatMessage::assistant_calls(
                None,
                vec![LlmToolCall {
                    id: format!("c{i}"),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: tool.into(),
                        arguments: format!("{{\"path\":\"f{i}.rs\"}}"),
                    },
                }],
            ));
            messages.push(ChatMessage::tool_result(
                format!("c{i}"),
                "x".repeat(12_000),
            ));
        }
        for i in 0..KEEP_RECENT_MESSAGES {
            messages.push(msg(Role::User, &format!("recent {i}")));
        }
        let summarizer = crate::agent::compaction::verbatim::summary_mode();
        let (compacted, usage) = compact_history(
            &config,
            &mut messages,
            &crate::agent::state::GlobalCancellation,
            false,
            summarizer.prunes_jev(),
            summarizer,
            &crate::agent::composable::DexHarness::default(),
        )
        .await
        .unwrap();
        assert!(compacted, "live jev must prune a tool-heavy span");
        assert!(usage.is_none(), "pruning spends no summarizer tokens");
        assert!(!messages
            .iter()
            .any(|m| m.name.as_deref() == Some("summary")));

        // The endpoint saw the span's pairs: 6 result questions per request.
        let log = log.lock().unwrap();
        assert!(!log.is_empty(), "the mock endpoint must have been called");
        let questions = log[0]["questions"].as_object().unwrap();
        assert!(questions
            .keys()
            .any(|k| k.ends_with("_result_needed_verbatim")));
        assert_eq!(
            log[0]["model"],
            crate::agent::compaction::verbatim::JEV_MODEL
        );
        drop(log);

        // Endpoint verdicts applied: even (read) pairs dropped entirely,
        // odd (bash) results truncated, recent users untouched.
        assert!(!messages.iter().any(|m| m
            .tool_call_id
            .as_deref()
            .is_some_and(|id| id == "c0" || id == "c2")));
        assert!(messages
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("c1")
                && m.content_str().contains("jev: truncated")));
        assert!(messages
            .iter()
            .any(|m| m.content_str() == "goal: build the thing"));

        crate::llm::config::invalidate_config_cache();
    }
}
