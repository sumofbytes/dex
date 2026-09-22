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
//! new dependency and hermetic in tests. With a Typesafe key present (and
//! opt-in — see [`live_credentials`]), the real Typesafe Jev scorer takes
//! over: the span's pairs go to `POST <base_url>/systemone` as state and
//! two `noul` questions per pair ("call still matters?", "result still
//! needed verbatim?"), batched under [`LIVE_STATE_CHAR_LIMIT`] chars per
//! request. `TYPESAFE_JEV_URL` (or `jev.url:`) overrides the endpoint
//! (tests, gateways). A live failure (HTTP error, bad body) degrades to the
//! heuristic scorer for that prune with a `warn_once` — never to a dropped
//! compaction.
//!
//! The caller passes the exact archivable span (from `find_cut_point`, which
//! already excludes the keep-recent window): there is no internal
//! recency pin.
//!
//! Stack order with the other output shrinkers: this prune runs last at
//! compaction time over the intact session history. A Jev drop is
//! destructive, contained by only ever dropping re-runnable reads (re-run
//! the tool to restore): errors and mutating-tool evidence truncate instead
//! of dropping.

use std::collections::{HashMap, HashSet};

use serde_json::json;

use crate::protocol::{ChatMessage, Role};
use crate::runtime::notice::warn_once;

/// Value of [`COMPACTION_ENV`] selecting verbatim pruning instead of
/// summarization.
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

/// One knob shape shared by [`COMPACTION_ENV`]: `jev`
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

/// `TYPESAFE_JEV_URL` overrides the evaluation endpoint (tests and
/// self-hosted gateways).
pub(crate) const JEV_URL_ENV: &str = "TYPESAFE_JEV_URL";

/// Default endpoint from the TypeSafe docs (`POST /v1/systemone`),
/// matching `providers.typesafe.base_url: https://api.typesafe.ai/v1` +
/// [`JEV_SYSTEMONE_PATH`].
pub(crate) const JEV_DEFAULT_URL: &str = "https://api.typesafe.ai/v1/systemone";

/// Wire model alias from the TypeSafe docs.
pub(crate) const JEV_MODEL: &str = "jev-latest";

/// Path appended to the `providers.typesafe.base_url` entry: Typesafe is a
/// normal provider whose only difference is the evaluation API contract.
const JEV_SYSTEMONE_PATH: &str = "/systemone";

/// Provider key whose `providers:` entry deposits the Jev scorer
/// credentials — the same `providers.<name>.api_key` / `base_url` shape as
/// every other provider.
pub(crate) const JEV_PROVIDER: &str = "typesafe";

/// Resolve the live scorer credentials (`endpoint`, `key`) when the real
/// Jev asker is enabled. The key comes from `providers.typesafe.api_key`
/// (the standard provider deposit) or `TYPESAFE_API_KEY`; the endpoint
/// from `TYPESAFE_JEV_URL` > `jev.url:` > `providers.typesafe.base_url` +
/// `/systemone` > [`JEV_DEFAULT_URL`]. Opt-in is explicit config: a `jev:`
/// table or a `providers.typesafe` entry — without either, the daemon
/// never touches the network from compaction. `jev.enabled: false` is the
/// kill switch even when a provider entry exists.
pub(crate) fn live_credentials() -> Option<(String, String)> {
    let file = crate::llm::config::config_file_value();
    let table = file.as_ref().and_then(|f| f.get("jev"));
    if table
        .and_then(|t| t.get("enabled"))
        .and_then(|v| v.as_bool())
        == Some(false)
    {
        return None;
    }
    let entry = crate::llm::config::load_provider_entries(&file).remove(JEV_PROVIDER);
    let opted_in = table.is_some()
        || entry
            .as_ref()
            .is_some_and(|e| e.api_key.is_some() || e.base_url.is_some());
    if !opted_in {
        return None;
    }
    let key = entry
        .as_ref()
        .and_then(|e| e.api_key.clone())
        .filter(|k| !k.trim().is_empty())
        .or_else(|| {
            std::env::var(JEV_KEY_ENV)
                .ok()
                .filter(|k| !k.trim().is_empty())
        })?;
    let url = std::env::var(JEV_URL_ENV)
        .ok()
        .filter(|u| !u.trim().is_empty())
        .or_else(|| {
            table
                .and_then(|t| t.get("url"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .or_else(|| {
            entry.as_ref().and_then(|e| e.base_url.clone()).map(|base| {
                let base = base.trim_end_matches('/');
                if base.ends_with(JEV_SYSTEMONE_PATH) {
                    base.to_string()
                } else {
                    format!("{base}{JEV_SYSTEMONE_PATH}")
                }
            })
        })
        .unwrap_or_else(|| JEV_DEFAULT_URL.to_string());
    Some((url, key))
}

/// `TYPESAFE_API_KEY`: env deposit for the Typesafe key when it is not in
/// `providers.typesafe.api_key`.
pub(crate) const JEV_KEY_ENV: &str = "TYPESAFE_API_KEY";

/// Error from [`noul_answers`]: the endpoint answered non-2xx with its
/// detail body, or the body could not be parsed.
#[derive(Debug)]
pub(crate) enum NoulError {
    Http { status: u16, body: String },
    Decode(String),
}

impl std::fmt::Display for NoulError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoulError::Http { status, body } => {
                let detail: String = body.chars().take(200).collect();
                write!(f, "HTTP {status}: {detail}")
            }
            NoulError::Decode(e) => write!(f, "decode: {e}"),
        }
    }
}

/// One answer from a noul question, keyed by question id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct NoulAnswer {
    pub(crate) value: f64,
}

/// Evaluate the `questions` map against the TypeSafe systemone endpoint
/// and return each noul value. Questions are evaluated in parallel and in
/// isolation per the docs, so one request serves every pair in the span.
pub(crate) async fn noul_answers(
    endpoint: &str,
    api_key: &str,
    state: serde_json::Value,
    questions: serde_json::Map<String, serde_json::Value>,
) -> Result<HashMap<String, NoulAnswer>, NoulError> {
    if questions.is_empty() {
        return Ok(HashMap::new());
    }
    let body = json!({
        "state": state,
        "model": JEV_MODEL,
        "questions": questions,
    });
    let client = crate::runtime::http::shared_async_client();
    let resp = client
        .post(endpoint)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| NoulError::Decode(format!("{e}")))?;
    let status = resp.status().as_u16();
    let text = resp
        .text()
        .await
        .map_err(|e| NoulError::Decode(format!("{e}")))?;
    if !(200..300).contains(&status) {
        return Err(NoulError::Http { status, body: text });
    }
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| NoulError::Decode(format!("{e}")))?;
    let answers = value
        .get("answers")
        .and_then(|a| a.as_object())
        .ok_or_else(|| NoulError::Decode("response missing `answers` map".into()))?;
    let mut out = HashMap::with_capacity(answers.len());
    for (id, answer) in answers {
        let Some(noul) = answer.get("noul").and_then(|v| v.as_f64()) else {
            return Err(NoulError::Decode(format!("answer `{id}` missing noul")));
        };
        out.insert(id.clone(), NoulAnswer { value: noul });
    }
    Ok(out)
}

/// One in-span tool_use→tool_result pair handed to a scorer. `call_arguments`
/// is the raw JSON arguments string; `result` the verbatim tool output.
#[derive(Clone, Debug)]
pub(crate) struct Pair {
    pub(crate) tool: String,
    pub(crate) call_arguments: String,
    pub(crate) result: String,
}

/// One pair's scorer verdict: which halves survive the prune.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JevDecision {
    pub(crate) keep_call: bool,
    pub(crate) keep_result: bool,
}

/// Noul threshold the doc's routing pattern uses (yes above 0.8).
pub(crate) const NOUL_YES_THRESHOLD: f64 = 0.8;

/// How [`prune_span`] decides each pair's fate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scorer {
    /// Deterministic size/re-runnability heuristic (default, offline).
    Heuristic,
    /// Real Jev: two noul questions per pair over the TypeSafe API.
    Jev,
}

impl Scorer {
    /// Asker for this scorer. The live scorer is async and fallible; the
    /// heuristic never fails.
    pub(crate) fn is_live(self) -> bool {
        self == Scorer::Jev
    }
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
/// pure reads (`read`, `ls`, the fff tools) drop when old and huge,
/// while mutating / delegating / external tools (`bash`, `write`, `edit`,
/// `mcp__*`, extensions) only ever truncate, so file-op evidence
/// and child answers survive. Unknown tools have no registry row and stay
/// conservative: truncate, never drop.
fn is_rerunnable(tool: &str) -> bool {
    crate::tools::metadata(tool).is_some_and(|m| m.read_only && m.idempotent)
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

/// Context object threaded through a live prune: endpoint + key.
#[derive(Clone)]
pub(crate) struct LiveScorer<'a> {
    pub(crate) endpoint: &'a str,
    pub(crate) api_key: &'a str,
}

impl LiveScorer<'_> {
    /// Score every pair in the span with batched systemone calls: the
    /// state is the span's tool_use→tool_result pairs, one noul pair per
    /// pair keyed `p{offset}_call_still_matters` / `p{offset}_result_needed_verbatim`.
    /// Each request stays within [`LIVE_STATE_CHAR_LIMIT`] chars of result
    /// content and [`LIVE_MAX_BATCH`] pairs (the doc's batching guidance);
    /// oversized spans are scored in consecutive requests.
    pub(crate) async fn score_span(&self, pairs: &[Pair]) -> Result<Vec<JevDecision>, NoulError> {
        let mut decisions = vec![
            JevDecision {
                keep_call: true,
                keep_result: true,
            };
            pairs.len()
        ];
        let mut offset = 0;
        while offset < pairs.len() {
            // Fit as many pairs as the pair-count and char budgets allow.
            let mut batch_end = offset;
            let mut batch_chars = 0usize;
            while batch_end < pairs.len()
                && batch_end - offset < LIVE_MAX_BATCH
                && (batch_end == offset || batch_chars < LIVE_STATE_CHAR_LIMIT)
            {
                batch_chars += pairs[batch_end].result.chars().count();
                batch_end += 1;
            }
            let batch = &pairs[offset..batch_end];
            let mut state_rows = Vec::with_capacity(batch.len());
            let mut questions = serde_json::Map::new();
            for (idx, pair) in batch.iter().enumerate() {
                let id = format!("p{idx}");
                state_rows.push(json!({
                    "id": id,
                    "tool_name": pair.tool,
                    "tool_call": pair.call_arguments,
                    "result": pair.result,
                }));
                questions.insert(
                    format!("{id}_call_still_matters"),
                    json!({
                        "type": "noul",
                        "instructions": {
                            "rows": "`rows`",
                            "question": "For the tool_use→tool_result pair in `rows` \
                                         whose `id` matches the prefix of this question \
                                         key, is the tool CALL still relevant to keep in \
                                         the conversation history?",
                        },
                        "criteria": {
                            "true": "The call record still carries information the \
                                     conversation needs (what was looked at, what \
                                     changed)",
                            "false": "The call is a stale read whose purpose is \
                                      superseded; only the result might matter",
                        },
                    }),
                );
                questions.insert(
                    format!("{id}_result_needed_verbatim"),
                    json!({
                        "type": "noul",
                        "instructions": {
                            "rows": "`rows`",
                            "question": "For the tool_use→tool_result pair in `rows` \
                                         whose `id` matches the prefix of this question \
                                         key, does the RESULT still need to be available \
                                         verbatim, or can it be replaced by a short \
                                         truncation note?",
                        },
                        "criteria": {
                            "true": "The verbatim result content still matters (errors, \
                                     evidence, numbers later referred to, mutating-tool \
                                     output)",
                            "false": "The result is a bulky reproducible read; a short \
                                      note suffices",
                        },
                    }),
                );
            }
            let state = json!({ "rows": state_rows });
            let answers = noul_answers(self.endpoint, self.api_key, state, questions).await?;
            for (idx, _) in batch.iter().enumerate() {
                if let Some(a) = answers.get(&format!("p{idx}_call_still_matters")) {
                    decisions[offset + idx].keep_call = a.value >= NOUL_YES_THRESHOLD;
                }
                if let Some(a) = answers.get(&format!("p{idx}_result_needed_verbatim")) {
                    decisions[offset + idx].keep_result = a.value >= NOUL_YES_THRESHOLD;
                }
            }
            offset = batch_end;
        }
        Ok(decisions)
    }
}

/// Max pairs scored per systemone request — keeps each state payload
/// bounded (the doc's batching guidance).
const LIVE_MAX_BATCH: usize = 16;

/// Soft cap on characters of tool-result content one batched request
/// carries: batches stop early when the accumulated result chars would
/// exceed this, splitting oversized spans into more requests instead of
/// one huge payload.
const LIVE_STATE_CHAR_LIMIT: usize = 120_000;

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
///
/// `scorer` selects the asker: [`Scorer::Heuristic`] scores offline from
/// size/re-runnability/error evidence; [`Scorer::Jev`] sends the span's
/// pairs to the TypeSafe systemone endpoint (two noul questions per pair,
/// batched) and falls back to the heuristic when the API fails or is not
/// configured (`live` is `None`). A live failure degrades to the heuristic
/// with a `warn_once`, never to a dropped prune.
pub(crate) async fn prune_span(
    messages: &mut Vec<ChatMessage>,
    start: usize,
    end: usize,
    scorer: Scorer,
    live: Option<&LiveScorer<'_>>,
) -> JevStats {
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

    // Owner lookup: call id → (assistant index, tool name + arguments).
    // Scanned from the transcript head so results whose call sits before
    // the span still resolve (they truncate, never drop — no orphans).
    let mut owners: HashMap<String, (usize, String, String)> = HashMap::new();
    for (idx, msg) in messages.iter().enumerate().take(end) {
        if msg.role != Role::Assistant {
            continue;
        }
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                owners.entry(call.id.clone()).or_insert((
                    idx,
                    call.function.name.clone(),
                    call.function.arguments.clone(),
                ));
            }
        }
    }

    // Collect the span's pairs once — shared by both scorers and by the
    // apply step.
    let mut pair_msgs: Vec<(usize, String)> = Vec::new(); // (msg idx, call id)
    let mut pairs: Vec<Pair> = Vec::new();
    let mut owned_inside: Vec<bool> = Vec::new();
    for (idx, msg) in messages.iter().enumerate().take(end).skip(start) {
        if msg.role != Role::Tool {
            continue;
        }
        let call_id = msg.tool_call_id.clone().unwrap_or_default();
        if call_id.is_empty() {
            continue;
        }
        let Some((owner_idx, tool, args)) = owners.get(&call_id).cloned() else {
            continue;
        };
        pair_msgs.push((idx, call_id));
        pairs.push(Pair {
            tool,
            call_arguments: args,
            result: msg.content.clone().unwrap_or_default(),
        });
        owned_inside.push(owner_idx >= start && owner_idx < end);
    }

    // Score. The live asker is a pure decision upgrade: any failure falls
    // back to the heuristic rather than aborting the prune.
    let mut decisions: Vec<JevDecision> = pairs
        .iter()
        .map(|p| {
            let (keep_call, keep_result) = decide(&p.tool, &p.result);
            JevDecision {
                keep_call,
                keep_result,
            }
        })
        .collect();
    if scorer.is_live() {
        if let Some(live) = live {
            match live.score_span(&pairs).await {
                Ok(live_decisions) => decisions = live_decisions,
                Err(e) => warn_once(
                    "jev:live-error",
                    &format!("jev live scoring failed ({e}) — using heuristic fallback"),
                ),
            }
        }
    }

    let mut drop_ids: HashSet<String> = HashSet::new();
    let mut truncate: HashMap<usize, (String, usize, String)> = HashMap::new();
    let mut kept = 0usize;
    let mut dropped = 0usize;
    let mut truncated = 0usize;

    for (i, ((idx, call_id), pair)) in pair_msgs.iter().zip(&pairs).enumerate() {
        let decision = decisions[i];
        // A call owned outside the span stays — only its in-span result may
        // be truncated, never dropped (no orphaned results).
        if decision.keep_result {
            kept += 1;
        } else if decision.keep_call || !owned_inside[i] {
            let head: String = pair.result.chars().take(JEV_TRUNCATE_HEAD_CHARS).collect();
            truncate.insert(*idx, (head, pair.result.len(), pair.tool.clone()));
            truncated += 1;
        } else {
            drop_ids.insert(call_id.clone());
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

    #[tokio::test]
    async fn drops_old_large_rerunnable_and_truncates_medium() {
        let mut msgs = history();
        let stats = prune_span(&mut msgs, 1, 5, Scorer::Heuristic, None).await;
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

    #[tokio::test]
    async fn large_bash_output_truncates_but_never_drops() {
        // `bash` is mutating: no re-run restores a side effect, so even a
        // huge result only truncates — the call and its evidence survive.
        let mut msgs = vec![ChatMessage::system("sys")];
        msgs.push(ChatMessage::assistant_calls(None, vec![call("b1", "bash")]));
        msgs.push(ChatMessage::tool_result("b1", "z".repeat(50_000)));
        msgs.push(ChatMessage::user("tail"));
        let stats = prune_span(&mut msgs, 1, 3, Scorer::Heuristic, None).await;
        assert_eq!(stats.dropped, 0, "{stats:?}");
        assert_eq!(stats.truncated, 1, "{stats:?}");
        let kept = msgs
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("b1"))
            .expect("bash result stays");
        assert!(kept.content_str().contains("jev: truncated"));
    }

    #[tokio::test]
    async fn error_evidence_is_never_dropped() {
        // `bash` is mutating and its error output is the evidence: even a
        // huge result only truncates, never drops.
        let mut msgs = vec![ChatMessage::system("sys")];
        msgs.push(ChatMessage::assistant_calls(None, vec![call("e1", "bash")]));
        msgs.push(ChatMessage::tool_result(
            "e1",
            format!("ERROR boom {}", "e".repeat(11_000)),
        ));
        msgs.push(ChatMessage::user("tail"));
        let stats = prune_span(&mut msgs, 1, 3, Scorer::Heuristic, None).await;
        assert_eq!(stats.dropped, 0, "{stats:?}");
        assert!(msgs.iter().any(|m| m.tool_call_id.as_deref() == Some("e1")));
    }

    #[tokio::test]
    async fn cross_span_owner_truncates_without_orphaning() {
        let mut msgs = vec![ChatMessage::system("sys")];
        msgs.push(ChatMessage::assistant_calls(None, vec![call("c1", "read")]));
        msgs.push(ChatMessage::tool_result("c1", "x".repeat(12_000)));
        // Prune only the result; the owning call sits before the span.
        let stats = prune_span(&mut msgs, 2, 3, Scorer::Heuristic, None).await;
        assert_eq!(stats.dropped, 0, "{stats:?}");
        assert_eq!(stats.truncated, 1, "{stats:?}");
        // The call survives, so the (truncated) result is not orphaned.
        assert!(msgs.iter().any(|m| m
            .tool_calls
            .as_deref()
            .is_some_and(|cs| cs.iter().any(|c| c.id == "c1"))));
    }

    #[tokio::test]
    async fn tiny_span_is_not_worthwhile() {
        let mut msgs = vec![ChatMessage::system("sys"), ChatMessage::user("hi")];
        let stats = prune_span(&mut msgs, 1, 2, Scorer::Heuristic, None).await;
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

    // --- live scorer ---

    /// Mock TypeSafe systemone endpoint: POST /v1/systemone records the
    /// request body and answers each question id via `reply(request)` —
    /// the full response JSON. Returns `(base_url, request_log)`.
    async fn spawn_mock(
        reply: impl Fn(&serde_json::Value) -> serde_json::Value + Send + Sync + 'static,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    ) {
        use std::sync::{Arc, Mutex};
        let log: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        type Reply = Arc<dyn Fn(&serde_json::Value) -> serde_json::Value + Send + Sync>;
        let state = (log.clone(), Arc::new(reply) as Reply);
        let handler = move |axum::extract::State(state): axum::extract::State<(
            Arc<Mutex<Vec<serde_json::Value>>>,
            Reply,
        )>,
                            body: String| async move {
            let request: serde_json::Value = serde_json::from_str(&body).unwrap();
            state.0.lock().unwrap().push(request.clone());
            axum::Json((state.1)(&request))
        };
        let app = axum::Router::new()
            .route("/v1/systemone", axum::routing::post(handler))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/v1/systemone"), log)
    }

    /// Reply builder: answers every question id with a noul value chosen
    /// per id (`f`), defaulting to 1.0 (keep) when `f` returns None.
    fn answer_all(
        f: impl Fn(&str) -> Option<f64> + Send + Sync + 'static,
    ) -> impl Fn(&serde_json::Value) -> serde_json::Value + Send + Sync + 'static {
        move |request| {
            let mut answers = serde_json::Map::new();
            for id in request["questions"].as_object().unwrap().keys() {
                let value = f(id).unwrap_or(1.0);
                answers.insert(id.clone(), json!({"type": "noul", "noul": value}));
            }
            json!({ "model": "jev-test", "answers": answers })
        }
    }

    #[tokio::test]
    async fn live_scorer_scores_pairs_and_batches_large_spans() {
        let (url, log) = spawn_mock(answer_all(|id| {
            if id.ends_with("_result_needed_verbatim") {
                Some(0.1)
            } else {
                None
            }
        }))
        .await;
        let scorer = LiveScorer {
            endpoint: &url,
            api_key: "k",
        };
        // 20 pairs → two batches (LIVE_MAX_BATCH = 16).
        let pairs: Vec<Pair> = (0..20)
            .map(|i| Pair {
                tool: "read".into(),
                call_arguments: format!("{{\"path\":\"f{i}.rs\"}}"),
                result: "x".repeat(500),
            })
            .collect();
        let decisions = scorer.score_span(&pairs).await.unwrap();
        assert_eq!(decisions.len(), 20);
        let log = log.lock().unwrap();
        assert_eq!(
            log.len(),
            2,
            "20 pairs split across {LIVE_MAX_BATCH}-pair batches"
        );
        // Every result question said no; every call question defaulted to yes.
        assert!(decisions.iter().all(|d| !d.keep_result && d.keep_call));
        // State rows carry the pair fields; questions are typed noul.
        let first = &log[0];
        let rows = first["state"]["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 16);
        assert_eq!(rows[0]["tool_name"], "read");
        assert_eq!(
            first["questions"]["p0_result_needed_verbatim"]["type"],
            "noul"
        );
        assert_eq!(first["model"], JEV_MODEL);
    }

    #[tokio::test]
    async fn live_error_degrades_to_heuristic_in_prune_span() {
        // Endpoint 500s: `prune_span` must fall back to the heuristic and
        // still produce the heuristic outcome.
        let (url, _log) =
            spawn_mock(|_| json!({"status": 500u16, "body": {"error": "overloaded"}})).await;
        let mut msgs = vec![ChatMessage::system("sys")];
        msgs.push(ChatMessage::assistant_calls(None, vec![call("c1", "read")]));
        msgs.push(ChatMessage::tool_result("c1", "x".repeat(12_000)));
        msgs.push(ChatMessage::user("tail"));
        let live = LiveScorer {
            endpoint: &url,
            api_key: "k",
        };
        let stats = prune_span(&mut msgs, 1, 3, Scorer::Jev, Some(&live)).await;
        // Heuristic fallback: huge re-runnable read drops.
        assert_eq!(stats.dropped, 1, "{stats:?}");
        assert!(!msgs.iter().any(|m| m.tool_call_id.as_deref() == Some("c1")));
    }

    #[tokio::test]
    async fn live_scorer_drops_and_truncates_by_noul_answers() {
        let (url, _log) = spawn_mock(answer_all(|id| {
            // `read` pair: drop both halves; `bash` pair: keep the call,
            // truncate the result — the API decides per pair, not per tool.
            if id.starts_with("p0_") {
                Some(0.05)
            } else if id == "p1_result_needed_verbatim" {
                Some(0.2)
            } else {
                Some(0.95)
            }
        }))
        .await;
        let mut msgs = vec![ChatMessage::system("sys")];
        msgs.push(ChatMessage::assistant_calls(None, vec![call("c1", "read")]));
        msgs.push(ChatMessage::tool_result("c1", "x".repeat(12_000)));
        msgs.push(ChatMessage::assistant_calls(None, vec![call("c2", "bash")]));
        msgs.push(ChatMessage::tool_result("c2", "y".repeat(5_000)));
        msgs.push(ChatMessage::user("tail"));
        let live = LiveScorer {
            endpoint: &url,
            api_key: "k",
        };
        let stats = prune_span(&mut msgs, 1, 5, Scorer::Jev, Some(&live)).await;
        assert_eq!(stats.dropped, 1, "{stats:?}");
        assert_eq!(stats.truncated, 1, "{stats:?}");
        // Dropped pair is gone entirely; bash result survives truncated
        // with its call — the mutating-tool safety net still applies to
        // cross-span owners, but the scorer's verdicts apply within.
        assert!(!msgs.iter().any(|m| m.tool_call_id.as_deref() == Some("c1")));
        let c2 = msgs
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("c2"))
            .expect("truncated result stays");
        assert!(c2.content_str().contains("jev: truncated"));
    }

    #[tokio::test]
    async fn live_credentials_require_key_and_config_opt_in() {
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Hermetic env + config file: a plain config with no `jev:` table
        // and no `providers.typesafe` entry means no opt-in regardless of
        // the key.
        let dir = std::env::temp_dir().join("dex-jev-creds-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.yaml");
        std::fs::write(&cfg, "model: zen/m\n").unwrap();
        crate::llm::config::invalidate_config_cache();
        // Guard entries record the PREVIOUS value, restored on drop — never
        // the test's own value (that would leak `DEX_CONFIG` into later
        // tests). The vars are then set for the test body below.
        let _guard = crate::session::EnvGuard(vec![
            (JEV_KEY_ENV, std::env::var_os(JEV_KEY_ENV)),
            (JEV_URL_ENV, std::env::var_os(JEV_URL_ENV)),
            ("DEX_CONFIG", std::env::var_os("DEX_CONFIG")),
        ]);
        std::env::remove_var(JEV_URL_ENV);
        std::env::set_var("DEX_CONFIG", &cfg);
        // No key, no file → off.
        std::env::remove_var(JEV_KEY_ENV);
        assert_eq!(live_credentials(), None);
        // Key but no config opt-in (no `jev:` table, no
        // `providers.typesafe`) → off: no surprise network from compaction
        // without opt-in.
        std::env::set_var(JEV_KEY_ENV, "sk-test");
        assert_eq!(live_credentials(), None);
        // A `providers.typesafe` entry with a key opts in and supplies the
        // credential; base_url gets the `/systemone` path appended.
        std::fs::write(
            &cfg,
            "model: zen/m\nproviders:\n  typesafe:\n    api_key: sk-file\n    base_url: https://gw.internal/v1\n",
        )
        .unwrap();
        crate::llm::config::invalidate_config_cache();
        std::env::remove_var(JEV_KEY_ENV);
        assert_eq!(
            live_credentials(),
            Some(("https://gw.internal/v1/systemone".into(), "sk-file".into()))
        );
        // `jev.enabled: false` is the kill switch even with a provider
        // entry.
        std::fs::write(
            &cfg,
            "providers:\n  typesafe:\n    api_key: sk-file\njev:\n  enabled: false\n",
        )
        .unwrap();
        crate::llm::config::invalidate_config_cache();
        assert_eq!(live_credentials(), None);
        // `TYPESAFE_API_KEY` still works as the key deposit when the file
        // has a `jev:` table (env key > file key), and `TYPESAFE_JEV_URL`
        // > `jev.url:` > provider base_url for the endpoint.
        std::fs::write(&cfg, "jev:\n  url: https://cfg/j1\n").unwrap();
        crate::llm::config::invalidate_config_cache();
        std::env::set_var(JEV_KEY_ENV, "sk-env");
        assert_eq!(
            live_credentials(),
            Some(("https://cfg/j1".into(), "sk-env".into()))
        );
        std::env::remove_var(JEV_URL_ENV);
        crate::llm::config::invalidate_config_cache();
    }
}
