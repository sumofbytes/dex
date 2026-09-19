//! Session store tests, split out of `store.rs` (which stays the
//! `Session` load/append/undo implementation).

use super::*;
use crate::session::changes::{load_changes, make_change_record, record_change};
use crate::session::events::events_cache;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};

fn unique_path(prefix: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let mut h = DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    let tid = h.finish();
    let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "{}-{}-{}-{}-{}.jsonl",
        prefix,
        std::process::id(),
        tid,
        nanos,
        nonce
    ))
}

#[test]
fn clear_marker_removes_messages_during_recovery() {
    let path = unique_path("dex-session-test");
    let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
    let message = r#"{"type":"message","id":"1","timestamp":"2020-01-01T00:00:00Z","role":"user","content":"old"}"#;
    let clear = r#"{"type":"clear","id":"2","timestamp":"2020-01-01T00:00:00Z"}"#;
    fs::write(&path, format!("{}\n{}\n{}\n", header, message, clear)).unwrap();
    assert!(load_messages_from_session(&path).unwrap().is_empty());
    // A clear followed by new messages loads again.
    fs::write(
        &path,
        format!("{}\n{}\n{}\n{}\n", header, message, clear, message),
    )
    .unwrap();
    assert_eq!(load_messages_from_session(&path).unwrap().len(), 1);
    // System-only sessions hold no displayable content.
    let system = r#"{"type":"message","id":"3","timestamp":"2020-01-01T00:00:00Z","role":"system","content":"note"}"#;
    fs::write(&path, format!("{}\n{}\n", header, system)).unwrap();
    assert!(load_messages_from_session(&path).unwrap().is_empty());
    let _ = fs::remove_file(path);
}

#[test]
fn session_state_last_write_wins_and_ignores_other_entries() {
    let path = unique_path("dex-session-state");
    let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
    let first = r#"{"type":"session_state","id":"1","timestamp":"2020-01-01T00:00:01Z","key":"model","value":"old-model"}"#;
    let message = r#"{"type":"message","id":"2","timestamp":"2020-01-01T00:00:02Z","role":"user","content":"hi"}"#;
    let second = r#"{"type":"session_state","id":"3","timestamp":"2020-01-01T00:00:03Z","key":"model","value":"new-model"}"#;
    let provider = r#"{"type":"session_state","id":"4","timestamp":"2020-01-01T00:00:04Z","key":"provider","value":"openai-codex"}"#;
    fs::write(
        &path,
        format!(
            "{}\n{}\n{}\n{}\n{}\n",
            header, first, message, second, provider
        ),
    )
    .unwrap();
    let state = load_session_state(&path).unwrap();
    assert_eq!(state.get("model").map(String::as_str), Some("new-model"));
    assert_eq!(
        state.get("provider").map(String::as_str),
        Some("openai-codex")
    );
    assert_eq!(state.len(), 2);
    let _ = fs::remove_file(path);
}

#[test]
fn session_state_missing_file_is_an_error() {
    let path = unique_path("dex-session-state-missing");
    let _ = fs::remove_file(&path);
    assert!(load_session_state(&path).is_err());
}

/// Reasoning replay fields must survive the JSONL journal: the
/// flattened `ChatMessage` (session.rs `SessionMessageEntry`) round-trips
/// `reasoning_items` + `reasoning_content` so a resumed session keeps
/// its reasoning thread instead of re-reasoning from scratch.
#[test]
fn message_round_trip_preserves_reasoning_replay_fields() {
    let path = unique_path("dex-session-reasoning");
    let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
    let mut msg = serde_json::to_value(ChatMessage {
        role: Role::Assistant,
        content: Some("done".to_string()),
        tool_calls: None,
        tool_call_id: None,
        name: None,
        reasoning_items: Some(vec![serde_json::json!({
            "type": "reasoning",
            "id": "r1",
            "encrypted_content": "blob1",
        })]),
        reasoning_content: Some("step 1".to_string()),
    })
    .unwrap();
    msg["type"] = serde_json::json!("message");
    msg["id"] = serde_json::json!("1");
    msg["timestamp"] = serde_json::json!("2020-01-01T00:00:00Z");
    fs::write(&path, format!("{}\n{}\n", header, msg)).unwrap();
    let loaded = load_messages_from_session(&path).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(
        loaded[0].reasoning_items.as_ref().unwrap()[0]["encrypted_content"],
        "blob1"
    );
    assert_eq!(loaded[0].reasoning_content.as_deref(), Some("step 1"));
    let _ = fs::remove_file(path);
}

#[test]
fn llm_history_drops_excluded_shell_runs_but_transcript_keeps_them() {
    // `!!`: saved to history and shown in the TUI, never sent to
    // the LLM. The transcript rebuild uses the full load; the
    // model-bound load filters.
    use crate::protocol::BASH_EXCLUDED_NAME;
    let path = unique_path("dex-session-shell-exclude");
    let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
    fs::write(&path, format!("{header}\n")).unwrap();
    let mut session = Session::from_path(&path).unwrap();
    session
        .append_message(&ChatMessage::user("Ran `echo hi`\n```\nhi\n```"))
        .unwrap();
    session
        .append_message(&ChatMessage::user_named(
            "Ran `echo secret`\n```\nsecret\n```",
            BASH_EXCLUDED_NAME,
        ))
        .unwrap();
    drop(session);
    let full = load_messages_from_session(&path).unwrap();
    assert_eq!(full.len(), 2);
    let llm = load_llm_messages_from_session(&path).unwrap();
    assert_eq!(llm.len(), 1);
    assert!(llm[0].content_str().contains("echo hi"));
    let _ = fs::remove_file(path);
}

#[test]
fn events_journal_replays_after_seq_cursor() {
    // Sessions live under XDG_DATA_HOME: serialize against tests that redirect it.
    let _lock = TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut s = Session::new("/tmp/dex-events-test".into(), None).unwrap();
    s.append_event(0, r#"{"type":"assistant_text","data":"a"}"#)
        .unwrap();
    s.append_event(1, r#"{"type":"assistant_text","data":"b"}"#)
        .unwrap();
    s.append_event(2, r#"{"type":"turn_complete","data":{"response":"done"}}"#)
        .unwrap();
    s.append_event(3, r#"{"type":"assistant_text","data":"c"}"#)
        .unwrap();
    let path = s.path().unwrap().to_path_buf();
    let after = Session::load_events(&path, 2, usize::MAX).unwrap();
    let seqs: Vec<u64> = after.iter().map(|(seq, _)| *seq).collect();
    let texts: Vec<String> = after
        .iter()
        .map(|(_, p)| serde_json::from_str::<Value>(p).unwrap()["data"].to_string())
        .collect();
    assert_eq!(seqs, vec![2, 3]);
    assert_eq!(texts.len(), 2);
    assert_eq!(Session::max_event_seq(&path), Some(3));
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("events.jsonl"));
}

#[test]
fn history_cache_serves_appends_without_rescan() {
    let path = unique_path("dex-history-cache");
    let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
    fs::write(&path, format!("{header}\n")).unwrap();
    let mut s = Session::from_path(&path).unwrap();
    s.append_message(&ChatMessage::user("one")).unwrap();
    // Miss parses; hit serves the snapshot.
    assert_eq!(load_messages_from_session(&path).unwrap().len(), 1);
    assert_eq!(load_messages_from_session(&path).unwrap().len(), 1);
    // The write funnel extends the snapshot: still no re-parse.
    s.append_message(&ChatMessage::user("two")).unwrap();
    let two = load_messages_from_session(&path).unwrap();
    assert_eq!(two.len(), 2);
    assert!(two[1].content_str().contains("two"));
    // `clear` folds the snapshot like the loader folds the file.
    s.clear_messages().unwrap();
    assert!(load_messages_from_session(&path).unwrap().is_empty());
    s.append_message(&ChatMessage::user("three")).unwrap();
    assert_eq!(load_messages_from_session(&path).unwrap().len(), 1);
    // An out-of-band rewrite misses and re-parses.
    fs::write(&path, format!("{header}\n")).unwrap();
    assert!(load_messages_from_session(&path).unwrap().is_empty());
    // A missing file evicts instead of serving stale rows.
    fs::remove_file(&path).unwrap();
    assert!(load_messages_from_session(&path).is_err());
    assert!(history_cache_get(&path).is_none());
}

#[test]
fn history_cache_hit_repairs_dangling_tool_calls_idempotently() {
    use crate::protocol::{FunctionCall, LlmToolCall};
    let path = unique_path("dex-history-cache-repair");
    let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
    fs::write(&path, format!("{header}\n")).unwrap();
    let mut s = Session::from_path(&path).unwrap();
    // Assistant tool call whose result never landed (cancelled turn).
    let dangling = ChatMessage::assistant_calls(
        Some("calling".into()),
        vec![LlmToolCall {
            id: "c1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "read".into(),
                arguments: "{}".into(),
            },
        }],
    );
    s.append_message(&dangling).unwrap();
    // Every load synthesizes the placeholder exactly once — the cached
    // hit must match a fresh parse, not accumulate duplicates.
    let first = load_messages_from_session(&path).unwrap();
    let second = load_messages_from_session(&path).unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(second.len(), 2);
    let _ = fs::remove_file(&path);
}

#[test]
fn history_cache_touch_evicts_on_foreign_append() {
    let path = unique_path("dex-history-foreign");
    let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
    fs::write(&path, format!("{header}\n")).unwrap();
    let mut s = Session::from_path(&path).unwrap();
    s.append_message(&ChatMessage::user("one")).unwrap();
    assert_eq!(load_messages_from_session(&path).unwrap().len(), 1);
    // A second process appends a message row behind our back...
    let foreign = ChatMessage::user("foreign");
    let entry = SessionMessageEntry {
        entry_type: "message",
        id: "foreign",
        timestamp: "2020-01-01T00:00:01Z",
        message: &foreign,
    };
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
    drop(f);
    // ...then our own turn marker touches the snapshot: the length delta
    // no longer matches, so it must evict rather than publish a vector
    // that silently omits the foreign row.
    s.turn_event("turn_start").unwrap();
    let loaded = load_messages_from_session(&path).unwrap();
    assert_eq!(loaded.len(), 2);
    assert!(loaded.iter().any(|m| m.content_str().contains("foreign")));
    let _ = fs::remove_file(&path);
}

#[test]
fn rewrite_messages_preserves_session_state() {
    let _lock = TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut s = Session::new("/tmp/dex-rewrite-state".into(), None).unwrap();
    s.set_state("plan", "the plan").unwrap();
    s.set_state("changes", "[]").unwrap();
    let path = s.path().unwrap().to_path_buf();
    // The turn loop's `messages` carry the system prompt at index 0; the
    // rewrite skips it and re-emits the rest.
    let messages = vec![ChatMessage::system("sys"), ChatMessage::user("keep")];
    s.rewrite_messages(&messages).unwrap();
    let state = load_session_state(&path).unwrap();
    assert_eq!(state.get("plan").map(String::as_str), Some("the plan"));
    assert_eq!(state.get("changes").map(String::as_str), Some("[]"));
    let after = load_messages_from_session(&path).unwrap();
    assert_eq!(after.len(), 1);
    assert!(after[0].content_str().contains("keep"));
    let _ = fs::remove_file(&path);
}

#[test]
fn events_page_limit_zero_serves_nothing() {
    let _lock = TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut s = Session::new("/tmp/dex-events-limit0".into(), None).unwrap();
    s.append_event(0, r#"{"type":"assistant_text","data":"a"}"#)
        .unwrap();
    let path = s.path().unwrap().to_path_buf();
    assert!(Session::load_events(&path, 0, 0).unwrap().is_empty());
    // A nonzero page still serves.
    assert_eq!(Session::load_events(&path, 0, 1).unwrap().len(), 1);
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("events.jsonl"));
}

#[test]
fn events_poll_skips_unchanged_journal_and_seeks_checkpoints() {
    let path = unique_path("dex-events-cache");
    let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
    fs::write(&path, format!("{header}\n")).unwrap();
    let mut s = Session::from_path(&path).unwrap();
    for seq in 0..4u64 {
        s.append_event(
            seq,
            &format!("{{\"type\":\"assistant_text\",\"data\":\"{seq}\"}}"),
        )
        .unwrap();
    }
    // Miss parses; the idle re-poll serves nothing without file IO.
    // Cursor is the next seq to serve (inclusive): `since=0` serves seq 0.
    assert_eq!(Session::load_events(&path, 0, usize::MAX).unwrap().len(), 4);
    assert!(Session::load_events(&path, 4, usize::MAX)
        .unwrap()
        .is_empty());
    // A behind cursor re-scans and still gets every row exactly once.
    let behind = Session::load_events(&path, 1, usize::MAX).unwrap();
    assert_eq!(
        behind.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    // Appends extend the cursor: only the tail is served.
    s.append_event(4, r#"{"type":"assistant_text","data":"4"}"#)
        .unwrap();
    let tail = Session::load_events(&path, 4, usize::MAX).unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].0, 4);
    assert_eq!(Session::max_event_seq(&path), Some(4));
    // Grow past one checkpoint and seek to it: same tail, no full scan
    // by construction (checkpoints anchor the resume offset).
    for seq in 5..1600u64 {
        s.append_event(
            seq,
            &format!("{{\"type\":\"assistant_text\",\"data\":\"{seq}\"}}"),
        )
        .unwrap();
    }
    {
        let cache = events_cache().lock().expect("events cache lock");
        assert!(!cache
            .get(&path.with_extension("events.jsonl"))
            .unwrap()
            .checkpoints
            .is_empty());
    }
    let tail = Session::load_events(&path, 1591, usize::MAX).unwrap();
    assert_eq!(tail.len(), 9);
    assert_eq!(tail[0].0, 1591);
    assert_eq!(Session::max_event_seq(&path), Some(1599));
    // Out-of-band truncation falls back to a full scan, never garbage:
    // the 64-byte stump holds no complete row.
    let events_path = path.with_extension("events.jsonl");
    let stump = fs::read(&events_path).unwrap()[..64].to_vec();
    fs::write(&events_path, stump).unwrap();
    assert!(Session::load_events(&path, 0, usize::MAX)
        .unwrap()
        .is_empty());
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(events_path);
}

#[test]
fn events_journal_pages_with_exactly_once_delivery() {
    // §1: page-limited serving splits the journal across pages; chaining
    // pages by last served seq delivers every row exactly once, and the
    // drained tail re-arms the one-stat idle fast path.
    let path = unique_path("dex-events-pages");
    let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
    fs::write(&path, format!("{header}\n")).unwrap();
    let mut s = Session::from_path(&path).unwrap();
    for seq in 0..7u64 {
        s.append_event(
            seq,
            &format!("{{\"type\":\"assistant_text\",\"data\":\"{seq}\"}}"),
        )
        .unwrap();
    }
    // Cursor semantics are inclusive (`seq >= since`): `since` is the
    // next seq to serve, so pages chain with `last + 1` (the daemon's
    // `next_seq`). Every row lands exactly once, including seq 0.
    let mut since = 0u64;
    let mut got = Vec::new();
    for _ in 0..10 {
        let page = Session::load_events(&path, since, 3).unwrap();
        if page.is_empty() {
            break;
        }
        since = page.last().unwrap().0 + 1;
        got.extend(page.into_iter().map(|(seq, _)| seq));
    }
    assert_eq!(got, vec![0, 1, 2, 3, 4, 5, 6]);
    // Drained: the idle re-poll serves nothing (fast path, no file open).
    assert!(Session::load_events(&path, since, 3).unwrap().is_empty());
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("events.jsonl"));
}

#[test]
fn events_opener_skips_line_count_scan() {
    let path = unique_path("dex-events-opener");
    let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
    fs::write(&path, format!("{header}\n")).unwrap();
    let mut s = Session::from_path(&path).unwrap();
    s.append_message(&ChatMessage::user("history")).unwrap();
    // Header-only open: no scan, counter unused — the handle only ever
    // appends events.
    let mut journal = Session::from_path_for_events(&path).unwrap();
    journal
        .append_event(0, r#"{"type":"assistant_text","data":"a"}"#)
        .unwrap();
    // The row landed (max sees seq 0); replay serves it to a `since=0`
    // poll under the inclusive cursor semantics.
    assert_eq!(Session::max_event_seq(&path), Some(0));
    journal
        .append_event(1, r#"{"type":"assistant_text","data":"b"}"#)
        .unwrap();
    let rows = Session::load_events(&path, 0, usize::MAX).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, 0);
    assert_eq!(rows[1].0, 1);
    let _ = fs::remove_file(&path);
    let _ = fs::remove_file(path.with_extension("events.jsonl"));
}

#[test]
fn scan_summary_matches_full_load() {
    let path = unique_path("dex-scan-summary");
    let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
    let user = r#"{"type":"message","id":"1","timestamp":"2020-01-01T00:00:00Z","role":"user","content":"hi"}"#;
    let system = r#"{"type":"message","id":"2","timestamp":"2020-01-01T00:00:00Z","role":"system","content":"note"}"#;
    let bad = r#"{"type":"message","id":"bad","role":"mystery"}"#;
    let start = r#"{"type":"turn_start","id":"3","timestamp":"2020-01-01T00:00:00Z"}"#;
    let done = r#"{"type":"turn_complete","id":"4","timestamp":"2020-01-01T00:00:00Z"}"#;
    let state = r#"{"type":"session_state","id":"5","timestamp":"2020-01-01T00:00:00Z","key":"k","value":"v"}"#;
    // Non-compact spacing exercises the parse-and-classify fallback.
    let spaced = r#"{ "type" : "message" , "id" : "6" , "role" : "user" , "content" : "sp" }"#;
    fs::write(
        &path,
        format!("{header}\n{user}\n{system}\n{bad}\n{start}\n{done}\n{state}\n{spaced}\n"),
    )
    .unwrap();
    let loaded = load_messages_from_session(&path).unwrap();
    let (count, turn) = Session::scan_summary(&path).unwrap();
    assert_eq!(count, loaded.len());
    assert_eq!(count, 2);
    assert_eq!(turn, "complete");
    // `clear` folds the count like the loader folds the vec.
    let clear = r#"{"type":"clear","id":"7","timestamp":"2020-01-01T00:00:00Z"}"#;
    fs::write(&path, format!("{header}\n{user}\n{clear}\n{spaced}\n")).unwrap();
    let (count, _) = Session::scan_summary(&path).unwrap();
    assert_eq!(count, load_messages_from_session(&path).unwrap().len());
    assert_eq!(count, 1);
    // Missing file is an error (callers map to 0/unknown).
    let _ = fs::remove_file(&path);
    assert!(Session::scan_summary(&path).is_err());
}

#[test]
fn last_turn_state_tracks_terminal_entries() {
    let path = unique_path("dex-turn-state");
    let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
    let start = r#"{"type":"turn_start","id":"1","timestamp":"2020-01-01T00:00:00Z"}"#;
    fs::write(&path, format!("{}\n{}\n", header, start)).unwrap();
    assert_eq!(Session::last_turn_state(&path), "interrupted");
    // Append a terminal entry and it flips.
    let done = r#"{"type":"turn_complete","id":"2","timestamp":"2020-01-01T00:00:00Z"}"#;
    fs::write(&path, format!("{}\n{}\n{}\n", header, start, done)).unwrap();
    assert_eq!(Session::last_turn_state(&path), "complete");
    let _ = fs::remove_file(&path);
}

#[test]
fn find_by_id_filename_resolves_exact_and_prefix() {
    // Sessions live under XDG_DATA_HOME: redirect + serialize against
    // tests doing the same. Two sessions prove the lookup discriminates
    // by filename instead of returning the first header parsed.
    let _lock = TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard(vec![("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]);
    let dir = std::env::temp_dir().join(format!("dex-find-id-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::env::set_var("XDG_DATA_HOME", &dir);
    let a = Session::new("/tmp/dex-find-a".into(), None).unwrap();
    let b = Session::new("/tmp/dex-find-b".into(), None).unwrap();
    let (ida, pa) = (a.id().to_string(), a.path().unwrap().to_path_buf());
    let (idb, pb) = (b.id().to_string(), b.path().unwrap().to_path_buf());
    assert_ne!(ida, idb);
    assert_eq!(Session::find_by_id_filename(&ida), Some(pa.clone()));
    assert_eq!(Session::find_by_id_filename(&idb), Some(pb.clone()));
    // Near-full prefix: the 8-char workspace slug collides across
    // sessions, so abbreviate inside the random suffix instead.
    assert_eq!(
        Session::find_by_id_filename(&ida[..ida.len() - 1]),
        Some(pa.clone())
    );
    assert_eq!(Session::find_by_id_filename("no-such-session"), None);
    drop(a);
    drop(b);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_messages_and_plan_matches_separate_loads() {
    let path = unique_path("dex-messages-plan");
    let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
    let user = r#"{"type":"message","id":"1","timestamp":"2020-01-01T00:00:00Z","role":"user","content":"hi"}"#;
    let want = crate::protocol::Plan {
        goal: Some("g".into()),
        steps: vec![("s".into(), false)],
        constraints: Vec::new(),
        acceptance: Vec::new(),
    };
    let state = format!(
        r#"{{"type":"session_state","id":"2","timestamp":"2020-01-01T00:00:00Z","key":"plan","value":{}}}"#,
        serde_json::to_string(&want.to_json()).unwrap()
    );
    fs::write(&path, format!("{header}\n{user}\n{state}\n")).unwrap();
    let (messages, plan) = load_messages_and_plan(&path).unwrap();
    // Same messages as the standalone loader, same plan as the
    // standalone second pass — from one scan.
    assert_eq!(
        serde_json::to_string(&messages).unwrap(),
        serde_json::to_string(&load_messages_from_session(&path).unwrap()).unwrap()
    );
    assert_eq!(plan, load_plan(&path));
    assert_eq!(plan, want);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn change_ledger_records_then_undo_restores() {
    // Sessions live under XDG_DATA_HOME: serialize against tests that redirect it.
    let _lock = TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut s = Session::new("/tmp/dex-undo-test".into(), None).unwrap();
    let work = s.path().unwrap().parent().unwrap().join("work.txt");
    fs::write(&work, b"before\n").unwrap();
    let h_before = crate::tools::hash_file(&work.display().to_string());
    fs::write(&work, b"after\n").unwrap();
    let h_after = crate::tools::hash_file(&work.display().to_string());
    record_change(
        &mut s,
        make_change_record(
            "write",
            &work.display().to_string(),
            Some("before\n"),
            Some("after\n"),
            &h_before,
            &h_after,
        ),
    )
    .unwrap();
    // File is currently "after" — matches after_hash, so undo applies.
    let changes = load_changes(s.path().unwrap());
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].after_hash, h_after);
    let msg = undo_last_change(&mut s).unwrap();
    assert!(msg.contains("undid write"));
    assert_eq!(fs::read_to_string(&work).unwrap(), "before\n");
    assert!(load_changes(s.path().unwrap()).is_empty());
    let _ = fs::remove_file(&work);
    if let Some(p) = s.path() {
        let _ = fs::remove_file(p);
    }
}

#[test]
fn undo_refuses_when_file_moved_on() {
    // Sessions live under XDG_DATA_HOME: serialize against tests that redirect it.
    let _lock = TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut s = Session::new("/tmp/dex-undo-concurrent".into(), None).unwrap();
    let work = s.path().unwrap().parent().unwrap().join("c.txt");
    fs::write(&work, b"v1\n").unwrap();
    let h = crate::tools::hash_file(&work.display().to_string());
    record_change(
        &mut s,
        make_change_record(
            "write",
            &work.display().to_string(),
            Some("v1\n"),
            Some("v2\n"),
            &h,
            &h,
        ),
    )
    .unwrap();
    // Rewrite the file afterwards but keep the same hash (hash is of
    // content; simulate a concurrent edit changing it):
    // A concurrent edit changes the content -> new hash -> refuse.
    fs::write(&work, b"vX\n").unwrap();
    let err = undo_last_change(&mut s).unwrap_err();
    assert!(err.to_string().contains("refusing to undo"));
    let _ = fs::remove_file(&work);
    if let Some(p) = s.path() {
        let _ = fs::remove_file(p);
    }
}

#[test]
fn effect_journal_records_intent_and_outcome() {
    // Sessions live under XDG_DATA_HOME: serialize against tests that redirect it.
    let _lock = TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut s = Session::new("/tmp/dex-effect-test".into(), None).unwrap();
    s.effect_start("call-1", "edit", "abc").unwrap();
    s.turn_event("turn_start").unwrap();
    s.effect_result("call-1", true).unwrap();
    s.turn_event("turn_complete").unwrap();
    let text = fs::read_to_string(s.path().unwrap()).unwrap();
    assert!(text.contains("effect_start"));
    assert!(text.contains("call-1"));
    assert!(text.contains("effect_result"));
    assert!(text.contains("\"ok\":true"));
    assert!(Session::last_turn_state(s.path().unwrap()) == "complete");
    if let Some(p) = s.path() {
        let _ = fs::remove_file(p);
    }
}

/// The routed tier rides the `turn_start` marker (`None` serializes to
/// nothing, so unrouted journals stay byte-identical), and recovery
/// still keys on the marker type alone.
#[test]
fn turn_start_carries_routing_tier() {
    let _lock = TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut s = Session::new("/tmp/dex-tier-test".into(), None).unwrap();
    s.turn_event_with_tier("turn_start", Some("powerful"))
        .unwrap();
    s.turn_event("turn_complete").unwrap();
    let text = fs::read_to_string(s.path().unwrap()).unwrap();
    assert!(text.contains(r#""type":"turn_start""#), "{text}");
    assert!(text.contains(r#""tier":"powerful""#), "{text}");
    assert!(Session::last_turn_state(s.path().unwrap()) == "complete");
    if let Some(p) = s.path() {
        let _ = fs::remove_file(p);
    }
}

#[test]
fn record_skills_writes_session_state_entry() {
    // Sessions live under XDG_DATA_HOME: serialize against tests that redirect it.
    let _lock = TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let skills = vec![crate::protocol::Skill {
        name: "demo".into(),
        description: "does demo things".into(),
        path: std::path::PathBuf::from("/tmp/demo/SKILL.md"),
    }];
    let mut s = Session::new("/tmp/dex-skills-test".into(), None).unwrap();
    s.record_skills(&skills);
    let path = s.path().unwrap().to_path_buf();
    let state = load_session_state(&path).unwrap();
    let recorded: serde_json::Value = serde_json::from_str(state.get("skills").unwrap()).unwrap();
    assert_eq!(recorded[0]["name"], "demo");
    assert_eq!(recorded[0]["description"], "does demo things");
    assert_eq!(recorded[0]["path"], "/tmp/demo/SKILL.md");
    let _ = fs::remove_file(path);
}

#[test]
fn default_session_name_is_workspace_plus_k8s_suffix() {
    let name = Session::default_session_name("/home/user/dex");
    let (base, suffix) = name.rsplit_once('-').unwrap();
    assert_eq!(base, "dex");
    assert_eq!(suffix.len(), 7);
    assert!(suffix
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    // Sanitizes: punctuation/folds collapse to one `-`, lowercased.
    assert!(Session::default_session_name("/home/user/My Project!").starts_with("my-project-"));
    // Degenerate cwds fall back to `session`.
    assert!(Session::default_session_name("/").starts_with("session-"));
    assert!(Session::default_session_name("").starts_with("session-"));
    // Unique per call.
    assert_ne!(
        Session::default_session_name("/home/user/dex"),
        Session::default_session_name("/home/user/dex")
    );
}

#[test]
fn new_session_defaults_name_but_keeps_explicit() {
    // Sessions live under XDG_DATA_HOME: serialize against tests that redirect it.
    let _lock = TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let s = Session::new("/tmp/dex-name-default".into(), None).unwrap();
    let name = s.name().unwrap().to_string();
    assert!(name.starts_with("dex-name-default-"), "got: {name}");
    assert_eq!(name.rsplit_once('-').unwrap().1.len(), 7);
    // The generated name is persisted in the on-disk header.
    let path = s.path().unwrap().to_path_buf();
    let raw = std::fs::read_to_string(&path).unwrap();
    let header: SessionHeader = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
    assert_eq!(header.name(), Some(name.as_str()));
    // The id is `<workspace>-<16 chars>`: self-describing so the resume
    // hint and daemon logs say which workspace *name* a session belongs to.
    let id = s.id().to_string();
    let (id_slug, id_suffix) = id.rsplit_once('-').unwrap();
    assert_eq!(id_slug, "dex-name-default", "got: {id}");
    assert_eq!(id_suffix.len(), 16, "got: {id}");
    assert!(
        id.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "got: {id}"
    );
    // The same slug rules drive the name's workspace half, so id and name
    // agree on the workspace even though the suffixes are drawn separately.
    assert_eq!(id_slug, name.rsplit_once('-').unwrap().0);
    assert!(Session::new_id("/").starts_with("session-"));
    assert!(Session::new_id("").starts_with("session-"));
    assert_eq!(path.file_stem().and_then(|s| s.to_str()), Some(id.as_str()));
    let _ = std::fs::remove_file(&path);

    let s = Session::new("/tmp/dex-name-explicit".into(), Some("mine".into())).unwrap();
    assert_eq!(s.name(), Some("mine"));
    let path = s.path().unwrap().to_path_buf();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn session_ids_are_unique_within_a_workspace() {
    // The id is both the JSONL filename and the daemon registry key, and
    // the old epoch prefix is gone: distinctness rests entirely on the 16
    // random chars, so two sessions in one workspace must never collide.
    let ids: std::collections::HashSet<String> = (0..256)
        .map(|_| Session::new_id("/tmp/dex-unique-workspace"))
        .collect();
    assert_eq!(ids.len(), 256);
    assert_ne!(
        Session::new_id("/tmp/dex-unique-workspace"),
        Session::new_id("/tmp/dex-unique-workspace")
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn session_async_readers_match_sync() {
    // TDD Phase 6: async file streams / spawn_blocking, same wire format, same undo ledger.
    let _lock = TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("dex-sess-async-{}", std::process::id()));
    let _env = EnvGuard(vec![("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]);
    std::env::set_var("XDG_DATA_HOME", &dir);
    let mut s = Session::new("/tmp/async-cwd".into(), None).unwrap();
    let msg = ChatMessage::user("hello async");
    s.append_message(&msg).unwrap();
    let path = s.path().unwrap().to_path_buf();
    drop(s);
    let sync_msgs = load_messages_from_session(&path).unwrap();
    let async_msgs = load_messages_from_session_async(path.clone())
        .await
        .unwrap();
    assert_eq!(sync_msgs.len(), async_msgs.len());
    assert_eq!(sync_msgs[0].content, async_msgs[0].content);
    // list_all_async matches list_all (JoinSet, join, sort)
    let sync_list = Session::list_all().unwrap();
    let async_list = Session::list_all_async().await.unwrap();
    assert_eq!(sync_list.len(), async_list.len());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dangling_tool_call_is_synthesized_on_load() {
    // A crash between the assistant call and its result must not wedge
    // the next resume: providers reject an unanswered tool_call batch.
    let _lock = TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard(vec![("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]);
    let dir = std::env::temp_dir().join(format!("dex-sess-repair-{}", std::process::id()));
    std::env::set_var("XDG_DATA_HOME", &dir);
    let mut s = Session::new("/tmp/dex-repair-cwd".into(), None).unwrap();
    s.append_message(&ChatMessage::user("goal")).unwrap();
    s.append_message(&ChatMessage::assistant_calls(
        None,
        vec![crate::protocol::LlmToolCall {
            id: "call-1".into(),
            call_type: "function".into(),
            function: crate::protocol::FunctionCall {
                name: "edit".into(),
                arguments: r#"{"path":"a.rs"}"#.into(),
            },
        }],
    ))
    .unwrap();
    let path = s.path().unwrap().to_path_buf();
    drop(s);
    let messages = load_messages_from_session(&path).unwrap();
    assert_eq!(
        messages.len(),
        3,
        "assistant call must get a synthesized result"
    );
    let last = messages.last().unwrap();
    assert_eq!(last.role, crate::protocol::Role::Tool);
    assert_eq!(last.tool_call_id.as_deref(), Some("call-1"));
    assert!(
        last.content
            .as_deref()
            .unwrap()
            .contains("tool result missing"),
        "synthesized result must say what happened: {:?}",
        last.content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn complete_tool_batch_loads_without_synthesis() {
    let _lock = TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard(vec![("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]);
    let dir = std::env::temp_dir().join(format!("dex-sess-clean-{}", std::process::id()));
    std::env::set_var("XDG_DATA_HOME", &dir);
    let mut s = Session::new("/tmp/dex-clean-cwd".into(), None).unwrap();
    s.append_message(&ChatMessage::assistant_calls(
        None,
        vec![crate::protocol::LlmToolCall {
            id: "call-1".into(),
            call_type: "function".into(),
            function: crate::protocol::FunctionCall {
                name: "read".into(),
                arguments: "{}".into(),
            },
        }],
    ))
    .unwrap();
    s.append_message(&ChatMessage::tool_result("call-1", "contents"))
        .unwrap();
    let path = s.path().unwrap().to_path_buf();
    drop(s);
    let messages = load_messages_from_session(&path).unwrap();
    assert_eq!(messages.len(), 2, "no placeholder for a complete batch");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn dangling_middle_batch_repairs_in_place_not_at_end() {
    // A historic middle-batch gap gets its placeholder right after its
    // assistant message, preserving causality for the model.
    let mut messages = vec![
        ChatMessage::user("goal"),
        ChatMessage::assistant_calls(
            None,
            vec![crate::protocol::LlmToolCall {
                id: "mid-1".into(),
                call_type: "function".into(),
                function: crate::protocol::FunctionCall {
                    name: "read".into(),
                    arguments: "{}".into(),
                },
            }],
        ),
        ChatMessage::user("follow-up"),
    ];
    super::repair_dangling_tool_calls(&mut messages);
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[2].role, crate::protocol::Role::Tool);
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("mid-1"));
    assert_eq!(messages[3].content.as_deref(), Some("follow-up"));
}

#[test]
fn child_session_writes_its_own_file_with_markers() {
    // §16: the child's JSONL lands in `agents/` beside the parent file
    // with the same turn-marker discipline.
    let parent = Session::new("/tmp/dex-child-parent".into(), None).unwrap();
    let parent_path = parent.path().unwrap().to_path_buf();
    let mut child =
        Session::child(&parent_path, "/tmp/dex-child-parent", "p-0", "explorer", 0).unwrap();
    let child_path = child.path().unwrap().to_path_buf();
    assert_eq!(
        child_path.parent().unwrap(),
        parent_path.parent().unwrap().join("agents")
    );
    assert_eq!(
        child_path.file_name().and_then(|s| s.to_str()),
        Some("p-0-explorer.jsonl")
    );
    child.turn_event("turn_start").unwrap();
    child
        .append_message(&ChatMessage::user("child task"))
        .unwrap();
    child.turn_event("turn_complete").unwrap();
    // Marker discipline holds: a completed turn is not "interrupted".
    assert_eq!(Session::last_turn_state(&child_path), "complete");
    let messages = load_messages_from_session(&child_path).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content.as_deref(), Some("child task"));
    let _ = std::fs::remove_dir_all(parent_path.parent().unwrap());
}

#[test]
fn list_children_reports_runs_and_interrupted_state() {
    // Phase 8 exit: resume shows children and interrupted runs — every
    // run under `agents/` with its last turn state, `"interrupted"`
    // being a `turn_start` with no terminal marker (crashed or
    // daemon-restart-killed child). Loaders still ignore the directory.
    let _lock = TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let parent = Session::new("/tmp/dex-children-list".into(), None).unwrap();
    let parent_path = parent.path().unwrap().to_path_buf();
    drop(parent);
    let mut done =
        Session::child(&parent_path, "/tmp/dex-children-list", "c-0", "explorer", 0).unwrap();
    done.turn_event("turn_start").unwrap();
    done.turn_event("turn_complete").unwrap();
    let mut hung =
        Session::child(&parent_path, "/tmp/dex-children-list", "c-1", "tester", 0).unwrap();
    hung.turn_event("turn_start").unwrap();
    // A crash before a terminal marker leaves the run interrupted.
    drop(done);
    drop(hung);
    let runs = Session::list_children(&parent_path).unwrap();
    assert_eq!(runs.len(), 2);
    let state_of = |prefix: &str| {
        runs.iter()
            .find(|(_, header, _)| header.id().starts_with(prefix))
            .map(|(.., state)| *state)
    };
    assert_eq!(state_of("c-0"), Some("complete"));
    assert_eq!(state_of("c-1"), Some("interrupted"));
    // Headers keep the parent linkage (§22-N: parent/child recorded).
    assert!(runs
        .iter()
        .all(|(_, header, _)| header.name().is_some_and(|n| n.contains("(child of "))));
    let _ = std::fs::remove_dir_all(parent_path.parent().unwrap());
}

#[test]
fn child_generations_never_collide() {
    // §24.3: generation 0 keeps the V1 filename; resume generations
    // append `.g<N>` so a resume never clobbers its parent.
    let _lock = TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let parent = Session::new("/tmp/dex-supervision-gen".into(), None).unwrap();
    let parent_path = parent.path().unwrap().to_path_buf();
    drop(parent);
    let first = Session::child(
        &parent_path,
        "/tmp/dex-supervision-gen",
        "s-0",
        "explorer",
        0,
    )
    .unwrap();
    let second = Session::child(
        &parent_path,
        "/tmp/dex-supervision-gen",
        "s-0",
        "explorer",
        1,
    )
    .unwrap();
    let first_path = first.path().unwrap().to_path_buf();
    let second_path = second.path().unwrap().to_path_buf();
    assert_ne!(first_path, second_path);
    assert!(first_path.ends_with("agents/s-0-explorer.jsonl"));
    assert!(second_path.ends_with("agents/s-0-explorer.g1.jsonl"));
    assert_eq!(first.id(), "s-0-explorer");
    assert_eq!(second.id(), "s-0-explorer.g1");
    drop(first);
    drop(second);
    assert_eq!(
        Session::child_path(&parent_path, "s-0", "explorer", 0),
        first_path
    );
    assert_eq!(
        Session::child_path(&parent_path, "s-0", "explorer", 1),
        second_path
    );
    let _ = std::fs::remove_dir_all(parent_path.parent().unwrap());
}

#[test]
fn parent_listing_and_loader_ignore_child_sessions() {
    // §16 hard rule: `agents/*` never enters the parent transcript or
    // the session registry. Serialization: Session::new writes into the
    // shared sessions dir.
    let _lock = TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let parent = Session::new("/tmp/dex-ignore-child".into(), None).unwrap();
    let parent_path = parent.path().unwrap().to_path_buf();
    let mut child =
        Session::child(&parent_path, "/tmp/dex-ignore-child", "p-1", "tester", 0).unwrap();
    let child_path = child.path().unwrap().to_path_buf();
    child
        .append_message(&ChatMessage::user("child-only content"))
        .unwrap();
    // Listing reads only direct .jsonl files in each session directory.
    let listed = Session::list_all().unwrap();
    assert!(
        listed.iter().all(|(path, _)| path != &child_path),
        "child session must not be listed as a session"
    );
    assert!(
        listed.iter().any(|(path, _)| path == &parent_path),
        "the parent session itself stays listed"
    );
    // And the loaders take explicit paths: the parent's history has no
    // child content.
    let parent_messages = load_llm_messages_from_session(&parent_path).unwrap();
    assert!(parent_messages
        .iter()
        .all(|m| m.content.as_deref() != Some("child-only content")));
    let _ = std::fs::remove_dir_all(parent_path.parent().unwrap());
}
