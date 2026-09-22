//! ui/format tests.

use super::*;

/// Four-arg convenience for the common no-diff case; tests that pass a
/// unified diff call [`tool_result_summary`] directly.
fn summary(name: &str, input: &str, text: &str, ok: bool) -> String {
    tool_result_summary(name, input, text, ok, None)
}

/// Display columns of a string (wide chars count 2) — the unit of the
/// shared [`PREVIEW_LINE_COLS`] budget.
fn display_cols(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// The budget's canonical cut of `l`-filler: budget-1 columns of content
/// plus the 1-column `…` marker — what any 400+-column line must clip to.
fn clipped_at_budget() -> String {
    format!("{}…", "l".repeat(PREVIEW_LINE_COLS - 1))
}

/// The approval prompt must show the `then_run` shell command. An
/// approver who only sees the file diff is approving something broader than
/// what the summary describes.
#[test]
fn approval_prompt_shows_the_then_run_command() {
    let input = r#"{"path":"src/a.rs","oldText":"a","newText":"b","then_run":"cargo clippy --all-targets"}"#;
    assert_eq!(
        approval_summary("edit", input),
        "src/a.rs · -1 +1 · then: cargo clippy --all-targets"
    );
    assert!(
        approval_details("edit", input)
            .iter()
            .any(|line| line == "then: $ cargo clippy --all-targets"),
        "details must carry the command"
    );
    // No then_run → no trace of one.
    assert_eq!(
        approval_summary("edit", r#"{"path":"src/a.rs","oldText":"a","newText":"b"}"#),
        "src/a.rs · -1 +1"
    );
    assert!(
        !approval_details("write", r#"{"path":"x","content":"y"}"#)
            .iter()
            .any(|line| line.starts_with("then:")),
        "a plain write has no command line"
    );
}

/// A `then_run` mutation is a shell command in disguise: the approval
/// overlay must not label it a plain write or under-rate its risk.
#[cfg(all(test, feature = "tui"))]
#[test]
fn approval_surface_escalates_a_then_run_mutation() {
    let input = r#"{"path":"src/a.rs","content":"x","then_run":"cargo test"}"#;
    assert_eq!(
        approval_title("write", input),
        "File change + shell verification"
    );
    assert_eq!(approval_risk("write", input).0, "high");
    // A plain write keeps its title and medium risk.
    let plain = r#"{"path":"src/a.rs","content":"x"}"#;
    assert_eq!(approval_title("write", plain), "Create / overwrite file");
    assert_eq!(approval_risk("write", plain).0, "medium");
    // …and a blank then_run is not a command.
    let blank = r#"{"path":"src/a.rs","content":"x","then_run":"  "}"#;
    assert_eq!(approval_title("write", blank), "Create / overwrite file");
}

/// A failed verification must not leave the transcript summary reading as
/// an unqualified success: the mutation landed, so `ok` stays true, but the
/// `then_run` verdict rides along.
#[test]
fn tool_summary_carries_the_then_run_verdict() {
    let input = r#"{"path":"src/a.rs","content":"x\ny\n","then_run":"cargo test"}"#;
    let passed = "wrote src/a.rs\n\n[then_run:succeeded] cargo test\nok";
    assert_eq!(
        summary("write", input, passed, true),
        "2 lines written · then_run: succeeded"
    );
    let failed = "wrote src/a.rs\n\n[then_run:failed (exit 3)] cargo test\nboom";
    assert_eq!(
        summary("write", input, failed, true),
        "2 lines written · then_run: failed (exit 3)"
    );
    // No marker → the summary is unchanged.
    assert_eq!(
        summary("write", r#"{"content":"x"}"#, "wrote x", true),
        "1 line written"
    );
}

#[test]
fn tool_rows_share_one_display_column_budget() {
    // The standard: every transcript tool row — the input preview
    // (`short_arg`), the output summary (`one_line_summary`), and every
    // preview flavor — is bounded by the same budget, measured in
    // display columns (wide chars count 2), with every cut marked `…`.
    let long = "l".repeat(400); // 400 display columns
    let clipped = clipped_at_budget();
    assert_eq!(display_cols(&clipped), PREVIEW_LINE_COLS);
    assert!(clipped.ends_with('…'), "cut must be marked");

    // Input row and output rows obey the same budget, byte-for-byte.
    assert_eq!(
        short_arg("bash", &format!(r#"{{"command":"{long}"}}"#)),
        clipped,
        "input row"
    );
    assert_eq!(one_line_summary(&long), clipped, "summary row");
    assert_eq!(
        tool_result_preview(&long, TRANSCRIPT_PREVIEW_LINES, false)[0],
        clipped,
        "generic preview"
    );
    // Prefixes (diff marker, read gutter) count against the same budget.
    let budgeted = |prefix: &str| {
        format!(
            "{prefix}{}…",
            "l".repeat(PREVIEW_LINE_COLS - 1 - prefix.chars().count())
        )
    };
    assert_eq!(
        diff_preview_lines(&format!("-{long}"), 6)[0],
        budgeted("-"),
        "diff preview keeps its marker prefix"
    );
    assert_eq!(
        read_preview_lines(&format!("   1  {long}"), 6)[0],
        budgeted("   1  "),
        "read preview keeps its gutter"
    );
}

#[test]
fn headless_rows_share_one_display_column_budget() {
    // The headless one-shot path (`[tool input]` / `[tool output]` on
    // stderr) must adhere to the same per-line standard as the
    // transcript rows: [`PREVIEW_LINE_COLS`] display columns, wide chars
    // count 2, every cut marked `…`, ANSI escapes stripped.
    let long = "l".repeat(400); // 400 display columns
    let clipped = clipped_at_budget();

    assert_eq!(terminal_preview(&long), clipped, "output body");
    // Every line obeys the budget independently.
    assert_eq!(
        terminal_preview(&format!("{long}\n{long}")),
        format!("{clipped}\n{clipped}"),
        "per line"
    );
    // ANSI bytes are stripped, not counted and not leaked to the terminal.
    let ansi = format!("\x1b[31m{long}\x1b[0m");
    assert_eq!(terminal_preview(&ansi), clipped);
    // Wide chars spend 2 columns each: 200 CJK chars (400 columns) clip.
    let wide = "界".repeat(200);
    let wc = terminal_preview(&wide);
    assert!(display_cols(&wc) <= PREVIEW_LINE_COLS && wc.ends_with('…'));

    // A [`MAX_LINE_CHARS`] cut landing mid-escape-sequence must not
    // swallow the `…` marker: escapes are stripped before the budget is
    // applied, so this 5000-column line still previews as the budgeted
    // cut instead of an empty row.
    let giant = format!("\x1b[{}m", "1".repeat(MAX_LINE_CHARS - 2));
    let raw = format!("{giant}{}", "l".repeat(5_000));
    assert_eq!(
        terminal_preview(&raw),
        clipped,
        "escape cut keeps the marker"
    );

    // The dispatch wrapper and the input row agree with the same budget.
    assert_eq!(tool_preview_body("bash", true, None, &long), clipped);
    assert_eq!(
        short_arg("bash", &format!(r#"{{"command":"{long}"}}"#)),
        clipped,
        "headless input row uses the same short_arg preview as the TUI"
    );
}

#[test]
fn short_arg_fallback_collapses_whitespace() {
    // Unknown tool or unparseable input: the raw fallback is
    // whitespace-collapsed so a pretty-printed JSON blob previews as one
    // compact row instead of just its first line (`{`).
    let pretty = "{\n  \"weird\": \"value\"\n}";
    assert_eq!(
        short_arg("unknown_tool", pretty),
        "{ \"weird\": \"value\" }"
    );
    // Plain multi-line fallback is compacted the same way.
    assert_eq!(short_arg("unknown_tool", "a\n  b\n  c"), "a b c");
}

#[test]
fn preview_budget_counts_display_columns_not_chars() {
    // Wide chars spend 2 columns each: 160 CJK chars are 320 columns and
    // must fit the 320-column budget untouched (the old char-count clip
    // at 120 "chars" cut them and left 240+ column rows).
    let fits = "界".repeat(160);
    assert_eq!(UnicodeWidthStr::width(fits.as_str()), PREVIEW_LINE_COLS);
    assert_eq!(
        one_line_summary(&fits),
        fits,
        "no cut at exactly the budget"
    );
    assert_eq!(
        tool_result_preview(&fits, 6, false),
        vec![fits],
        "preview agrees with the summary row"
    );
    // 200 CJK chars (400 columns) clip to the budget with a marker.
    let wide = "界".repeat(200);
    let clipped = one_line_summary(&wide);
    assert!(clipped.ends_with('…'));
    assert!(UnicodeWidthStr::width(clipped.as_str()) <= PREVIEW_LINE_COLS);
    assert_eq!(clipped.chars().count(), 160, "159 wide chars + ellipsis");
}

/// Empty `paths`/`glob` arrive as schema defaults next to a real `path`;
/// the approval surface must describe the read that will actually run.
#[test]
fn approval_read_treats_empty_paths_and_glob_as_absent() {
    let input = r#"{"path":"src/a.rs","paths":[],"glob":""}"#;
    assert_eq!(approval_summary("read", input), "src/a.rs");
    assert_eq!(approval_details("read", input), vec!["path: src/a.rs"]);
    // Populated variants keep their fan-out display.
    assert_eq!(
        approval_summary("read", r#"{"paths":["a","b"]}"#),
        "2 files"
    );
    assert_eq!(
        approval_summary("read", r#"{"glob":"src/**/*.rs"}"#),
        "glob: src/**/*.rs"
    );
}

#[test]
fn summary_is_outcome_first_without_ok_prefix() {
    // A successful read whose content happens to contain the shell
    // failure marker must not be reported as failed.
    assert_eq!(
        summary(
            "read",
            "{}",
            "use serde_json::{Map, Value};\nresult.push_str(\"[exit 1]\");\n",
            true
        ),
        "2 lines"
    );
    // A genuinely failed tool reports failure with its first output line.
    assert_eq!(
        summary("bash", "{}", "ls: no such file\n[exit 2]", false),
        "failed · ls: no such file"
    );
    // grep defaults to files mode: the summary counts files, not matches.
    assert_eq!(summary("grep", "{}", "", true), "0 files matched");
    assert_eq!(
        summary(
            "grep",
            r#"{"output_mode":"content"}"#,
            "src/a.rs:1:hit",
            true
        ),
        "1 match"
    );
    // Context rows and the truncation trailer are not matches; fuzzy
    // fallback rows (`12: code` under a path header) are.
    assert_eq!(
            summary(
                "grep",
                r#"{"output_mode":"content"}"#,
                "src/a.rs:2:hit\nsrc/a.rs:1-before\nsrc/a.rs:3-after\n[... 1 match shown, more files unscanned; continue with file_offset 2 ...]",
                true
            ),
            "1 match"
        );
    assert_eq!(
        summary(
            "grep",
            r#"{"output_mode":"content"}"#,
            "0 exact matches for 'q'. 2 approximate:\nsrc/a.rs\n  12: first\n  30: second",
            true
        ),
        "2 matches"
    );
    assert_eq!(
            summary(
                "grep",
                "{}",
                "a.rs\nb.rs\nc.rs\n[... 3 files shown, more files unscanned; continue with file_offset 3 ...]",
                true
            ),
            "3 files matched"
        );
    // Files-mode fuzzy fallback lists paths under a prose header:
    // the header is not a file.
    assert_eq!(
        summary(
            "ffgrep",
            "{}",
            "0 exact matches for 'q'. 2 approximate:\nsrc/a.rs\nsrc/b.rs",
            true
        ),
        "2 files matched"
    );
    // Empty successful output says so instead of a bare "ok".
    assert_eq!(summary("bash", "{}", "", true), "(no output)");
    assert_eq!(
        summary("bash", "{}", "hello world\nrest", true),
        "hello world"
    );
}

#[test]
fn mutation_summaries_show_diffstats_from_input() {
    let write_input = r#"{"path":"src/x.rs","content":"a\nb\nc\n"}"#;
    assert_eq!(
        summary("write", write_input, "wrote src/x.rs", true),
        "3 lines written"
    );
    let single = r#"{"path":"src/x.rs","content":"only"}"#;
    assert_eq!(
        summary("write", single, "wrote src/x.rs", true),
        "1 line written"
    );
    let edit_input = r#"{"path":"src/x.rs","oldText":"a\nb","newText":"x\ny\nz"}"#;
    assert_eq!(
        summary("edit", edit_input, "edited src/x.rs", true),
        "+3 −2"
    );
}

#[test]
fn duration_formats_for_each_scale() {
    assert_eq!(format_duration(0.042), "42ms");
    assert_eq!(format_duration(0.42), "420ms");
    assert_eq!(format_duration(2.14), "2.1s");
    assert_eq!(format_duration(9.96), "10.0s");
    assert_eq!(format_duration(41.96), "42s");
    assert_eq!(format_duration(65.0), "1m 05s");
    assert_eq!(format_duration(125.4), "2m 05s");
    assert_eq!(format_duration(-1.0), "");
}

#[test]
fn clamp_keeps_head_and_tail_with_real_counts() {
    let text: String = (1..=100).map(|n| format!("line {n}\n")).collect();
    let clamped = clamp_lines(text.trim_end(), 10, 1 << 20);
    assert!(clamped.starts_with("line 1\n"), "{clamped}");
    assert!(clamped.ends_with("line 100"), "{clamped}");
    assert!(
        clamped.contains("[... 90 of 100 lines truncated ...]"),
        "{clamped}"
    );

    // Within the limit the text passes through untouched.
    assert_eq!(clamp_lines("a\nb\nc", 10, 1024), "a\nb\nc");

    // Pathologically long lines are clipped, not kept whole.
    let long_line = "x".repeat(5000);
    let clamped = clamp_lines(&long_line, 10, 1 << 20);
    assert!(clamped.ends_with('…'));
    assert!(clamped.len() < 2100, "{}", clamped.len());

    // The byte budget bounds the result even below the line limit.
    let text: String = (1..=50)
        .map(|n| format!("{n} {}\n", "y".repeat(500)))
        .collect();
    let clamped = clamp_lines(text.trim_end(), 100, 4096);
    assert!(clamped.contains("lines truncated"), "{clamped}");
    assert!(clamped.len() < 8192, "{}", clamped.len());
}

#[test]
fn clamp_checked_flags_real_cuts_not_newline_normalization() {
    // Trailing newline is normalization, not a cut: the capture path
    // must not archive-and-mark a result for it.
    let (out, cut) = clamp_lines_checked("a\nb\nc\n", 10, 1024);
    assert_eq!(out, "a\nb\nc");
    assert!(!cut);
    assert_eq!(
        clamp_lines_checked("no newline", 10, 1024),
        ("no newline".to_string(), false)
    );

    // Line-budget cut.
    let text: String = (1..=100).map(|n| format!("line {n}\n")).collect();
    let (_, cut) = clamp_lines_checked(&text, 10, 1 << 20);
    assert!(cut);

    // Per-line clip is a cut even where the output can gain bytes (`…`).
    let (_, cut) = clamp_lines_checked(&format!("{}\n", "x".repeat(5000)), 10, 1 << 20);
    assert!(cut);
    let (_, cut) = clamp_lines_checked("x".repeat(2001).as_str(), 10, 1 << 20);
    assert!(cut);
}

#[test]
fn preview_shows_meaningful_lines_with_more_tail() {
    let text = "\nfirst\n\nsecond\nthird\nfourth\n";
    assert_eq!(
        tool_result_preview(text, 3, false),
        vec!["first", "second", "third", "… +1 more line"]
    );
}

#[test]
fn preview_can_skip_the_line_already_in_the_summary() {
    let text = "Error: boom\nat src/main.rs:1\nat src/main.rs:2\n";
    assert_eq!(
        tool_result_preview(text, 3, true),
        vec!["at src/main.rs:1", "at src/main.rs:2"]
    );
}

#[test]
fn preview_strips_ansi_and_clips_long_lines() {
    let text = format!("\x1b[1;32m{}\x1b[0m", "x".repeat(400));
    let preview = tool_result_preview(&text, 1, false);
    assert_eq!(preview.len(), 1);
    assert!(preview[0].starts_with("xxx"));
    assert!(preview[0].ends_with('…'));
    // The shared transcript budget, in display columns like every other
    // tool row (see tool_rows_share_one_display_column_budget).
    assert!(UnicodeWidthStr::width(preview[0].as_str()) <= PREVIEW_LINE_COLS);
    assert!(!preview[0].contains('\x1b'));
}

#[test]
fn preview_of_empty_output_is_empty() {
    assert!(tool_result_preview("", 3, false).is_empty());
    assert!(tool_result_preview("\n \n", 3, true).is_empty());
}

#[test]
fn summary_and_arg_strip_escapes_and_carriage_returns() {
    assert_eq!(one_line_summary("\x1b[31mboom\x1b[0m\r\nnext"), "boom");
    assert_eq!(one_line_summary("progress\r\r\x07done"), "progressdone");
    // Fallback path: unparseable input JSON is sanitized too.
    assert_eq!(short_arg("x", "'\x1b[1mevil\x1b[0m\r"), "'evil");
}

#[test]
fn short_arg_keeps_full_command_and_marks_any_cut() {
    // A long bash command must not be silently chopped at one line: the
    // transcript wraps the row, so only pathological length is capped —
    // with a visible `…`, never a silent cut.
    let cmd = format!("echo {}", "a".repeat(500));
    let arg = short_arg("bash", &format!(r#"{{"command":"{cmd}"}}"#));
    assert_eq!(arg, format!("echo {}…", "a".repeat(314)));
    // Ordinary-length commands survive whole.
    assert_eq!(
        short_arg("bash", r#"{"command":"cargo test"}"#),
        "cargo test"
    );
}

#[test]
fn diff_preview_is_github_style_hunks_without_file_headers() {
    let diff = "--- a/src/x.rs\n+++ b/src/x.rs\n@@ -1,5 +1,6 @@\n ctx\n-old\n+new\n ctx2\n";
    let preview = diff_preview_lines(diff, 30);
    // File headers are dropped (the tool input line already names the
    // path); the hunk header and +/-/context lines remain.
    assert!(!preview.iter().any(|l| l.starts_with("--- ")));
    assert!(!preview.iter().any(|l| l.starts_with("+++ ")));
    assert!(preview.iter().any(|l| l.starts_with("@@")));
    assert!(preview.iter().any(|l| l == "-old"));
    assert!(preview.iter().any(|l| l == "+new"));
    assert!(preview.iter().any(|l| l == " ctx"));
}

#[test]
fn diff_preview_clips_long_lines_and_caps_the_tail() {
    let long = format!("+{}", "x".repeat(400));
    let diff = format!("--- a/f\n+++ b/f\n@@ -1 +1 @@\n{long}\n+two\n+three\n");
    let preview = diff_preview_lines(&diff, 2);
    assert_eq!(preview.len(), 3);
    assert!(preview[0].starts_with("@@"));
    assert!(preview[1].ends_with('…'));
    assert!(UnicodeWidthStr::width(preview[1].as_str()) <= PREVIEW_LINE_COLS);
    assert_eq!(preview[2], "… +2 more diff lines");
}

#[test]
fn preview_preserves_indentation_for_all_tools() {
    // Tree output, indented code, nested listings: the snippet must not
    // flatten what the tool actually printed.
    let text = "src/\n  main.rs\n    mod deep\n";
    assert_eq!(
        tool_result_preview(text, 6, false),
        vec!["src/", "  main.rs", "    mod deep"]
    );
}

#[test]
fn short_arg_covers_every_tool_without_raw_json() {
    assert_eq!(
        short_arg("find", r#"{"pattern": "TODO", "path": "."}"#),
        "."
    );
    assert_eq!(short_arg("ffgrep", r#"{"pattern": "TODO"}"#), "TODO");
    assert_eq!(short_arg("fffind", r#"{"pattern": "*.rs"}"#), "*.rs");
    assert_eq!(short_arg("git", r#"{"mode": "diff"}"#), "diff");
    assert_eq!(short_arg("git", "{}"), "status");
}

#[test]
fn summaries_cover_the_ff_aliases() {
    assert_eq!(
        summary("ffgrep", "{}", "a.rs\nb.rs\n", true),
        "2 files matched"
    );
    assert_eq!(summary("fffind", "{}", "a.rs\n", true), "1 entry");
}

#[test]
fn entry_summaries_exclude_truncation_trailers() {
    // fffind's truncation note is not an entry; its count becomes the tail.
    assert_eq!(
        summary(
            "fffind",
            "{}",
            "a.rs\nb.rs\n[... 3 of 5 paths matched; raise limit or narrow the query ...]",
            true
        ),
        "2 entries (+3 more)"
    );
    // The picker's empty result is prose, not one entry.
    assert_eq!(summary("fffind", "{}", "0 matches.", true), "empty");
    // ls clamp trailer: 500 real entries shown, N omitted.
    assert_eq!(
        summary("ls", "{}", "a\nb\n[... 7 of 507 lines truncated ...]", true),
        "2 entries (+7 more)"
    );
    assert_eq!(summary("ls", "{}", "(empty)", true), "empty");
    // chain: step-output trailers stay out of the line count.
    assert_eq!(
        summary(
            "chain",
            r#"{"steps":[{"tool":"ls"},{"tool":"ls"}]}"#,
            "==> src/a.rs <==\nmain.rs\n[... 4 of 10 lines truncated ...]",
            true
        ),
        "2 steps · 1 file · 2 lines (+4 more)"
    );
}

#[test]
fn edit_diffstat_counts_replicated_hunks_from_the_diff() {
    // replaceAll turned one hunk into three: the input shows a single
    // spelling, the real diff shows all three.
    let input = r#"{"path":"x.rs","oldText":"old","newText":"new"}"#;
    let diff = "--- a/x.rs\n+++ b/x.rs\n@@ -1,3 +1,3 @@\n-old\n+new\n-old\n+new\n-old\n+new\n";
    assert_eq!(
        tool_result_summary("edit", input, "edited x.rs", true, Some(diff)),
        "+3 −3"
    );
    // No diff available: the input-based estimate still applies.
    assert_eq!(summary("edit", input, "edited x.rs", true), "+1 −1");
}

#[test]
fn strip_ansi_consumes_osc_payloads() {
    // Hyperlink: OSC 8 payload with an ST terminator.
    assert_eq!(
        one_line_summary("\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\ here"),
        "link here"
    );
    // Window title with a BEL terminator.
    assert_eq!(one_line_summary("\x1b]0;title\x07done"), "done");
}

#[test]
fn render_mcp_panel_lists_tools_and_down_servers() {
    use crate::protocol::ServerStatus;
    use crate::protocol::{FunctionDef, ToolDefinition};
    let tool = |name: &str, description: &str| ToolDefinition {
        tool_type: "function".to_string(),
        function: FunctionDef {
            name: name.to_string(),
            description: description.to_string(),
            parameters: serde_json::json!({}),
        },
    };
    let lines = render_mcp_panel(
        &[
            ServerStatus {
                name: "gh".to_string(),
                state: "up".to_string(),
                tools: 2,
                error: None,
            },
            ServerStatus {
                name: "db".to_string(),
                state: "down".to_string(),
                tools: 0,
                error: Some("spawn sqlite-mcp: No such file\nmore".to_string()),
            },
        ],
        &[
            tool("mcp__gh__search", "Search issues\nand pull requests."),
            tool("mcp__gh_read_resource", "Read a resource by URI."),
            tool("mcp__other__x", "Belongs to another server."),
        ],
        0,
    );
    assert_eq!(lines[0], "MCP servers (1 connected, 1 down · 2 tools):");
    assert_eq!(lines[1], "✓ gh — 2 tools");
    // Prefix stripped, description collapsed to its first line.
    assert!(lines.contains(&"  · read_resource — Read a resource by URI.".to_string()));
    assert!(lines.contains(&"  · search — Search issues".to_string()));
    assert!(!lines.iter().any(|l| l.contains("other")));
    // Down server shows the first error line only.
    assert_eq!(
        lines.last().unwrap(),
        "✗ db — down: spawn sqlite-mcp: No such file"
    );
}

#[test]
fn render_mcp_panel_empty_and_truncated() {
    let lines = render_mcp_panel(&[], &[], 0);
    assert_eq!(lines, vec!["no MCP servers configured."]);
    use crate::protocol::ServerStatus;
    let lines = render_mcp_panel(
        &[ServerStatus {
            name: "gh".to_string(),
            state: "up".to_string(),
            tools: 1,
            error: None,
        }],
        &[],
        3,
    );
    assert!(lines
        .last()
        .unwrap()
        .contains("3 more tools hidden by the schema cap"));
}
