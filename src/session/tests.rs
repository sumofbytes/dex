//! Tests for the plan helpers — these live on the app side because they
//! exercise the `Plan` composition over the raw state value.

use super::*;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
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
fn load_messages_and_plan_matches_separate_loads() {
    let path = unique_path("dex-messages-plan");
    let header = r#"{"type":"session","version":1,"id":"x","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#;
    let user = r#"{"type":"message","id":"1","timestamp":"2020-01-01T00:00:00Z","role":"user","content":"hi"}"#;
    let want = Plan {
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
    use crate::session::changes::{load_changes, make_change_record, record_change};
    use crate::test_env::TEST_SESSIONS_ENV_LOCK;
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
    use crate::session::changes::{make_change_record, record_change};
    use crate::test_env::TEST_SESSIONS_ENV_LOCK;
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
    // Simulate a concurrent edit: the content (and therefore its hash)
    // changed after the record was written, so undo must refuse.
    fs::write(&work, b"vX\n").unwrap();
    let err = undo_last_change(&mut s).unwrap_err();
    assert!(err.to_string().contains("refusing to undo"));
    let _ = fs::remove_file(&work);
    if let Some(p) = s.path() {
        let _ = fs::remove_file(p);
    }
}
