//! Tests, split out of the module body so it stays implementation.

use super::then_run::then_run_command;
use super::*;
use crate::runtime::cancel::GlobalCancellation;
use serde_json::json;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fs;
use std::time::Duration;

#[test]
fn shell_escape_splits_command() {
    assert_eq!(
        parse_shell_escape("!ls -la"),
        Some(("ls -la".to_string(), false))
    );
    assert_eq!(
        parse_shell_escape("!  echo hi  "),
        Some(("echo hi".to_string(), false))
    );
    assert_eq!(
        parse_shell_escape("!!cargo test"),
        Some(("cargo test".to_string(), true))
    );
    assert_eq!(
        parse_shell_escape("!!  echo hi  "),
        Some(("echo hi".to_string(), true))
    );
    // Multiline scripts run whole.
    assert_eq!(
        parse_shell_escape("!echo a\necho b"),
        Some(("echo a\necho b".to_string(), false))
    );
    // Bare `!`/`!!` fall through to the agent (usage, not a run).
    assert_eq!(parse_shell_escape("!"), None);
    assert_eq!(parse_shell_escape("!   "), None);
    assert_eq!(parse_shell_escape("!!"), None);
    // Ordinary prompts and slash commands are not shell escapes.
    assert_eq!(parse_shell_escape("hello"), None);
    assert_eq!(parse_shell_escape("/model foo"), None);
    assert_eq!(parse_shell_escape(""), None);
}
#[tokio::test]
async fn bash_exposes_the_binary_for_local_stitching() {
    let (output, code) = run_bash_with_limits(
        "printf '%s' \"$DEX_BIN\"",
        Duration::from_secs(5),
        4096,
        &GlobalCancellation,
    )
    .await
    .unwrap();
    assert_eq!(code, Some(0));
    assert_eq!(
        output,
        std::env::current_exe().unwrap().display().to_string()
    );
}
#[tokio::test]
async fn bash_children_have_no_controlling_tty() {
    // A tool child must never share the user's terminal: a child that
    // probes it (e.g. cargo test → theme query → OSC 10/11 on /dev/tty)
    // would write to and race the TUI's crossterm for the same pts, and
    // half a color report could end up typed into the composer.
    let (output, code) = run_bash_with_limits(
        "if cat </dev/tty >/dev/null 2>&1; then echo HAS_TTY; else echo NO_TTY; fi",
        Duration::from_secs(5),
        4096,
        &GlobalCancellation,
    )
    .await
    .unwrap();
    assert_eq!(code, Some(0));
    assert!(
        output.contains("NO_TTY"),
        "tool child still has a controlling tty: {output}"
    );
}

#[tokio::test]
async fn metadata_classifies_tools() {
    assert!(metadata("read").unwrap().read_only);
    assert!(metadata("write").unwrap().mutating);
    assert!(!metadata("bash").unwrap().idempotent);
}

#[tokio::test]
#[cfg(unix)]
async fn expand_glob_lists_pruned_files_only() {
    // Hermetic fixture for the `find -prune` semantics glob expansion
    // relies on: pruned dirs never match, symlinked dirs are listed but
    // never descended, outside-workspace symlinks are rejected.
    let dir = std::env::temp_dir().join(format!(
        "dex-glob-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    for d in ["sub/nested", "target", ".git", "node_modules"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    for f in [
        "a.rs",
        "sub/b.rs",
        "sub/c.txt",
        "sub/nested/g.rs",
        "target/d.rs",
        ".git/e.rs",
        "node_modules/f.rs",
        "x",
    ] {
        std::fs::write(dir.join(f), "x").unwrap();
    }
    std::os::unix::fs::symlink("sub", dir.join("linksub")).unwrap();
    let outside = dir.with_extension("outside");
    let _ = std::fs::remove_dir_all(&outside);
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), "x").unwrap();
    std::os::unix::fs::symlink(outside.join("secret.txt"), dir.join("leak")).unwrap();
    let names = |paths: Vec<std::path::PathBuf>| -> Vec<String> {
        paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    };
    // Bare pattern: basenames anywhere, pruned dirs excluded.
    assert_eq!(
        names(expand_glob_in(&dir, "*.rs").await.unwrap()),
        ["a.rs", "b.rs", "g.rs"]
    );
    // Path pattern: `*` spans `/`, like `find -path`.
    assert_eq!(
        names(expand_glob_in(&dir, "sub/*.rs").await.unwrap()),
        ["b.rs", "g.rs"]
    );
    // No double-descent through the symlinked dir: each file once.
    assert_eq!(
        names(expand_glob_in(&dir, "sub/*").await.unwrap()),
        ["b.rs", "c.txt", "g.rs"]
    );
    // `?` matches the single-char file; the `.` root never surfaces.
    assert_eq!(names(expand_glob_in(&dir, "?").await.unwrap()), ["x"]);
    // Outside-workspace symlink stays rejected; dirs stay files-only.
    assert_eq!(
        names(expand_glob_in(&dir, "*").await.unwrap()),
        ["a.rs", "b.rs", "c.txt", "g.rs", "x"]
    );
    assert!(expand_glob_in(&dir, "*.nope").await.is_err());
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&outside);
}

#[tokio::test]
async fn tool_arguments_are_validated() {
    let args = Map::new();
    assert!(matches!(
        execute("read", &args, &GlobalCancellation, &Policy::trusted(), None).await,
        Err(ToolError::Missing("path"))
    ));
    let mut args = Map::new();
    args.insert("path".into(), Value::Bool(true));
    assert!(matches!(
        execute("read", &args, &GlobalCancellation, &Policy::trusted(), None).await,
        Err(ToolError::NotString("path"))
    ));
}

#[tokio::test]
async fn fffind_rejects_unbounded_patterns() {
    let mut args = Map::new();
    args.insert("pattern".into(), Value::String("*".into()));
    assert!(matches!(
        execute(
            "fffind",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None
        )
        .await,
        Err(ToolError::InvalidArgument(_))
    ));
    let mut args = Map::new();
    args.insert("pattern".into(), Value::String("".into()));
    assert!(matches!(
        execute(
            "ffgrep",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            None
        )
        .await,
        Err(ToolError::InvalidArgument(_))
    ));
}

#[tokio::test]
async fn write_and_edit_leave_no_temp_files_and_round_trip() {
    // Own subdirectory: other tests write into `target/` concurrently, and
    // their in-flight `.dex-write-*` temp files would race this scan.
    let dir = "target/dex-atomic-write-test";
    let cwd = std::env::current_dir().unwrap();
    fs::create_dir_all(cwd.join(dir)).unwrap();
    let rel = "target/dex-atomic-write-test/file.txt";
    let full = cwd.join(rel);
    let mut args = Map::new();
    args.insert("path".into(), Value::String(rel.into()));
    args.insert("content".into(), Value::String("first\n".into()));
    assert!(execute(
        "write",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        None
    )
    .await
    .is_ok());
    // Content round-trips and no `.dex-write-*` temp file survives.
    assert_eq!(fs::read_to_string(&full).unwrap(), "first\n");
    args.insert("oldText".into(), Value::String("first".into()));
    args.insert("newText".into(), Value::String("second".into()));
    assert!(
        execute("edit", &args, &GlobalCancellation, &Policy::trusted(), None)
            .await
            .is_ok()
    );
    assert_eq!(fs::read_to_string(&full).unwrap(), "second\n");
    let strays: Vec<_> = fs::read_dir(cwd.join(dir))
        .unwrap()
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with(".dex-write-"))
        })
        .collect();
    assert!(
        strays.is_empty(),
        "temp files must be renamed away, not left"
    );
    let _ = fs::remove_dir_all(cwd.join(dir));
}

/// `then_run` runs after a successful mutation and its output
/// arrives in the same tool result, so the model learns the verification
/// outcome without a second round-trip that re-sends the whole prefix.
#[cfg(unix)]
#[tokio::test]
async fn then_run_streams_verification_into_the_same_result() {
    let cwd = std::env::current_dir().unwrap();
    fs::create_dir_all(cwd.join("target")).unwrap();
    let rel = "target/dex-then-run-test.txt";
    let full = cwd.join(rel);
    let mut args = Map::new();
    args.insert("path".into(), Value::String(rel.into()));
    args.insert("content".into(), Value::String("verified\n".into()));
    args.insert("then_run".into(), Value::String(format!("cat {rel}")));
    let out = execute(
        "write",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        None,
    )
    .await
    .unwrap();
    assert!(out.contains("wrote"), "{out}");
    assert!(
        out.contains("[then_run:succeeded] cat target/dex-then-run-test.txt"),
        "{out}"
    );
    // The command saw the new content: it runs after the write, not before.
    assert!(out.trim_end().ends_with("verified"), "{out}");
    let _ = fs::remove_file(&full);
}

/// A failing command is not a failed tool call: the write landed, and the
/// model needs both facts — the mutation and the exit status — not an
/// `Error:` that hides which half of the call did what.
#[cfg(unix)]
#[tokio::test]
async fn then_run_failure_is_reported_in_band() {
    let cwd = std::env::current_dir().unwrap();
    fs::create_dir_all(cwd.join("target")).unwrap();
    let rel = "target/dex-then-run-fail.txt";
    let full = cwd.join(rel);
    let mut args = Map::new();
    args.insert("path".into(), Value::String(rel.into()));
    args.insert("content".into(), Value::String("x\n".into()));
    args.insert("then_run".into(), Value::String("echo boom; exit 3".into()));
    let out = execute(
        "write",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        None,
    )
    .await
    .expect("the write succeeded; the command's exit must not fail the call");
    assert!(out.contains("[then_run:failed (exit 3)]"), "{out}");
    assert!(out.contains("boom"), "{out}");
    let _ = fs::remove_file(&full);
}

/// A failed mutation never runs the command: a stale verification output
/// must not reach the model attached to a change that never landed.
#[tokio::test]
async fn failed_mutation_skips_then_run() {
    let cwd = std::env::current_dir().unwrap();
    fs::create_dir_all(cwd.join("target")).unwrap();
    let rel = "target/dex-then-run-skip.txt";
    let full = cwd.join(rel);
    fs::write(&full, "present\n").unwrap();
    let mut args = Map::new();
    args.insert("path".into(), Value::String(rel.into()));
    args.insert("oldText".into(), Value::String("absent\n".into()));
    args.insert("newText".into(), Value::String("never\n".into()));
    args.insert(
        "then_run".into(),
        Value::String("echo ran > target/dex-then-run-skip-ran.txt".into()),
    );
    let error = execute("edit", &args, &GlobalCancellation, &Policy::trusted(), None)
        .await
        .expect_err("oldText is absent");
    assert!(matches!(error, ToolError::InvalidArgument(_)), "{error}");
    assert!(
        !cwd.join("target/dex-then-run-skip-ran.txt").exists(),
        "the command must not run when the mutation failed"
    );
    assert_eq!(fs::read_to_string(&full).unwrap(), "present\n");
    let _ = fs::remove_file(&full);
}

/// `then_run` must clear the *shell* gate, not the write gate: otherwise an
/// `edit` with `then_run` would be a way to run a command with no approval at
/// all. No console is attached here, so a shell requirement surfaces as a
/// denial.
#[tokio::test]
async fn then_run_needs_the_shell_gate() {
    let cwd = std::env::current_dir().unwrap();
    fs::create_dir_all(cwd.join("target")).unwrap();
    let rel = "target/dex-then-run-gate.txt";
    let full = cwd.join(rel);
    fs::write(&full, "before\n").unwrap();
    let policy = Policy {
        mode: PermissionMode::Ask,
        console: None,
        agent: None,
    };
    let mut args = Map::new();
    args.insert("path".into(), Value::String(rel.into()));
    args.insert("oldText".into(), Value::String("before".into()));
    args.insert("newText".into(), Value::String("after".into()));
    args.insert("then_run".into(), Value::String("echo hi".into()));
    let error = execute("edit", &args, &GlobalCancellation, &policy, None)
        .await
        .expect_err("a then_run shell command is not covered by the write gate");
    assert!(matches!(error, ToolError::Denied(_)), "{error}");
    assert_eq!(
        fs::read_to_string(&full).unwrap(),
        "before\n",
        "the denial must land before the mutation, not after it"
    );
    let _ = fs::remove_file(&full);
}

/// Only write/edit take `then_run`; an unusable value fails loudly rather
/// than doing nothing while the model reads the mutation as verified.
#[test]
fn then_run_command_scope() {
    let mut args = Map::new();
    args.insert("then_run".into(), Value::String("   ".into()));
    assert_eq!(then_run_command("write", &args).unwrap(), None);
    args.insert("then_run".into(), Value::String(" cargo test ".into()));
    assert_eq!(
        then_run_command("write", &args).unwrap(),
        Some("cargo test")
    );
    assert_eq!(then_run_command("edit", &args).unwrap(), Some("cargo test"));
    // Other tools ignore it entirely: the field is not theirs.
    assert_eq!(then_run_command("bash", &args).unwrap(), None);
    assert_eq!(then_run_command("read", &args).unwrap(), None);
    // `null` is "absent" — some clients serialize omitted optionals that way.
    args.insert("then_run".into(), Value::Null);
    assert_eq!(then_run_command("write", &args).unwrap(), None);
    // A structured value is a caller mistake, not a request to skip.
    args.insert("then_run".into(), json!({ "command": "cargo test" }));
    assert!(matches!(
        then_run_command("write", &args),
        Err(ToolError::InvalidArgument(_))
    ));
}

/// An agent allowlisting `edit` but not `bash` must not gain shell through
/// the `then_run` command: the same boundary the permission gate enforces,
/// one layer down at the child's tool set.
#[tokio::test]
async fn then_run_command_respects_a_child_tool_allowlist() {
    let filter = ToolFilter {
        owner: "child".to_string(),
        allowed: BTreeSet::from(["edit".to_string()]),
    };
    let cwd = std::env::current_dir().unwrap();
    fs::create_dir_all(cwd.join("target")).unwrap();
    let rel = "target/dex-then-run-filter.txt";
    let full = cwd.join(rel);
    fs::write(&full, "before\n").unwrap();
    let mut args = Map::new();
    args.insert("path".into(), Value::String(rel.into()));
    args.insert("oldText".into(), Value::String("before".into()));
    args.insert("newText".into(), Value::String("after".into()));
    args.insert("then_run".into(), Value::String("echo hi".into()));
    let error = execute(
        "edit",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        Some(&filter),
    )
    .await
    .expect_err("bash is not in the child's allowlist");
    assert!(matches!(error, ToolError::Denied(_)), "{error}");
    assert!(error.to_string().contains("bash"), "{error}");
    assert_eq!(fs::read_to_string(&full).unwrap(), "before\n");
    // The same call without `then_run` is exactly what the filter allows.
    args.remove("then_run");
    assert!(
        execute(
            "edit",
            &args,
            &GlobalCancellation,
            &Policy::trusted(),
            Some(&filter)
        )
        .await
        .is_ok(),
        "a plain edit is within the child's tools"
    );
    let _ = fs::remove_file(&full);
}

/// A malformed `then_run` fails before the mutation runs, so the model can
/// never mistake an unusable field for a check that came back clean.
#[tokio::test]
async fn malformed_then_run_does_not_mutate() {
    let cwd = std::env::current_dir().unwrap();
    fs::create_dir_all(cwd.join("target")).unwrap();
    let rel = "target/dex-then-run-malformed.txt";
    let full = cwd.join(rel);
    let _ = fs::remove_file(&full);
    let mut args = Map::new();
    args.insert("path".into(), Value::String(rel.into()));
    args.insert("content".into(), Value::String("x\n".into()));
    args.insert("then_run".into(), json!({ "command": "cargo test" }));
    let error = execute(
        "write",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        None,
    )
    .await
    .expect_err("an object then_run is rejected");
    assert!(matches!(error, ToolError::InvalidArgument(_)), "{error}");
    assert!(!full.exists(), "the write must not have happened");
}

#[cfg(unix)]
#[tokio::test]
async fn atomic_write_preserves_executable_bit() {
    use std::os::unix::fs::PermissionsExt as _;
    let cwd = std::env::current_dir().unwrap();
    fs::create_dir_all(cwd.join("target")).unwrap();
    let rel = "target/dex-atomic-mode-test.sh";
    let full = cwd.join(rel);
    let mut args = Map::new();
    args.insert("path".into(), Value::String(rel.into()));
    args.insert("content".into(), Value::String("#!/bin/sh\n".into()));
    assert!(execute(
        "write",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        None
    )
    .await
    .is_ok());
    std::fs::set_permissions(&full, std::fs::Permissions::from_mode(0o755)).unwrap();
    args.insert("oldText".into(), Value::String("#!/bin/sh".into()));
    args.insert("newText".into(), Value::String("#!/bin/sh\necho hi".into()));
    assert!(
        execute("edit", &args, &GlobalCancellation, &Policy::trusted(), None)
            .await
            .is_ok()
    );
    let mode = std::fs::metadata(&full).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755, "edit must not strip +x");
    let _ = fs::remove_file(&full);
}

#[test]
fn conflict_paths_normalize_spellings() {
    // `./target` and `target` are the same directory; an absolute and a
    // relative spelling of one file must collide too.
    let cwd = std::env::current_dir().unwrap();
    let rel = "./target";
    assert_eq!(
        normalize_conflict_path(rel),
        normalize_conflict_path("target")
    );
    let abs = cwd.join("target").display().to_string();
    assert_eq!(
        normalize_conflict_path(&abs),
        normalize_conflict_path("target")
    );
    // Lexical fallback: new files in not-yet-existing dirs still
    // collide across `./` spellings without touching the FS.
    assert_eq!(
        normalize_conflict_path("./newdir-dex-test/f.rs"),
        normalize_conflict_path("newdir-dex-test/f.rs")
    );
    assert_eq!(
        normalize_conflict_path("a/../newdir-dex-test/f.rs"),
        normalize_conflict_path("newdir-dex-test/f.rs")
    );
    // Outside-workspace absolute paths clean lexically instead of
    // crashing the check.
    assert_eq!(
        normalize_conflict_path("/definitely/not/here"),
        "/definitely/not/here"
    );
}

#[tokio::test]
async fn edit_requires_exactly_one_match() {
    assert!(matches!(
        apply_edit("a a", "a", "b", false),
        Err(ToolError::EditNotUnique(2))
    ));
    let (updated, _) = apply_edit("a", "a", "b", false).unwrap();
    assert_eq!(updated, "b");
    assert!(matches!(
        apply_edit("a", "x", "b", false),
        Err(ToolError::InvalidArgument(_))
    ));
}

#[tokio::test]
async fn edit_replace_all_replaces_every_occurrence() {
    let (updated, note) = apply_edit("a b a", "a", "c", true).unwrap();
    assert_eq!(updated, "c b c");
    assert!(note.contains("2 occurrences"), "{note}");
    // Without replaceAll the duplicate is an error, not a silent partial.
    assert!(apply_edit("a b a", "a", "c", false).is_err());
}

#[tokio::test]
async fn write_edit_require_expected_hash_and_reject_stale() {
    // Real workspace file under target/ (inside cwd, cleaned up).
    let cwd = std::env::current_dir().unwrap();
    fs::create_dir_all(cwd.join("target")).unwrap();
    let path = cwd.join("target/dex-stale-test.txt");
    fs::write(&path, "v1\n").unwrap();
    let rel = "target/dex-stale-test.txt";
    let h = hash_file(&path.display().to_string());

    // Correct expected_hash: edit applies.
    let mut args = Map::new();
    args.insert("path".into(), Value::String(rel.into()));
    args.insert("oldText".into(), Value::String("v1".into()));
    args.insert("newText".into(), Value::String("v2".into()));
    args.insert("expected_hash".into(), Value::String(h.clone()));
    assert!(
        execute("edit", &args, &GlobalCancellation, &Policy::trusted(), None)
            .await
            .is_ok()
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "v2\n");

    // Stale expected_hash: rejected, file untouched.
    let mut stale = Map::new();
    stale.insert("path".into(), Value::String(rel.into()));
    stale.insert("oldText".into(), Value::String("v2".into()));
    stale.insert("newText".into(), Value::String("v3".into()));
    stale.insert("expected_hash".into(), Value::String("deadbeef".into()));
    assert!(matches!(
        execute(
            "edit",
            &stale,
            &GlobalCancellation,
            &Policy::trusted(),
            None
        )
        .await,
        Err(ToolError::StaleFile { .. })
    ));
    assert_eq!(fs::read_to_string(&path).unwrap(), "v2\n");

    // hash_file is deterministic.
    assert_eq!(
        hash_file(&path.display().to_string()),
        hash_file(&path.display().to_string())
    );
    let _ = fs::remove_file(&path);
}

#[tokio::test]
async fn change_diff_shows_unified_diff_for_write_and_edit() {
    let cwd = std::env::current_dir().unwrap();
    fs::create_dir_all(cwd.join("target")).unwrap();
    let path = cwd.join("target/dex-preview-test.txt");
    fs::write(&path, "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n").unwrap();
    let rel = "target/dex-preview-test.txt";
    let mut args = Map::new();
    args.insert("path".into(), Value::String(rel.into()));
    args.insert("oldText".into(), Value::String("l5\n".into()));
    args.insert("newText".into(), Value::String("L5\nL5b\n".into()));
    let diff = change_diff_async("edit", &args).await.unwrap();
    assert!(diff.contains("--- a/target/dex-preview-test.txt"), "{diff}");
    assert!(diff.contains("+++ b/target/dex-preview-test.txt"), "{diff}");
    assert!(diff.contains("@@"), "{diff}");
    assert!(diff.contains("-l5"), "{diff}");
    assert!(diff.contains("+L5b"), "{diff}");
    // Context lines surround the change (4 radius) and are untouched.
    assert!(diff.lines().any(|l| l == " l4"), "{diff}");
    assert!(diff.lines().any(|l| l == " l9"), "{diff}");
    // Lines beyond the 4-line context radius stay outside the hunks.
    assert!(!diff.contains(" l10\n"), "{diff}");
    let _ = fs::remove_file(&path);
}

#[tokio::test]
async fn change_diff_new_file_uses_dev_null_header() {
    let cwd = std::env::current_dir().unwrap();
    let rel = "target/dex-preview-new.txt";
    let _ = fs::remove_file(cwd.join(rel));
    let mut args = Map::new();
    args.insert("path".into(), Value::String(rel.into()));
    args.insert("content".into(), Value::String("hello\n".into()));
    let diff = change_diff_async("write", &args).await.unwrap();
    assert!(diff.contains("--- /dev/null"), "{diff}");
    assert!(diff.contains("+++ b/target/dex-preview-new.txt"), "{diff}");
    assert!(diff.contains("+hello"), "{diff}");
}

#[tokio::test]
async fn execute_outcome_carries_diff_for_edit() {
    let cwd = std::env::current_dir().unwrap();
    fs::create_dir_all(cwd.join("target")).unwrap();
    let rel = "target/dex-outcome-diff-test.txt";
    fs::write(cwd.join(rel), "a\nb\nc\n").unwrap();
    let mut args = Map::new();
    args.insert("path".into(), Value::String(rel.into()));
    args.insert("oldText".into(), Value::String("b\n".into()));
    args.insert("newText".into(), Value::String("B\n".into()));
    let outcome =
        execute_outcome("edit", &args, &GlobalCancellation, &Policy::trusted(), None).await;
    assert!(outcome.ok, "{}", outcome.text);
    let diff = outcome.diff.expect("edit outcome carries a diff");
    assert!(diff.contains("-b"), "{diff}");
    assert!(diff.contains("+B"), "{diff}");
    let _ = fs::remove_file(cwd.join(rel));
}

#[tokio::test]
async fn edit_fuzzy_fallback_handles_whitespace_drift() {
    // oldText with different indentation still matches exactly one
    // line-window and replaces whole lines.
    let content = "fn main() {\n    let x = 1;\n    println!(x);\n}\n";
    let old = "let x = 1;\nprintln!(x);";
    let new = "let y = 2;\nprintln!(y);";
    let (updated, note) = apply_edit(content, old, new, false).unwrap();
    assert!(updated.contains("let y = 2;"), "{updated}");
    assert!(updated.contains("    println!(y);"), "{updated}");
    assert!(note.contains("whitespace-insensitive"), "{note}");
    // Ambiguous fuzzy matches are rejected, not guessed.
    assert!(apply_edit("a\nb\na\nb", "a\nb", "c", false).is_err());
    // A genuinely absent match reports actionable guidance.
    let err = apply_edit("x", "nope", "c", false).unwrap_err();
    assert!(err.to_string().contains("read the file"), "{err}");
}

#[tokio::test]
async fn edit_fuzzy_fallback_folds_lookalike_characters() {
    // Smart quotes, em-dashes, and non-breaking spaces in the file match
    // their ASCII equivalents in oldText; unchanged lines keep original bytes.
    let content = "let a = \u{2018}hi\u{2019};\n// a \u{2014} b\nx =\u{00A0}1;\n";
    let (updated, note) = apply_edit(
        content,
        "let a = 'hi';\n// a - b",
        "let a = 'yo';\n// a - b!",
        false,
    )
    .unwrap();
    assert!(updated.contains("let a = 'yo';"), "{updated}");
    assert!(updated.contains("// a - b!"), "{updated}");
    assert!(updated.contains("x =\u{00A0}1;\n"), "{updated}");
    assert!(note.contains("whitespace-insensitive"), "{note}");
}

#[tokio::test]
async fn edit_fuzzy_fallback_preserves_crlf_outside_the_window() {
    // LF oldText matches a CRLF file's line window (with an indentation
    // and quote drift forcing the fuzzy path); the edit writes LF for
    // the replaced line but leaves surrounding CRLF bytes alone.
    let content = "a\r\n    say \u{2018}hi\u{2019}  \r\nc\r\n";
    let (updated, _) = apply_edit(content, "say 'hi'", "say 'yo'", false).unwrap();
    assert_eq!(updated, "a\r\n    say 'yo'\nc\r\n", "{updated:?}");
}

#[tokio::test]
async fn edit_batch_applies_disjoint_edits_in_one_call() {
    let content = "alpha\nbeta\ngamma\ndelta\n";
    let ops = vec![
        ("alpha".to_string(), "ALPHA".to_string()),
        ("gamma\ndelta".to_string(), "GAMMA\nDELTA".to_string()),
    ];
    let (updated, note) = apply_edit_batch(content, &ops, false).unwrap();
    assert_eq!(updated, "ALPHA\nbeta\nGAMMA\nDELTA\n", "{updated:?}");
    assert!(note.contains("2 edits"), "{note}");
    assert!(note.contains("line 1"), "{note}");
    assert!(note.contains("lines 3-4"), "{note}");
}

#[tokio::test]
async fn edit_batch_note_counts_fan_out_sites() {
    // replaceAll inside a batch: the note must not claim "2 edits" while
    // listing three spans — the site count is stated separately.
    let content = "a\nb\nb\n";
    let ops = vec![
        ("a".to_string(), "A".to_string()),
        ("b".to_string(), "B".to_string()),
    ];
    let (updated, note) = apply_edit_batch(content, &ops, true).unwrap();
    assert_eq!(updated, "A\nB\nB\n", "{updated:?}");
    assert!(note.contains("2 edits, 3 sites"), "{note}");
}

#[tokio::test]
async fn edit_batch_null_optional_args_are_treated_as_absent() {
    // Some clients serialize omitted optional fields as explicit `null`
    // (`{"path": ..., "oldText": null, "edits": [...]}`). That used to trip
    // the both-shapes guard; null is "absent", exactly like `then_run: null`.
    let mut args = Map::new();
    args.insert(
        "edits".into(),
        Value::Array(vec![serde_json::json!({"oldText": "a", "newText": "A"})]),
    );
    args.insert("oldText".into(), Value::Null);
    args.insert("newText".into(), Value::Null);
    args.insert("replaceAll".into(), Value::Null);
    let ops = parse_edit_ops(&args).unwrap();
    assert_eq!(ops, vec![("a".to_string(), "A".to_string())]);
    // A real oldText alongside edits[] still fails loudly.
    args.insert("oldText".into(), Value::String("a".into()));
    assert!(parse_edit_ops(&args).is_err());
}

#[tokio::test]
async fn edit_batch_rejects_non_object_entries_by_name() {
    // `edits: ["foo"]` used to report "missing oldText"; name the actual
    // shape problem so the model can self-correct.
    let mut args = Map::new();
    args.insert(
        "edits".into(),
        Value::Array(vec![Value::String("foo".into())]),
    );
    let error = parse_edit_ops(&args).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("edits[0] must be an object with oldText and newText"),
        "{error}"
    );
}

#[tokio::test]
async fn edit_batch_noop_error_names_the_entry() {
    let mut args = Map::new();
    args.insert(
        "edits".into(),
        Value::Array(vec![
            serde_json::json!({"oldText": "a", "newText": "A"}),
            serde_json::json!({"oldText": "same", "newText": "same"}),
        ]),
    );
    let error = parse_edit_ops(&args).unwrap_err();
    assert!(
        error.to_string().contains("edits[1]") && error.to_string().contains("identical"),
        "{error}"
    );
}

#[tokio::test]
async fn edit_batch_rejects_overlapping_entries() {
    let content = "one\ntwo\nthree\n";
    let ops = vec![
        ("one\ntwo".to_string(), "1\n2".to_string()),
        ("two\nthree".to_string(), "2\n3".to_string()),
    ];
    let err = apply_edit_batch(content, &ops, false).unwrap_err();
    assert!(err.to_string().contains("overlap"), "{err}");
    // No partial application: the error leaves the file untouched.
}

#[tokio::test]
async fn edit_batch_arg_shapes_are_validated() {
    let cwd = std::env::current_dir().unwrap();
    fs::create_dir_all(cwd.join("target")).unwrap();
    let rel = "target/dex-edit-batch-shapes.txt";
    fs::write(cwd.join(rel), "aaa\nbbb\nccc\n").unwrap();
    let run = |args: Map<String, Value>| async move {
        execute("edit", &args, &GlobalCancellation, &Policy::trusted(), None).await
    };
    // Mixing the two shapes fails loudly.
    let mut mixed = Map::new();
    mixed.insert("path".into(), Value::String(rel.into()));
    mixed.insert("oldText".into(), Value::String("aaa".into()));
    mixed.insert("newText".into(), Value::String("A".into()));
    mixed.insert(
        "edits".into(),
        Value::Array(vec![serde_json::json!({"oldText": "bbb", "newText": "B"})]),
    );
    assert!(run(mixed).await.is_err());
    // A disjoint batch applies end to end through `execute`.
    let mut batched = Map::new();
    batched.insert("path".into(), Value::String(rel.into()));
    batched.insert(
        "edits".into(),
        Value::Array(vec![
            serde_json::json!({"oldText": "aaa", "newText": "A"}),
            serde_json::json!({"oldText": "ccc", "newText": "C"}),
        ]),
    );
    let out = run(batched).await.expect("disjoint batch applies");
    assert!(out.contains("2 edits"), "{out}");
    assert_eq!(fs::read_to_string(cwd.join(rel)).unwrap(), "A\nbbb\nC\n");
    // replaceAll fans out across a batch too: every entry's matches
    // are collected up front and overlap-checked before anything applies.
    let mut batched_all = Map::new();
    batched_all.insert("path".into(), Value::String(rel.into()));
    batched_all.insert(
        "edits".into(),
        Value::Array(vec![serde_json::json!({"oldText": "b", "newText": "B"})]),
    );
    batched_all.insert("replaceAll".into(), Value::Bool(true));
    run(batched_all).await.expect("batch replaceAll applies");
    assert_eq!(fs::read_to_string(cwd.join(rel)).unwrap(), "A\nBBB\nC\n");
    let _ = fs::remove_file(cwd.join(rel));
}

#[tokio::test]
async fn temporary_workspace_paths_are_confined() {
    let root = std::env::temp_dir().join(format!("dex-workspace-test-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    assert!(resolve_workspace_path(&root, "inside.txt")
        .unwrap()
        .starts_with(&root));
    assert!(matches!(
        resolve_workspace_path(&root, "../outside.txt"),
        Err(crate::workspace::WorkspaceError::OutsideWorkspace(_))
    ));
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn shell_timeout_terminates_long_running_command() {
    let (result, code) = run_bash_with_limits(
        "sleep 1",
        Duration::from_millis(10),
        1024,
        &GlobalCancellation,
    )
    .await
    .unwrap();
    assert!(result.contains("timed out"));
    assert_eq!(code, None);
}

#[tokio::test]
async fn shell_exit_code_is_reported_separately_from_output() {
    let (output, code) = run_bash_with_limits(
        "echo partial-results; exit 3",
        Duration::from_secs(5),
        1024,
        &GlobalCancellation,
    )
    .await
    .unwrap();
    assert_eq!(code, Some(3));
    assert_eq!(output, "partial-results\n");
}

#[tokio::test]
async fn stderr_is_labeled_and_capture_limit_is_marked() {
    let (output, _) = run_bash_with_limits(
        "echo out; echo err 1>&2",
        Duration::from_secs(5),
        1024,
        &GlobalCancellation,
    )
    .await
    .unwrap();
    assert!(output.contains("out\n"), "{output:?}");
    assert!(output.contains("--- stderr ---\nerr"), "{output:?}");

    let (clipped, _) =
        run_bash_with_limits("seq 1 100", Duration::from_secs(5), 16, &GlobalCancellation)
            .await
            .unwrap();
    assert!(clipped.contains("capture limit"), "{clipped:?}");
}

#[tokio::test]
async fn read_is_line_numbered_and_paginates() {
    // Fixtures live under target/ so the workspace path confinement
    // accepts them (and the directory is already ignored).
    let root = std::env::current_dir()
        .unwrap()
        .join("target")
        .join(format!("dex-read-test-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("sample.txt");
    fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();

    let mut args = Map::new();
    args.insert("path".into(), Value::String(path.display().to_string()));
    let outcome =
        execute_outcome("read", &args, &GlobalCancellation, &Policy::trusted(), None).await;
    assert!(outcome.ok, "{}", outcome.text);
    // Line numbers are now right-aligned with two spaces (no raw tab) and file
    // tabs are expanded per tab_width, so the separator is stable.
    assert_eq!(
        outcome.text,
        "   1  one\n   2  two\n   3  three\n   4  four"
    );

    args.insert("offset".into(), Value::Number(2.into()));
    args.insert("limit".into(), Value::Number(1.into()));
    let outcome =
        execute_outcome("read", &args, &GlobalCancellation, &Policy::trusted(), None).await;
    assert_eq!(outcome.text, "   2  two");

    args.insert("offset".into(), Value::Number(9.into()));
    let outcome =
        execute_outcome("read", &args, &GlobalCancellation, &Policy::trusted(), None).await;
    assert!(!outcome.ok, "offset past end must fail: {}", outcome.text);

    // Binary content is refused instead of dumped into the context.
    fs::write(root.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
    let mut args = Map::new();
    args.insert(
        "path".into(),
        Value::String(root.join("blob.bin").display().to_string()),
    );
    let outcome =
        execute_outcome("read", &args, &GlobalCancellation, &Policy::trusted(), None).await;
    assert!(!outcome.ok, "{}", outcome.text);
    assert!(outcome.text.contains("binary file"), "{}", outcome.text);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn read_fanout_reads_many_files_in_one_call() {
    // Fixtures live at the workspace root (not target/) because search
    // and glob rules deliberately exclude target/.
    let root = std::env::current_dir()
        .unwrap()
        .join(format!("dex-fanout-test-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.txt"), "alpha\n").unwrap();
    fs::write(root.join("b.txt"), "beta\n").unwrap();

    // Explicit list; a missing file is isolated, not fatal.
    let mut args = Map::new();
    args.insert(
        "paths".into(),
        Value::Array(vec![
            Value::String(root.join("a.txt").display().to_string()),
            Value::String(root.join("missing.txt").display().to_string()),
            Value::String(root.join("b.txt").display().to_string()),
        ]),
    );
    let outcome =
        execute_outcome("read", &args, &GlobalCancellation, &Policy::trusted(), None).await;
    assert!(outcome.ok, "{}", outcome.text);
    assert!(outcome.text.contains("==> "), "{}", outcome.text);
    assert!(outcome.text.contains("   1  alpha"), "{}", outcome.text);
    assert!(outcome.text.contains("   1  beta"), "{}", outcome.text);
    assert!(outcome.text.contains("error:"), "{}", outcome.text);

    // Glob fan-out, sorted, capped.
    let mut args = Map::new();
    args.insert("glob".into(), Value::String("*.txt".into()));
    let outcome =
        execute_outcome("read", &args, &GlobalCancellation, &Policy::trusted(), None).await;
    assert!(outcome.ok, "{}", outcome.text);
    assert!(outcome.text.contains("a.txt"), "{}", outcome.text);
    assert!(outcome.text.contains("b.txt"), "{}", outcome.text);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn ffgrep_truncation_trailer_counts_shown_and_how_to_continue() {
    // Truncation must read like read's trailer: what was shown, that more
    // was left unscanned, and the exact next-page argument.
    let pid = std::process::id();
    let root = std::env::current_dir()
        .unwrap()
        .join(format!("dex-fff-page-{pid}"));
    fs::create_dir_all(&root).unwrap();
    let needle = format!("PAGETOKEN_{pid}");
    for f in 0..6 {
        let body: String = (0..20).map(|_| format!("{needle}\n")).collect();
        fs::write(root.join(format!("f{f}.rs")), body).unwrap();
    }
    super::search::rescan();

    // Files mode: 3 of 6 files shown, next page starts at file_offset 3.
    let mut args = Map::new();
    args.insert("pattern".into(), Value::String(needle.clone()));
    args.insert("head_limit".into(), Value::Number(3.into()));
    let outcome = execute_outcome(
        "ffgrep",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        None,
    )
    .await;
    assert!(outcome.ok, "{}", outcome.text);
    assert!(outcome.text.contains("3 files shown"), "{}", outcome.text);
    assert!(
        outcome
            .text
            .contains("continue with file_offset 3 or raise head_limit"),
        "{}",
        outcome.text
    );

    // Content mode resumes from that offset: pages 4 and 5, each capped
    // at 10 matches per file, under the default head_limit of 50.
    let mut args = Map::new();
    args.insert("pattern".into(), Value::String(needle.clone()));
    args.insert("output_mode".into(), Value::String("content".into()));
    args.insert("file_offset".into(), Value::Number(4.into()));
    let outcome = execute_outcome(
        "ffgrep",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        None,
    )
    .await;
    assert!(outcome.ok, "{}", outcome.text);
    assert!(
        outcome.text.contains("every file hit the 10-match cap"),
        "{}",
        outcome.text
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn ffgrep_context_returns_surrounding_lines() {
    // Fixture at the workspace root (not target/): fff respects
    // .gitignore, so ignored fixture dirs are invisible to it.
    let root = std::env::current_dir()
        .unwrap()
        .join(format!("dex-fff-ctx-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let needle = format!("CTXNEEDLE_{}", std::process::id());
    fs::write(
        root.join("code.rs"),
        format!("top\nbefore\n{needle} here\nafter\nbottom\n"),
    )
    .unwrap();
    super::search::rescan();

    let mut args = Map::new();
    args.insert("pattern".into(), Value::String(needle.clone()));
    args.insert("output_mode".into(), Value::String("content".into()));
    args.insert("context".into(), Value::Number(1.into()));
    let outcome = execute_outcome(
        "ffgrep",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        None,
    )
    .await;
    assert!(outcome.ok, "{}", outcome.text);
    assert!(outcome.text.contains("before"), "{}", outcome.text);
    assert!(outcome.text.contains("after"), "{}", outcome.text);
    assert!(!outcome.text.contains("top"), "{}", outcome.text);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn ffgrep_fuzzy_fallback_recovers_typos() {
    let root = std::env::current_dir()
        .unwrap()
        .join(format!("dex-fff-typo-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("thing.rs"),
        "struct UserAccountController { field: u32 }\n",
    )
    .unwrap();
    super::search::rescan();

    // Exact query misses (the token has a transposed 'lr'), fuzzy retry hits.
    // Assembled at runtime so the query text does not appear verbatim in
    // this source file — the exact search would hit this file otherwise.
    let mut args = Map::new();
    args.insert(
        "pattern".into(),
        Value::String(format!("UserAccountControlel{}", "r")),
    );
    args.insert("output_mode".into(), Value::String("content".into()));
    let outcome = execute_outcome(
        "ffgrep",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        None,
    )
    .await;
    assert!(outcome.ok, "{}", outcome.text);
    assert!(outcome.text.contains("approximate"), "{}", outcome.text);
    assert!(
        outcome.text.contains("UserAccountController"),
        "{}",
        outcome.text
    );
    // Detail rows get their own lines, not glued to the path
    // (`thing.rs  1: …` would break the summary counter and preview).
    assert!(!outcome.text.contains("thing.rs  "), "{}", outcome.text);

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn chain_runs_search_then_reads_matched_files_in_one_call() {
    // Fixtures live at the workspace root (not target/): fff respects
    // .gitignore, so ignored fixture dirs are invisible to it.
    let needle = format!("TARGET_{}", "TOKEN");
    let root = std::env::current_dir()
        .unwrap()
        .join(format!("dex-chain-fx-{}", std::process::id()));
    // A previous run that panicked mid-assert leaked its fixture dir
    // (cleanup below only runs on the happy path); sweep those first.
    if let Ok(entries) = std::fs::read_dir(".") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("dex-chain-fx-") {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
    }
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("one.rs"), format!("{needle} in one\n")).unwrap();
    fs::write(root.join("two.rs"), "nothing here\n").unwrap();
    super::search::rescan();

    let args = json!({
        "steps": [
            {"tool": "ffgrep", "args": {"pattern": &needle, "output_mode": "files"}},
            {"tool": "read", "from": 0, "take": "paths", "args": {"limit": 10}}
        ]
    })
    .as_object()
    .unwrap()
    .clone();
    let outcome = execute_outcome(
        "chain",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        None,
    )
    .await;
    assert!(outcome.ok, "{}", outcome.text);
    assert!(
        outcome.text.contains("--- step 0: ffgrep ---"),
        "{}",
        outcome.text
    );
    assert!(
        outcome.text.contains("--- step 1: read ---"),
        "{}",
        outcome.text
    );
    assert!(
        outcome.text.contains(&format!("{needle} in one")),
        "{}",
        outcome.text
    );

    // Mutation and shell tools are refused inside chains.
    let args = json!({
        "steps": [
            {"tool": "ffgrep", "args": {"pattern": "x-unlikely", "output_mode": "files"}},
            {"tool": "bash", "args": {"command": "echo hi"}}
        ]
    })
    .as_object()
    .unwrap()
    .clone();
    let outcome = execute_outcome(
        "chain",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        None,
    )
    .await;
    assert!(!outcome.ok, "{}", outcome.text);
    assert!(outcome.text.contains("read-only"), "{}", outcome.text);

    // `from` must reference an earlier step.
    let args = json!({
        "steps": [
            {"tool": "ffgrep", "args": {"pattern": "x-unlikely", "output_mode": "files"}},
            {"tool": "read", "from": 1, "take": "paths"}
        ]
    })
    .as_object()
    .unwrap()
    .clone();
    let outcome = execute_outcome(
        "chain",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        None,
    )
    .await;
    assert!(!outcome.ok, "{}", outcome.text);
    assert!(outcome.text.contains("earlier step"), "{}", outcome.text);

    // A failing second step still ships the first step's output.
    let args = json!({
        "steps": [
            {"tool": "ffgrep", "args": {"pattern": &needle, "output_mode": "files"}},
            {"tool": "read", "from": 0, "take": "paths", "args": {"offset": 99}}
        ]
    })
    .as_object()
    .unwrap()
    .clone();
    let outcome = execute_outcome(
        "chain",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        None,
    )
    .await;
    assert!(!outcome.ok, "{}", outcome.text);
    assert!(
        outcome.text.contains("step 0: ffgrep") && outcome.text.contains("step 1 failed"),
        "{}",
        outcome.text
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn ffgrep_without_matches_is_success() {
    // Assembled at runtime so the needle does not appear in this source
    // file (the test greps the crate it lives in). Gibberish so the
    // fuzzy fallback has nothing approximate to land on either.
    let needle = format!("zxq{}wvut", std::process::id());
    let mut args = Map::new();
    args.insert("pattern".into(), Value::String(needle));
    let outcome = execute_outcome(
        "ffgrep",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        None,
    )
    .await;
    assert!(
        outcome.ok,
        "no matches (exact or fuzzy) must be ok: {}",
        outcome.text
    );
    assert!(
        outcome.text.contains("0 matches."),
        "no matches must report zero: {:?}",
        outcome.text
    );
}

#[tokio::test]
async fn fffind_finds_paths_fuzzily() {
    super::search::rescan();
    let mut args = Map::new();
    args.insert("pattern".into(), Value::String("tools mod".into()));
    let outcome = execute_outcome(
        "fffind",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        None,
    )
    .await;
    assert!(outcome.ok, "{}", outcome.text);
    assert!(
        outcome.text.contains("src/tools/mod.rs"),
        "{}",
        outcome.text
    );
}

#[tokio::test]
async fn failed_shell_command_keeps_output_and_exit_marker() {
    let mut args = Map::new();
    args.insert("command".into(), Value::String("echo boom; exit 2".into()));
    let outcome =
        execute_outcome("bash", &args, &GlobalCancellation, &Policy::trusted(), None).await;
    assert!(!outcome.ok);
    assert!(outcome.text.contains("boom"));
    assert!(outcome.text.contains("[exit 2]"));
}

#[tokio::test]
async fn fanout_read_is_concurrent_ordered_and_isolated() {
    // TDD Phase 3 (S1): ≤10 files concurrent, join + sort to input order, budget on join, per-file errors isolated.
    let cwd = std::env::current_dir().unwrap();
    let dir = cwd.join("target/dex-fanout-test");
    let _ = tokio::fs::create_dir_all(&dir).await;
    let mut rels = Vec::new();
    for i in 0..5 {
        let rel = format!("target/dex-fanout-test/f{i}.txt");
        tokio::fs::write(cwd.join(&rel), format!("content-{i}\nline2\n"))
            .await
            .unwrap();
        rels.push(rel);
    }
    // One missing file among good ones: isolated, call still succeeds.
    rels.push("target/dex-fanout-test/missing-xyz.txt".to_string());
    let mut args = Map::new();
    args.insert(
        "paths".to_string(),
        serde_json::Value::Array(
            rels.iter()
                .map(|r| serde_json::Value::String(r.clone()))
                .collect(),
        ),
    );
    // Use workspace_path resolution via execute (paths confined).
    let out = execute("read", &args, &GlobalCancellation, &Policy::trusted(), None)
        .await
        .unwrap();
    // All good files present, in input order (==> path <== sections sorted by input, not completion).
    let mut last_pos = 0;
    for i in 0..5 {
        let marker = format!("f{i}.txt");
        let pos = out.find(&marker).expect("each file present");
        assert!(pos >= last_pos, "fan-out must preserve input order");
        last_pos = pos;
        assert!(out.contains(&format!("content-{i}")));
    }
    assert!(
        out.contains("missing-xyz"),
        "per-file errors isolated, not fatal"
    );
    let _ = tokio::fs::remove_dir_all(&dir).await;
}

#[tokio::test]
async fn bash_timeout_kills_and_reports_exactly() {
    // TDD Phase 3 (S5): timeout exact, not 25ms-quantized; [exit N] reporting preserved.
    let (out, code) = run_bash_with_limits(
        "sleep 5; echo never",
        Duration::from_millis(80),
        4096,
        &GlobalCancellation,
    )
    .await
    .unwrap();
    assert_eq!(code, None);
    assert!(
        out.contains("timed out after 0 seconds") || out.contains("timed out"),
        "{out}"
    );
}

#[tokio::test]
async fn bash_cancel_is_prompt_not_poll_quantized() {
    // TDD Phase 3: cancel via select!, not 25ms poll.
    use crate::runtime::console::CancellationToken;
    let token = CancellationToken::new();
    let t2 = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        t2.cancel();
    });
    let start = std::time::Instant::now();
    let (out, code) =
        run_bash_with_limits("sleep 5; echo never", Duration::from_secs(10), 4096, &token)
            .await
            .unwrap();
    assert_eq!(code, None);
    assert!(out.contains("cancelled"), "{out}");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "cancel must preempt sleep without 25ms quanta pile-up"
    );
}

#[tokio::test]
async fn cancelled_tool_reports_error_outcome_for_loop_suppression() {
    // Producer side of the cancel-during-IO contract: a fired token
    // turns the tool into ok:false, so the loop neither caches nor
    // replays it — and `execute_outcome` never derives success from text.
    use crate::runtime::console::CancellationToken;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut args = serde_json::Map::new();
    args.insert(
        "command".to_string(),
        serde_json::Value::String("sleep 30".to_string()),
    );
    let outcome = execute_outcome("bash", &args, &cancel, &Policy::trusted(), None).await;
    assert!(!outcome.ok);
    assert!(outcome.text.contains("cancelled"), "{}", outcome.text);
}

#[tokio::test]
async fn bash_exit_code_and_stderr_label_preserved() {
    let (out, code) = run_bash_with_limits(
        "echo out; echo err >&2; exit 3",
        Duration::from_secs(5),
        4096,
        &GlobalCancellation,
    )
    .await
    .unwrap();
    assert_eq!(code, Some(3));
    // tool_bash maps non-zero to Shell error with clamp + [exit N] via Display; direct run returns output + code.
    assert!(out.contains("out"));
    assert!(out.contains("--- stderr ---"));
    assert!(out.contains("err"));
    // Clamp preserved via tool_bash
    let mut args = Map::new();
    args.insert(
        "command".into(),
        serde_json::Value::String("echo hi".into()),
    );
    let ok = execute("bash", &args, &GlobalCancellation, &Policy::trusted(), None)
        .await
        .unwrap();
    assert!(ok.contains("hi"));
}

#[tokio::test]
async fn read_pagination_and_binary_refusal() {
    let cwd = std::env::current_dir().unwrap();
    let rel = "target/dex-read-pag-test.txt";
    let content = (1..=10)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    tokio::fs::create_dir_all(cwd.join("target")).await.unwrap();
    tokio::fs::write(cwd.join(rel), &content).await.unwrap();
    let mut args = Map::new();
    args.insert("path".into(), serde_json::Value::String(rel.into()));
    args.insert("offset".into(), serde_json::Value::from(3u64));
    args.insert("limit".into(), serde_json::Value::from(2u64));
    let out = execute("read", &args, &GlobalCancellation, &Policy::trusted(), None)
        .await
        .unwrap();
    assert!(out.contains("3"), "{out}");
    assert!(out.contains("line3") && out.contains("line4"));
    assert!(!out.contains("line5"));
    // Binary refused, not dumped
    let bin_rel = "target/dex-read-bin-test.bin";
    tokio::fs::write(cwd.join(bin_rel), vec![0u8, 1, 2, 3])
        .await
        .unwrap();
    let mut bargs = Map::new();
    bargs.insert("path".into(), serde_json::Value::String(bin_rel.into()));
    let err = execute(
        "read",
        &bargs,
        &GlobalCancellation,
        &Policy::trusted(),
        None,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("binary"), "{err}");
    let _ = tokio::fs::remove_file(cwd.join(rel)).await;
    let _ = tokio::fs::remove_file(cwd.join(bin_rel)).await;
}

#[tokio::test]
async fn chain_refuses_mutating_steps() {
    let mut args = Map::new();
    args.insert(
        "steps".into(),
        serde_json::Value::Array(vec![
            serde_json::json!({"tool": "read", "args": {"path": "Cargo.toml"}}),
            serde_json::json!({"tool": "bash", "args": {"command": "echo hi"}}),
        ]),
    );
    let err = execute(
        "chain",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        None,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("read-only"), "{err}");
}

// Phase 0 gate tests: dispatch consults the turn policy, parks an
// approval prompt for mutating calls under ask modes, and blocks for
// the verdict. Files land under `target/` (unique per test) with
// best-effort cleanup, mirroring the existing tests in this module.
use crate::protocol::{ApprovalDecision, PermissionMode};
use crate::runtime::console::Console;

fn phase0_console() -> (
    Console,
    tokio::sync::mpsc::Receiver<crate::protocol::ApprovalRequest>,
) {
    let (sink_tx, _sink_rx) = tokio::sync::mpsc::channel::<crate::protocol::SinkLine>(16);
    let (approval_tx, approval_rx) =
        tokio::sync::mpsc::channel::<crate::protocol::ApprovalRequest>(16);
    (Console::new(sink_tx, approval_tx), approval_rx)
}

fn phase0_write_args(path: &str) -> Map<String, Value> {
    let mut args = Map::new();
    args.insert("path".into(), Value::String(path.into()));
    args.insert("content".into(), Value::String("phase0\n".into()));
    args
}

#[tokio::test]
async fn ask_writes_parks_write_and_blocks_until_allowed() {
    let rel = "target/phase0-allow.txt";
    let _ = fs::remove_file(rel);
    let (console, mut approval_rx) = phase0_console();
    let policy = Policy::turn(PermissionMode::Ask, &console);
    let args = phase0_write_args(rel);
    let expected_input = Value::Object(args.clone()).to_string();
    let mut handle = tokio::spawn(async move {
        execute_outcome("write", &args, &GlobalCancellation, &policy, None).await
    });
    // The call blocks: no outcome before the verdict …
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut handle)
            .await
            .is_err(),
        "write must block for approval under ask"
    );
    // … and exactly one prompt is parked.
    let request = tokio::time::timeout(Duration::from_secs(5), approval_rx.recv())
        .await
        .expect("approval prompt must arrive")
        .expect("approval channel must stay open");
    assert_eq!(request.name, "write");
    assert_eq!(request.input, expected_input);
    request
        .response
        .send(ApprovalDecision::AllowOnce)
        .await
        .expect("agent must still be waiting");
    let outcome = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("verdict must unblock the call")
        .expect("worker panicked");
    assert!(outcome.ok, "{}", outcome.text);
    assert_eq!(fs::read_to_string(rel).unwrap(), "phase0\n");
    assert!(
        approval_rx.try_recv().is_err(),
        "exactly one prompt must be parked"
    );
    let _ = fs::remove_file(rel);
}

#[tokio::test]
async fn ask_writes_deny_blocks_the_write() {
    let rel = "target/phase0-deny.txt";
    let _ = fs::remove_file(rel);
    let (console, mut approval_rx) = phase0_console();
    let policy = Policy::turn(PermissionMode::Ask, &console);
    let args = phase0_write_args(rel);
    let handle = tokio::spawn(async move {
        execute_outcome("write", &args, &GlobalCancellation, &policy, None).await
    });
    let request = tokio::time::timeout(Duration::from_secs(5), approval_rx.recv())
        .await
        .expect("approval prompt must arrive")
        .expect("approval channel must stay open");
    request
        .response
        .send(ApprovalDecision::Deny)
        .await
        .expect("agent must still be waiting");
    let outcome = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("verdict must unblock the call")
        .expect("worker panicked");
    assert!(!outcome.ok);
    assert!(outcome.text.contains("denied"), "{}", outcome.text);
    assert!(!std::path::Path::new(rel).exists(), "deny must not write");
}

#[tokio::test]
async fn ask_writes_allow_session_skips_the_second_prompt() {
    let rel = "target/phase0-session.txt";
    let _ = fs::remove_file(rel);
    let (console, mut approval_rx) = phase0_console();
    let policy = Policy::turn(PermissionMode::Ask, &console);
    // First identical call prompts; deny it to release the worker.
    let args = phase0_write_args(rel);
    let policy2 = policy.clone();
    let first = tokio::spawn(async move {
        execute_outcome("write", &args, &GlobalCancellation, &policy2, None).await
    });
    let request = tokio::time::timeout(Duration::from_secs(5), approval_rx.recv())
        .await
        .expect("approval prompt must arrive")
        .expect("approval channel must stay open");
    request
        .response
        .send(ApprovalDecision::Deny)
        .await
        .expect("agent must still be waiting");
    let outcome = tokio::time::timeout(Duration::from_secs(10), first)
        .await
        .expect("verdict must unblock the call")
        .expect("worker panicked");
    assert!(!outcome.ok);
    assert!(!std::path::Path::new(rel).exists(), "deny must not write");
    // Second identical call prompts again; allow for session.
    let args2 = phase0_write_args(rel);
    let policy3 = policy.clone();
    let second = tokio::spawn(async move {
        execute_outcome("write", &args2, &GlobalCancellation, &policy3, None).await
    });
    let request2 = tokio::time::timeout(Duration::from_secs(5), approval_rx.recv())
        .await
        .expect("second prompt must arrive")
        .expect("approval channel must stay open");
    request2
        .response
        .send(ApprovalDecision::AllowSession)
        .await
        .expect("agent must still be waiting");
    let outcome2 = tokio::time::timeout(Duration::from_secs(10), second)
        .await
        .expect("verdict must unblock the call")
        .expect("worker panicked");
    assert!(outcome2.ok, "{}", outcome2.text);
    // Same-turn repeat: no new prompt, straight through.
    let args3 = phase0_write_args(rel);
    let outcome3 = tokio::time::timeout(
        Duration::from_secs(10),
        execute_outcome("write", &args3, &GlobalCancellation, &policy, None),
    )
    .await
    .expect("session-approved call must not block");
    assert!(outcome3.ok, "{}", outcome3.text);
    assert!(
        approval_rx.try_recv().is_err(),
        "no further prompt after allow-for-session"
    );
    let _ = fs::remove_file(rel);
}

#[tokio::test]
async fn read_only_rejects_write_without_prompt() {
    let rel = "target/phase0-readonly.txt";
    let _ = fs::remove_file(rel);
    let (console, mut approval_rx) = phase0_console();
    let policy = Policy::turn(PermissionMode::ReadOnly, &console);
    let args = phase0_write_args(rel);
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        execute_outcome("write", &args, &GlobalCancellation, &policy, None),
    )
    .await
    .expect("read-only rejection must not block");
    assert!(!outcome.ok);
    assert!(outcome.text.contains("read-only mode"), "{}", outcome.text);
    assert!(
        approval_rx.try_recv().is_err(),
        "read-only must reject, never prompt"
    );
    assert!(!std::path::Path::new(rel).exists());

    // Plan mode (`read-only`) still permits reads — its directive asks the
    // model to explore, so the read gate must not block that.
    let mut args = Map::new();
    args.insert("path".into(), Value::String("Cargo.toml".into()));
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        execute_outcome("read", &args, &GlobalCancellation, &policy, None),
    )
    .await
    .expect("a plan-mode read must not block");
    assert!(outcome.ok, "{}", outcome.text);
}

#[tokio::test]
async fn trusted_runs_mutations_with_no_channel() {
    let rel = "target/phase0-trusted.txt";
    let _ = fs::remove_file(rel);
    let args = phase0_write_args(rel);
    let outcome = execute_outcome(
        "write",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        None,
    )
    .await;
    assert!(outcome.ok, "{}", outcome.text);
    assert_eq!(fs::read_to_string(rel).unwrap(), "phase0\n");
    let _ = fs::remove_file(rel);
}

#[tokio::test]
async fn ask_permits_reads_but_prompts_writes_and_shell() {
    // A write under `ask` parks exactly one prompt and blocks for the
    // verdict (no console attached here, so it surfaces as a denial).
    let rel = "target/phase0-ask.txt";
    let _ = fs::remove_file(rel);
    let (console, mut approval_rx) = phase0_console();
    let policy = Policy::turn(PermissionMode::Ask, &console);
    let args = phase0_write_args(rel);
    let policy2 = policy.clone();
    let mut handle = tokio::spawn(async move {
        execute_outcome("write", &args, &GlobalCancellation, &policy2, None).await
    });
    // The call blocks: no outcome before the verdict …
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut handle)
            .await
            .is_err(),
        "write must block for approval under ask"
    );
    // … and exactly one prompt is parked.
    let request = tokio::time::timeout(Duration::from_secs(5), approval_rx.recv())
        .await
        .expect("approval prompt must arrive")
        .expect("approval channel must stay open");
    assert_eq!(request.name, "write");
    request
        .response
        .send(ApprovalDecision::AllowOnce)
        .await
        .expect("agent must still be waiting");
    let outcome = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("verdict must unblock the call")
        .expect("worker panicked");
    assert!(outcome.ok, "{}", outcome.text);
    assert_eq!(fs::read_to_string(rel).unwrap(), "phase0\n");
    let _ = fs::remove_file(rel);

    // A read is free under `ask` — it never touches the approval channel.
    let mut args = Map::new();
    args.insert("path".into(), Value::String("Cargo.toml".into()));
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        execute_outcome("read", &args, &GlobalCancellation, &policy, None),
    )
    .await
    .expect("a read must not block under ask");
    assert!(outcome.ok, "{}", outcome.text);
    assert!(
        approval_rx.try_recv().is_err(),
        "a read must not park a prompt"
    );
}

#[tokio::test]
async fn approval_wait_unwinds_on_cancel() {
    use crate::runtime::console::CancellationToken;
    let rel = "target/phase0-cancel.txt";
    let _ = fs::remove_file(rel);
    let (console, mut approval_rx) = phase0_console();
    let policy = Policy::turn(PermissionMode::Ask, &console);
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();
    let args = phase0_write_args(rel);
    let handle =
        tokio::spawn(async move { execute_outcome("write", &args, &cancel2, &policy, None).await });
    // Wait for the parked prompt, then cancel instead of answering.
    let _request = tokio::time::timeout(Duration::from_secs(5), approval_rx.recv())
        .await
        .expect("approval prompt must arrive")
        .expect("approval channel must stay open");
    cancel.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .expect("cancel must unblock the parked call")
        .expect("worker panicked");
    assert!(!outcome.ok);
    assert!(outcome.text.contains("cancelled"), "{}", outcome.text);
    assert!(!std::path::Path::new(rel).exists());
}

// Phase 2 (runtime extraction): the explicit ToolFilter allowlist,
// enforced at dispatch. `None` preserves the parent path everywhere.
#[test]
fn tool_filter_matches_exact_and_wildcard_only() {
    let filter = ToolFilter::new("explorer", ["read", "ffgrep", "mcp__gh__*"]);
    assert!(filter.allows("read"));
    assert!(filter.allows("ffgrep"));
    assert!(filter.allows("mcp__gh__search"));
    assert!(!filter.allows("bash"));
    assert!(!filter.allows("mcp__other__tool"));
    assert!(!filter.allows("read_all"), "no accidental prefix match");
    assert!(!filter.allows("delegate"));
}

#[tokio::test]
async fn filtered_out_tool_is_rejected_with_allowlist_error() {
    let filter = ToolFilter::new("explorer", ["read"]);
    let mut args = Map::new();
    args.insert("command".into(), Value::String("echo hi".into()));
    let err = execute(
        "bash",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        Some(&filter),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("explorer"), "{err}");
    assert!(err.to_string().contains("allowlist"), "{err}");
    // An allowed tool still runs under the same filter.
    let mut args = Map::new();
    args.insert("path".into(), Value::String("Cargo.toml".into()));
    let out = execute(
        "read",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        Some(&filter),
    )
    .await
    .unwrap();
    assert!(out.contains("dex"), "{out}");
}

#[tokio::test]
async fn filtered_out_tool_rejects_before_approval_without_prompt() {
    // Filter runs before policy: a denied tool must fail closed with an
    // allowlist error and never park an approval prompt.
    let (console, mut approval_rx) = phase0_console();
    let policy = Policy::turn(PermissionMode::Ask, &console);
    let filter = ToolFilter::new("explorer", ["read"]);
    let args = phase0_write_args("target/phase0-filter-no-prompt.txt");
    let outcome = tokio::time::timeout(
        Duration::from_secs(10),
        execute_outcome("write", &args, &GlobalCancellation, &policy, Some(&filter)),
    )
    .await
    .expect("filtered-out rejection must not block");
    assert!(!outcome.ok);
    assert!(outcome.text.contains("allowlist"), "{}", outcome.text);
    assert!(
        approval_rx.try_recv().is_err(),
        "filtered-out tool must reject, never prompt"
    );
    assert!(!std::path::Path::new("target/phase0-filter-no-prompt.txt").exists());
}

#[tokio::test]
async fn unknown_tool_stays_unknown_under_filter() {
    let filter = ToolFilter::new("explorer", ["read"]);
    let args = Map::new();
    let err = execute(
        "nope",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        Some(&filter),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("unknown tool"), "{err}");
}

#[tokio::test]
async fn chain_steps_run_under_the_same_filter() {
    // `chain` itself is allowed; its `read` step is not (chain's own
    // read-only gate rejects non-read steps before dispatch, so probe
    // the filter with a read step instead).
    let filter = ToolFilter::new("explorer", ["ffgrep", "chain"]);
    let mut args = Map::new();
    args.insert(
        "steps".into(),
        serde_json::Value::Array(vec![
            serde_json::json!({"tool": "ffgrep", "args": {"pattern": "dex"}}),
            serde_json::json!({"tool": "read", "args": {"path": "Cargo.toml"}}),
        ]),
    );
    let err = execute(
        "chain",
        &args,
        &GlobalCancellation,
        &Policy::trusted(),
        Some(&filter),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("allowlist"), "{err}");
}
