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

pub(crate) fn estimate_tokens(text: &str) -> usize {
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
        tokens: estimate_tokens(text),
    })
}

/// Write the payload to its content-addressed path, refusing symlinked
/// directories and verifying an existing object byte-for-byte before
/// reusing it. `create_new` + hash check: a matching size and hash proves
/// the existing file is this exact payload.
pub(crate) fn ensure_stored(session_path: &Path, observation: &Observation) -> std::io::Result<()> {
    let dir = observations_dir(session_path);
    fs::create_dir_all(&dir)?;
    let metadata = fs::metadata(&dir)?;
    #[cfg(unix)]
    if metadata.file_type().is_symlink() {
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
        let mut reversed: Vec<&str> = text.lines().rev().collect();
        reversed.reverse();
        // Reversed order above is wrong for taking from the end while
        // preserving order; rebuild properly.
        let _ = reversed;
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

/// The stable placeholder that replaces the payload in the projection.
pub(crate) fn placeholder_for(observation: &Observation) -> String {
    let head_budget = PLACEHOLDER_EXCERPT_BYTES / 2;
    let tail_budget = PLACEHOLDER_EXCERPT_BYTES - head_budget;
    let head = complete_line_excerpt(&observation.text, head_budget, false);
    let tail = complete_line_excerpt(&observation.text, tail_budget, true);
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
    let take = available.len().min(max_bytes + 4);
    let mut end = take.min(max_bytes);
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

/// Per-session send counts for the projection. Counted structurally —
/// `project` recounts from the history each call — so this cache is only a
/// fast path; a resume or fork rebuilds the counts from scratch and stays
/// correct.
pub(crate) struct ProjectionState {
    sends: Mutex<HashMap<String, usize>>,
}

impl ProjectionState {
    pub(crate) fn new() -> Self {
        Self {
            sends: Mutex::new(HashMap::new()),
        }
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
                if let Some(observation) = create_observation("tool", message.content_str()) {
                    if ensure_stored(session_path, &observation).is_ok() {
                        // Grace period: send in full until FULL_SENDS provider
                        // requests have seen this payload. The cached count is
                        // seeded from the structural count (assistant messages
                        // that follow the result), so a restart resumes in the
                        // right phase.
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
    fn placeholder_carries_recall_instructions_and_excerpts() {
        let text = big_text(600);
        let observation = create_observation("bash", &text).unwrap();
        let placeholder = placeholder_for(&observation);
        assert!(placeholder.contains(&observation.id));
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
}
