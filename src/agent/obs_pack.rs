//! Observation pack: keep large tool results reachable without replaying them.
//!
//! A large tool result is sent in full for its first few provider requests,
//! then replaced with a short, stable placeholder for every later request.
//! The original bytes are archived by observation id outside the provider
//! context, and the agent pulls exact pages back with the registered
//! `obs_recall` tool.
//!
//! The mechanism never edits history in place. `project` builds the
//! provider-bound message list from the intact session history on every
//! request, so the stored session stays untouched and recall keeps working
//! across compaction and resume.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Only tool results larger than this participate.
pub(crate) const THRESHOLD_BYTES: usize = 10 * 1024;
/// Provider requests that still carry the full payload before the
/// placeholder takes over.
pub(crate) const FULL_SENDS: usize = 2;
/// Placeholder excerpt budget, split evenly between head and tail, whole
/// lines only.
pub(crate) const PLACEHOLDER_EXCERPT_BYTES: usize = 1024;
/// Recall page caps (modeled on `dex`'s own bash tool output budget).
pub(crate) const RECALL_MAX_BYTES: usize = 32 * 1024;
pub(crate) const RECALL_MAX_LINES: usize = 400;
/// Feature gate, same pattern as `DEX_EXTRA_TOOLS` / online compaction:
/// the tool costs prompt tokens on every request, so it is opt-in.
pub(crate) const OBSERVATION_PACK_ENV: &str = "DEX_OBSERVATION_PACK";

pub(crate) fn observation_pack_enabled() -> bool {
    std::env::var(OBSERVATION_PACK_ENV).as_deref() == Ok("1")
}

/// `obs_<32 hex>` — validated before any path is built from it, so an id is
/// never a path traversal vector.
pub(crate) fn is_observation_id(id: &str) -> bool {
    id.len() == 36
        && id.starts_with("obs_")
        && id[4..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// sha256 hex digest, first 32 chars.
fn hash_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    digest[..16].iter().map(|b| format!("{b:02x}")).collect()
}

/// Token estimate for a single payload (~4 chars per token, same heuristic
/// as `agent/tokens.rs` but per-payload; named differently to keep the two
/// estimators distinct).
fn payload_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Archive directory: sibling of the session JSONL, shared by a session's
/// whole run (resume reuses the same file, so the same directory).
fn observations_dir(session_path: &Path) -> PathBuf {
    session_path.parent().unwrap_or(Path::new(".")).join("obs")
}

fn observation_path(session_path: &Path, id: &str) -> PathBuf {
    observations_dir(session_path).join(format!("{id}.txt"))
}

pub(crate) struct Observation {
    pub(crate) id: String,
    pub(crate) tool_name: String,
    pub(crate) text: String,
    pub(crate) bytes: usize,
    pub(crate) lines: usize,
    pub(crate) tokens: usize,
}

/// Build an observation from a tool result, or `None` when the result is too
/// small to be worth packing.
pub(crate) fn create_observation(tool_name: &str, text: &str) -> Option<Observation> {
    let bytes = text.len();
    if bytes <= THRESHOLD_BYTES {
        return None;
    }
    // Content-addressed inside one session: the id binds the tool name to
    // the payload hash, so identical re-runs dedup and any byte drift gets a
    // fresh object.
    let id = format!(
        "obs_{}",
        hash_hex(format!("{tool_name}\0{text}").as_bytes())
    );
    Some(Observation {
        id,
        tool_name: tool_name.to_string(),
        text: text.to_string(),
        bytes,
        lines: text.lines().count(),
        tokens: payload_tokens(text),
    })
}

/// Write the payload to its content-addressed path, refusing symlinked
/// directories and verifying an existing object byte-for-byte before
/// reusing it. `create_new` + hash check: a matching size and hash proves
/// the existing file is this exact payload.
pub(crate) fn ensure_stored(session_path: &Path, observation: &Observation) -> std::io::Result<()> {
    let dir = observations_dir(session_path);
    fs::create_dir_all(&dir)?;
    // `symlink_metadata` does not follow a final symlink: an `obs` directory
    // that is itself a symlink is refused before any file lands inside it.
    #[cfg(unix)]
    if fs::symlink_metadata(&dir)?.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "observation directory is a symlink",
        ));
    }
    let path = observation_path(session_path, &observation.id);
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut handle) => handle.write_all(observation.text.as_bytes()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = fs::read(&path)?;
            if existing.len() != observation.bytes
                || hash_hex(&existing) != hash_hex(observation.text.as_bytes())
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "content-addressed observation mismatch for {}",
                        observation.id
                    ),
                ));
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

/// Head+tail excerpt, whole lines only, within a byte budget.
fn complete_line_excerpt(text: &str, budget: usize, from_end: bool) -> String {
    let lines: Vec<&str> = if from_end {
        text.lines().rev().collect()
    } else {
        text.lines().collect()
    };
    let mut selected: Vec<&str> = Vec::new();
    let mut selected_bytes = 0;
    for line in lines {
        let line_bytes = line.len() + 1; // newline
        if selected_bytes + line_bytes > budget {
            break;
        }
        selected_bytes += line_bytes;
        selected.push(line);
    }
    if from_end {
        selected.reverse();
    }
    let mut out = selected.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Character-bounded fallback excerpt for payloads without line breaks
/// (minified JSON, base64): whole lines would produce an empty excerpt, so
/// take a raw byte slice trimmed to a UTF-8 boundary.
fn raw_excerpt(text: &str, budget: usize) -> String {
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// The stable placeholder that replaces the payload in the projection.
pub(crate) fn placeholder_for(observation: &Observation) -> String {
    let head_budget = PLACEHOLDER_EXCERPT_BYTES / 2;
    let tail_budget = PLACEHOLDER_EXCERPT_BYTES - head_budget;
    let mut head = complete_line_excerpt(&observation.text, head_budget, false);
    let mut tail = complete_line_excerpt(&observation.text, tail_budget, true);
    // Minified / single-line payloads: line excerpts are empty, fall back to
    // a raw character slice so the model can still see what kind of content
    // this is and judge whether recalling is worth it.
    if head.is_empty() {
        head = raw_excerpt(&observation.text, head_budget);
    }
    if tail.is_empty() {
        tail = raw_excerpt(&observation.text, tail_budget);
    }
    format!(
        "[large tool result replaced after its first {FULL_SENDS} provider requests]\n\
         id: {id}\n\
         tool: {tool}\n\
         original_bytes: {bytes}\n\
         original_lines: {lines}\n\
         estimated_tokens: {tokens}\n\
         retrieve: call obs_recall with {{\"id\":\"{id}\",\"offset\":0}}; continue with the returned next_offset\n\
         [first complete lines, up to {head_budget} bytes]\n\
         {head}\
         [middle omitted; last complete lines, up to {tail_budget} bytes]\n\
         {tail}\
         [{bytes} original bytes omitted]",
        id = observation.id,
        tool = observation.tool_name,
        bytes = observation.bytes,
        lines = observation.lines,
        tokens = observation.tokens,
        head_budget = head_budget,
        tail_budget = tail_budget,
        head = head,
        tail = tail,
    )
}

pub(crate) struct RecallChunk {
    pub(crate) text: String,
    pub(crate) bytes: usize,
    pub(crate) lines: usize,
    pub(crate) next_offset: usize,
    pub(crate) eof: bool,
}

/// Trim a chunk end so it never splits a UTF-8 code point.
fn trim_utf8_end(bytes: &[u8], mut end: usize) -> usize {
    while end > 0 && end < bytes.len() && (bytes[end] & 0xc0) == 0x80 {
        end -= 1;
    }
    end
}

/// Read one page of a stored observation. `offset` is a byte offset;
/// `next_offset` from the previous chunk continues the read.
pub(crate) fn read_recall_chunk(
    session_path: &Path,
    id: &str,
    offset: usize,
    limits: (usize, usize),
) -> std::io::Result<RecallChunk> {
    if !is_observation_id(id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unknown observation id: {id}"),
        ));
    }
    let path = observation_path(session_path, id);
    let full = fs::read(&path)?;
    if offset > full.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("offset {offset} exceeds observation size {}", full.len()),
        ));
    }
    let (max_bytes, max_lines) = limits;
    let available = &full[offset..];
    let mut end = available.len().min(max_bytes);
    let mut newline_count = 0;
    for (index, byte) in available[..end].iter().enumerate() {
        if *byte == b'\n' {
            newline_count += 1;
            if newline_count == max_lines {
                end = index + 1;
                break;
            }
        }
    }
    let end = trim_utf8_end(available, end);
    let chunk = &available[..end];
    let text = String::from_utf8_lossy(chunk).into_owned();
    let next_offset = offset + end;
    Ok(RecallChunk {
        lines: text.lines().count(),
        bytes: end,
        next_offset,
        eof: next_offset >= full.len(),
        text,
    })
}

/// The full recall tool result text: bounded header + paged body, so the
/// header itself can never push the reply past the model-facing caps.
pub(crate) fn recall_result(
    session_path: &Path,
    id: &str,
    offset: usize,
) -> std::io::Result<String> {
    const HEADER_RESERVE_BYTES: usize = 512;
    const HEADER_LINES: usize = 2;
    let chunk = read_recall_chunk(
        session_path,
        id,
        offset,
        (
            RECALL_MAX_BYTES - HEADER_RESERVE_BYTES,
            RECALL_MAX_LINES - HEADER_LINES,
        ),
    )?;
    let header = format!(
        "[obs_recall id={id} offset={offset} next_offset={next} eof={eof}]\n\
         [chunk_bytes={bytes} chunk_lines={lines}; use next_offset to continue]",
        next = chunk.next_offset,
        eof = chunk.eof,
        bytes = chunk.bytes,
        lines = chunk.lines,
    );
    let content = format!("{header}\n{}", chunk.text);
    if content.len() > RECALL_MAX_BYTES || content.lines().count() > RECALL_MAX_LINES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "recall output exceeded its hard limit",
        ));
    }
    Ok(content)
}

/// Per-session projection state: send counts for the grace period plus the
/// set of ids already verified as stored. Counted structurally — `project`
/// recounts from the history each call — so the send cache is only a fast
/// path; a resume or fork rebuilds the counts from scratch and stays
/// correct.
pub(crate) struct ProjectionState {
    sends: Mutex<HashMap<String, usize>>,
    /// Ids whose archive write has already been verified this run; skips
    /// the per-request `fs::read` + re-hash of `ensure_stored`.
    verified: Mutex<HashMap<String, ()>>,
    /// Ids already reported as packed this run — the transcript note fires
    /// once per id, not on every request.
    reported: Mutex<HashMap<String, ()>>,
    /// User-facing notes accumulated by `project` and drained by the agent
    /// loop after each request.
    notes: Mutex<Vec<String>>,
}

impl ProjectionState {
    pub(crate) fn new() -> Self {
        Self {
            sends: Mutex::new(HashMap::new()),
            verified: Mutex::new(HashMap::new()),
            reported: Mutex::new(HashMap::new()),
            notes: Mutex::new(Vec::new()),
        }
    }

    /// Drain the user-facing notes accumulated by the last `project` call:
    /// one line per id the placeholder took over for the first time.
    pub(crate) fn take_notes(&self) -> Vec<String> {
        self.notes
            .lock()
            .map(|mut notes| std::mem::take(&mut *notes))
            .unwrap_or_default()
    }
}

impl Default for ProjectionState {
    fn default() -> Self {
        Self::new()
    }
}

/// Count, for each message index, how many assistant messages *follow* it in
/// the history — the number of provider requests the message has already
/// been part of. Structural, so it survives resume/compaction with no extra
/// persisted state.
fn prior_assistant_counts(messages: &[crate::core::types::ChatMessage]) -> Vec<usize> {
    let mut counts = vec![0usize; messages.len()];
    let mut following = 0usize;
    for index in (0..messages.len()).rev() {
        counts[index] = following;
        if messages[index].role == crate::core::types::Role::Assistant {
            following += 1;
        }
    }
    counts
}

/// The projection: build the provider-bound message list from the intact
/// history. Large tool results past their grace period are replaced with
/// placeholders; the stored history is never touched. Any storage failure
/// fails open — the original full result is sent.
pub(crate) fn project(
    state: &ProjectionState,
    session_path: Option<&Path>,
    messages: &[crate::core::types::ChatMessage],
) -> Vec<crate::core::types::ChatMessage> {
    use crate::core::types::{ChatMessage, Role};

    let prior = prior_assistant_counts(messages);
    let mut projected: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        let mut packed = false;
        if message.role == Role::Tool {
            if let Some(session_path) = session_path {
                // The wire `name` (when present) keeps different tools'
                // outputs from colliding into one id and tells the model
                // which tool a placeholder came from.
                let tool_name = message.name.as_deref().unwrap_or("tool");
                if let Some(observation) = create_observation(tool_name, message.content_str()) {
                    // Already-verified ids skip the storage check entirely:
                    // the object was written (or verified byte-for-byte)
                    // earlier this run, and content addressing means it
                    // cannot drift. This keeps steady-state requests at
                    // zero archive IO.
                    let verified = state
                        .verified
                        .lock()
                        .map(|guard| guard.contains_key(&observation.id))
                        .unwrap_or(false);
                    let stored = verified
                        || ensure_stored(session_path, &observation)
                            .map(|_| {
                                if let Ok(mut guard) = state.verified.lock() {
                                    guard.insert(observation.id.clone(), ());
                                }
                            })
                            .is_ok();
                    if stored {
                        // Grace period: send in full until FULL_SENDS provider
                        // requests have seen this payload. The cached count is
                        // seeded from the structural count (assistant messages
                        // that follow the result), so a restart resumes in the
                        // right phase. The counter increments before the call
                        // resolves, so a cancelled request still consumes a
                        // grace send; the model can always obs_recall.
                        let previous = state
                            .sends
                            .lock()
                            .map(|mut sends| {
                                *sends.entry(observation.id.clone()).or_insert(prior[index])
                            })
                            .unwrap_or(prior[index]);
                        if previous < FULL_SENDS {
                            if let Ok(mut sends) = state.sends.lock() {
                                sends.insert(observation.id.clone(), previous + 1);
                            }
                        } else {
                            let placeholder = placeholder_for(&observation);
                            projected.push(ChatMessage {
                                role: Role::Tool,
                                content: Some(placeholder),
                                tool_calls: None,
                                tool_call_id: message.tool_call_id.clone(),
                                name: message.name.clone(),
                                reasoning_items: None,
                                reasoning_content: None,
                            });
                            packed = true;
                            // First takeover of this id this run: leave a
                            // note the agent loop surfaces in the transcript,
                            // so the otherwise-invisible swap is observable.
                            if let Ok(mut reported) = state.reported.lock() {
                                if reported.insert(observation.id.clone(), ()).is_none() {
                                    if let Ok(mut notes) = state.notes.lock() {
                                        notes.push(format!(
                                            "obs pack: {} result {} archived — the model recalls pages with obs_recall",
                                            observation.tool_name,
                                            crate::agent::evidence_reducer::format_bytes(
                                                observation.bytes
                                            ),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if !packed {
            projected.push(message.clone());
        }
    }
    projected
}

/// Execute the `obs_recall` tool against the session's observation archive.
pub(crate) fn tool_obs_recall(
    session_path: &Path,
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, crate::tools::ToolError> {
    let id = args
        .get("id")
        .and_then(serde_json::Value::as_str)
        .ok_or(crate::tools::ToolError::Missing("id"))?;
    let offset = args
        .get("offset")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0) as usize;
    if !is_observation_id(id) {
        return Err(crate::tools::ToolError::InvalidArgument(format!(
            "unknown observation id: {id}"
        )));
    }
    recall_result(session_path, id, offset).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            crate::tools::ToolError::InvalidArgument(format!("unknown observation id: {id}"))
        } else {
            crate::tools::ToolError::Io(error)
        }
    })
}

/// Build the observation object for any payload, without the participation
/// threshold: the evidence reducer archives diagnostic bodies as small as
/// 4 KiB, below `create_observation`'s 10 KiB floor.
pub(crate) fn observation_for(tool_name: &str, text: &str) -> Observation {
    let bytes = text.len();
    let id = format!(
        "obs_{}",
        hash_hex(format!("{tool_name}\0{text}").as_bytes())
    );
    Observation {
        id,
        tool_name: tool_name.to_string(),
        text: text.to_string(),
        bytes,
        lines: text.lines().count(),
        tokens: payload_tokens(text),
    }
}

/// Read one archived observation back as a string. The id is validated the
/// same way recall validates it, so it is never a path traversal vector.
pub(crate) fn read_observation(session_path: &Path, id: &str) -> std::io::Result<String> {
    if !is_observation_id(id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unknown observation id: {id}"),
        ));
    }
    fs::read_to_string(observation_path(session_path, id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{ChatMessage, Role};
    use crate::session::TEST_SESSIONS_ENV_LOCK;

    fn big_text(lines: usize) -> String {
        (0..lines)
            .map(|i| format!("line {i}: some build output that keeps going\n"))
            .collect()
    }

    fn temp_session_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("dex-obs-pack-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn small_results_never_pack() {
        assert!(create_observation("bash", "short output").is_none());
        assert!(create_observation("bash", "").is_none());
        let at_threshold = "x".repeat(THRESHOLD_BYTES);
        assert!(create_observation("bash", &at_threshold).is_none());
    }

    #[test]
    fn ids_are_content_addressed_and_validated() {
        let a = create_observation("bash", &big_text(600)).unwrap();
        let b = create_observation("bash", &big_text(600)).unwrap();
        assert_eq!(a.id, b.id, "identical payloads dedup");
        let c = create_observation("read", &big_text(600)).unwrap();
        assert_ne!(a.id, c.id, "tool name is part of the identity");
        assert!(is_observation_id(&a.id));
        assert!(!is_observation_id("../../etc/passwd"));
        assert!(!is_observation_id("obs_upper"));
        assert!(!is_observation_id("obs_short"));
    }

    #[test]
    fn unnamed_results_get_a_default_tool_name() {
        let a = create_observation("tool", &big_text(600)).unwrap();
        let b = create_observation("tool", &big_text(600)).unwrap();
        assert_eq!(a.id, b.id);
        let named = create_observation("read", &big_text(600)).unwrap();
        assert_ne!(a.id, named.id, "no name and a name must not collide");
    }

    #[test]
    fn placeholder_carries_recall_instructions_and_excerpts() {
        let text = big_text(600);
        let observation = create_observation("bash", &text).unwrap();
        let placeholder = placeholder_for(&observation);
        assert!(placeholder.contains(&observation.id));
        assert!(placeholder.contains(&observation.tool_name));
        assert!(placeholder.contains("original_bytes:"));
        assert!(placeholder.contains("obs_recall"));
        assert!(placeholder.contains("line 0:"), "head excerpt present");
        assert!(
            placeholder.contains(&format!("line {}:", observation.lines - 1)),
            "tail excerpt present"
        );
        assert!(placeholder.len() < 3000, "placeholder stays small");
        // Placeholder must be much smaller than the payload for the trick
        // to pay at all.
        assert!(placeholder.len() < text.len() / 4);
    }

    #[test]
    fn minified_payloads_get_a_raw_excerpt() {
        // One enormous line: whole-line excerpts are empty, the raw
        // fallback must still surface content.
        let text = format!("{{\"data\":\"{}\"}}", "x".repeat(THRESHOLD_BYTES * 2));
        let observation = create_observation("bash", &text).unwrap();
        let placeholder = placeholder_for(&observation);
        assert!(
            placeholder.contains("\"data\":\""),
            "raw head excerpt present"
        );
        assert!(
            placeholder.contains(&observation.id),
            "recall instructions still present"
        );
        assert!(placeholder.len() < 3000, "placeholder stays small");
    }

    #[test]
    fn storage_is_content_addressed_and_idempotent() {
        let dir = temp_session_dir("store");
        let session = dir.join("s.jsonl");
        let observation = create_observation("bash", &big_text(600)).unwrap();
        ensure_stored(&session, &observation).unwrap();
        ensure_stored(&session, &observation).unwrap();
        let stored = fs::read_to_string(observation_path(&session, &observation.id)).unwrap();
        assert_eq!(stored, observation.text);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_observation_directory_is_refused() {
        let dir = temp_session_dir("symlink");
        let session = dir.join("s.jsonl");
        let outside = temp_session_dir("symlink-target");
        std::os::unix::fs::symlink(&outside, dir.join("obs")).unwrap();
        let observation = create_observation("bash", &big_text(600)).unwrap();
        let error = ensure_stored(&session, &observation).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        // Nothing was written through the symlink.
        assert!(
            fs::read_dir(outside.join("obs")).is_err(),
            "no archive directory leaked outside"
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn recall_paginates_with_utf8_safety() {
        let dir = temp_session_dir("recall");
        let session = dir.join("s.jsonl");
        let text = big_text(600);
        let observation = create_observation("bash", &text).unwrap();
        ensure_stored(&session, &observation).unwrap();

        let mut offset = 0;
        let mut pages = 0;
        let mut reassembled = String::new();
        loop {
            let chunk = read_recall_chunk(&session, &observation.id, offset, (1024, 50)).unwrap();
            assert!(chunk.text.len() <= 1024);
            reassembled.push_str(&chunk.text);
            if chunk.eof {
                break;
            }
            offset = chunk.next_offset;
            pages += 1;
            assert!(pages < 1000, "pagination must terminate");
        }
        assert_eq!(reassembled, text, "paged recall must round-trip exactly");

        let error = read_recall_chunk(&session, &observation.id, u64::MAX as usize, (1024, 50));
        assert!(error.is_err(), "offset past the end is rejected");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn recall_rejects_bad_ids() {
        let dir = temp_session_dir("badid");
        let session = dir.join("s.jsonl");
        let mut args = serde_json::Map::new();
        args.insert("id".into(), serde_json::json!("../escape"));
        assert!(tool_obs_recall(&session, &args).is_err());
        args.insert("id".into(), serde_json::json!("obs_does_not_exist_000000"));
        let error = tool_obs_recall(&session, &args);
        assert!(matches!(
            error,
            Err(crate::tools::ToolError::InvalidArgument(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn projection_keeps_history_intact_and_fails_open() {
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = temp_session_dir("project");
        let session = dir.join("s.jsonl");

        let big = big_text(600);
        let tool_result = |call_id: &str| {
            vec![
                ChatMessage::assistant_calls(
                    None,
                    vec![crate::core::types::LlmToolCall {
                        id: call_id.into(),
                        call_type: "function".into(),
                        function: crate::core::types::FunctionCall {
                            name: "bash".into(),
                            arguments: "{}".into(),
                        },
                    }],
                ),
                ChatMessage::tool_result(call_id, big.clone()),
            ]
        };
        let system = ChatMessage::system("sys");

        let state = ProjectionState::new();
        // Simulate the real loop: each request appends the model's reply, so
        // the tool result gains one trailing assistant message per request —
        // that trailing count is what the structural send counting reads.
        let mut request = vec![system.clone(), ChatMessage::user("run the build")];
        request.extend(tool_result("call1"));
        let mut full_sends = 0;
        for turn in 0..FULL_SENDS {
            let projected = project(&state, Some(&session), &request);
            let tool_msg = projected
                .iter()
                .find(|m| m.role == Role::Tool)
                .expect("tool result present");
            assert_eq!(tool_msg.content_str(), big, "grace period sends full");
            full_sends += 1;
            request.push(ChatMessage::assistant(format!("ack {turn}")));
        }
        assert_eq!(full_sends, FULL_SENDS);
        // Next request: placeholder replaces the payload in the projection.
        let projected = project(&state, Some(&session), &request);
        let tool_msg = projected.iter().find(|m| m.role == Role::Tool).unwrap();
        assert!(tool_msg.content_str().contains("obs_recall"));
        assert!(tool_msg.content_str().len() < big.len() / 4);
        // History itself is untouched.
        let stored = request
            .iter()
            .find(|m| m.role == Role::Tool)
            .map(|m| m.content_str().to_string())
            .unwrap();
        assert_eq!(stored, big, "stored history is never rewritten");

        // No session path -> fail open: full result, no packing.
        let state2 = ProjectionState::new();
        let projected = project(&state2, None, &request);
        let tool_msg = projected.iter().find(|m| m.role == Role::Tool).unwrap();
        assert_eq!(tool_msg.content_str(), big);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_placeholder_takeover_leaves_a_note_once() {
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = temp_session_dir("notes");
        let session = dir.join("s.jsonl");
        let big = big_text(600);
        let mut request = vec![ChatMessage::system("sys"), ChatMessage::user("go")];
        request.push(ChatMessage::tool_result("call1", big.clone()));

        let state = ProjectionState::new();
        for turn in 0..FULL_SENDS {
            let projected = project(&state, Some(&session), &request);
            let tool_msg = projected.iter().find(|m| m.role == Role::Tool).unwrap();
            assert_eq!(tool_msg.content_str(), big, "grace period sends full");
            assert!(state.take_notes().is_empty(), "grace sends are silent");
            request.push(ChatMessage::assistant(format!("ack {turn}")));
        }
        // First takeover: exactly one note, naming the tool and the size.
        let projected = project(&state, Some(&session), &request);
        let tool_msg = projected.iter().find(|m| m.role == Role::Tool).unwrap();
        assert!(tool_msg.content_str().contains("obs_recall"));
        let notes = state.take_notes();
        assert_eq!(notes.len(), 1, "one note for the first takeover: {notes:?}");
        assert!(notes[0].contains("tool result"), "{notes:?}");
        assert!(notes[0].contains("obs_recall"), "{notes:?}");
        // Later requests keep placeholdering but never repeat the note.
        let projected = project(&state, Some(&session), &request);
        assert!(projected
            .iter()
            .find(|m| m.role == Role::Tool)
            .unwrap()
            .content_str()
            .contains("obs_recall"));
        assert!(state.take_notes().is_empty(), "no repeat note");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn projected_view_is_smaller_than_history() {
        // The token estimator on the projected view must reflect the
        // placeholder, not the archived payload — the compaction threshold
        // and online-compaction sampling read the projection.
        let dir = temp_session_dir("tokens");
        let session = dir.join("s.jsonl");
        let big = big_text(600);
        let mut request = vec![ChatMessage::system("sys"), ChatMessage::user("go")];
        request.push(ChatMessage::tool_result("call1", big.clone()));
        request.push(ChatMessage::assistant("ack"));
        request.push(ChatMessage::assistant("ack2"));
        request.push(ChatMessage::assistant("ack3"));

        let state = ProjectionState::new();
        let projected = project(&state, Some(&session), &request);
        let full = crate::agent::tokens::estimate_tokens(&request);
        let packed = crate::agent::tokens::estimate_tokens(&projected);
        assert!(
            packed < full / 2,
            "projected view must be much cheaper: {packed} vs {full}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
