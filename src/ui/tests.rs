//! UI module tests, split out of `mod.rs`.

use super::*;
use std::fs;
use std::path::PathBuf;

fn test_app() -> App {
    App::test_app()
}

#[test]
fn b64_matches_known_vectors() {
    assert_eq!(b64(b""), "");
    assert_eq!(b64(b"f"), "Zg==");
    assert_eq!(b64(b"fo"), "Zm8=");
    assert_eq!(b64(b"foo"), "Zm9v");
    assert_eq!(b64(b"foob"), "Zm9vYg==");
    assert_eq!(b64(b"fooba"), "Zm9vYmE=");
    assert_eq!(b64(b"foobar"), "Zm9vYmFy");
}

#[test]
fn selection_norm_orders_cells() {
    let s = Selection {
        anchor: (5, 2),
        end: (3, 7),
        sticky: false,
        whole_line: false,
    };
    assert_eq!(s.norm(), ((3, 7), (5, 2)));
    let s = Selection {
        anchor: (2, 9),
        end: (2, 1),
        sticky: false,
        whole_line: false,
    };
    assert_eq!(s.norm(), ((2, 1), (2, 9)));
    assert!(Selection {
        anchor: (1, 1),
        end: (1, 1),
        sticky: false,
        whole_line: false,
    }
    .is_empty());
    assert!(!Selection {
        anchor: (1, 1),
        end: (1, 2),
        sticky: false,
        whole_line: false,
    }
    .is_empty());
    // Whole-line selects order by row; cols carry no meaning downstream.
    let s = Selection {
        anchor: (5, 0),
        end: (2, 4),
        sticky: true,
        whole_line: true,
    };
    assert_eq!(s.norm(), ((2, 4), (5, 0)));
}

#[test]
fn line_width_counts_chars_across_spans() {
    let spans = Line::from(vec![Span::from("run "), Span::from("cargo")]);
    assert_eq!(line_width(&spans), 9);
    assert_eq!(line_width(&Line::default()), 0);
}

#[test]
fn line_selection_text_takes_full_rows() {
    let rows = vec![
        Line::from("alpha"),
        Line::default(),
        Line::from(Span::styled("gamma!", Style::default().fg(Color::Cyan))),
    ];
    // Every covered row in full, blank row included.
    assert_eq!(line_selection_text(&rows, 0, 2), "alpha\n\ngamma!");
    // Single row.
    assert_eq!(line_selection_text(&rows, 1, 1), "");
    // End past the cache is clamped.
    assert_eq!(line_selection_text(&rows, 2, 9), "gamma!");
}

#[test]
fn word_bounds_selects_enclosing_word() {
    let line = Line::from("run cargo test --all-targets");
    // Click inside "cargo" → whole word (inclusive bounds).
    assert_eq!(word_bounds(&line, 5), Some((4, 8)));
    assert_eq!(word_bounds(&line, 4), Some((4, 8)));
    assert_eq!(word_bounds(&line, 8), Some((4, 8)));
    // Word at line start/end ("--all-targets" spans 15..=27).
    assert_eq!(word_bounds(&line, 1), Some((0, 2)));
    assert_eq!(word_bounds(&line, 27), Some((15, 27)));
    // Whitespace click or past end → no selection.
    assert_eq!(word_bounds(&line, 3), None);
    assert_eq!(word_bounds(&line, 28), None);
    // Spans are joined before scanning.
    let spans = Line::from(vec![Span::from("run "), Span::from("cargo")]);
    assert_eq!(word_bounds(&spans, 6), Some((4, 8)));
}

#[test]
fn selection_text_slices_rows() {
    let rows = vec![
        Line::from("alpha"),
        Line::default(),
        Line::from(Span::styled("beta", Style::default().fg(Color::Cyan))),
    ];
    // Across rows, through the blank separator; end cell inclusive.
    assert_eq!(selection_text(&rows, (0, 1), (2, 2)), "lpha\n\nbet");
    // Single row, single range.
    assert_eq!(selection_text(&rows, (2, 1), (2, 3)), "eta");
    // Anchor past the end of the line yields nothing.
    assert_eq!(selection_text(&rows, (0, 90), (0, 95)), "");
}

#[test]
fn selection_text_covers_both_endpoint_cells() {
    // Native terminal convention: releasing on a char selects it.
    // Dragging onto a line's last char must include that char rather
    // than forcing an overshoot into the blank margin, which rounds up
    // to the whole rest of the line.
    let rows = vec![Line::from("hello world"), Line::from("second row")];
    // Forward drag releasing on the last char ('d', col 10).
    assert_eq!(selection_text(&rows, (0, 6), (0, 10)), "world");
    // Backward drag pressed on the last char: both ends covered (the
    // handler copies through `norm()`, as here).
    let sel = Selection {
        anchor: (0, 10),
        end: (0, 6),
        sticky: false,
        whole_line: false,
    };
    let ((r0, c0), (r1, c1)) = sel.norm();
    assert_eq!(selection_text(&rows, (r0, c0), (r1, c1)), "world");
    // Multi-row: the release cell on the last row is included.
    assert_eq!(selection_text(&rows, (0, 6), (1, 3)), "world\nseco");
    // End in the blank margin still takes the row's tail.
    assert_eq!(selection_text(&rows, (0, 6), (0, 40)), "world");
}

#[test]
fn line_selection_text_strips_code_border_prefix() {
    let border = Style::default().fg(Color::DarkGray);
    let code_row = |t: &str| {
        Line::from(vec![
            Span::raw(" "),
            Span::styled("│ ", border),
            Span::styled(t.to_string(), Color::Cyan),
        ])
    };
    // Fenced-code rows: the `│ ` prefix is display furniture.
    let rows = vec![code_row("dex-eval run"), code_row("  --out ~/x")];
    assert_eq!(
        line_selection_text(&rows, 0, 1),
        " dex-eval run\n   --out ~/x"
    );
    // Table rows use a bare `│` span — their pipes survive.
    let table = Line::from(vec![
        Span::raw(" "),
        Span::styled("│", border),
        Span::raw(" A "),
        Span::styled("│", border),
    ]);
    assert_eq!(line_selection_text(&[table], 0, 0), " │ A │");
}

#[test]
fn selection_text_slices_after_border_strip() {
    let row = Line::from(vec![
        Span::raw(" "),
        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
        Span::from("dex-eval run"),
    ]);
    let rows = vec![row];
    // Columns refer to the displayed row (` │ dex-eval run`); the copy
    // drops the two border cells and shifts with them, release cell
    // inclusive (display cols 3..=6 → `dex-`).
    assert_eq!(selection_text(&rows, (0, 3), (0, 6)), "dex-");
    // A selection ending on a border cell copies only the indent.
    assert_eq!(selection_text(&rows, (0, 0), (0, 2)), " ");
}

#[test]
fn mouse_cell_maps_through_area() {
    use crossterm::event::{self, KeyModifiers};
    let area = Rect::new(0, 5, 40, 10);
    let m = |col: u16, row: u16| event::MouseEvent {
        kind: event::MouseEventKind::Down(event::MouseButton::Left),
        column: col,
        row,
        modifiers: KeyModifiers::empty(),
    };
    // Inside: display row = scroll + offset, col = screen col.
    assert_eq!(
        mouse_display_cell(7, Some(area), 100, &m(3, 6)),
        Some((8, 3))
    );
    // Above the transcript area: ignored.
    assert_eq!(mouse_display_cell(7, Some(area), 100, &m(3, 4)), None);
    // Below the last cached row: ignored.
    assert_eq!(mouse_display_cell(97, Some(area), 100, &m(3, 9)), None);
    // Right edge is exclusive.
    assert_eq!(mouse_display_cell(0, Some(area), 100, &m(40, 5)), None);
    // No area recorded yet: ignored.
    assert_eq!(mouse_display_cell(0, None, 100, &m(0, 0)), None);
}

#[test]
fn banner_is_one_block_of_wordmark_rows() {
    // One Banner block holding the wordmark row, not an Info block:
    // TranscriptView inserts a blank gap line between blocks, which
    // would split the banner apart.
    let mut app = test_app();
    push_banner(&mut app);
    assert_eq!(app.transcript.len(), 1);
    let lines = app.transcript[0].lines();
    assert_eq!(lines.len(), 1);
    let muted = theme::muted_fg();
    let line = &lines[0];
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(
        text,
        format!("{}{BANNER}", transcript_indent()),
        "indent + banner row"
    );
    // Banner row is subtle chrome (the indent span is unstyled).
    assert!(line.spans.len() == 2 && line.spans[1].style.fg == Some(muted));
    assert!(
        text.contains("DEX (v") && text.ends_with(')'),
        "wordmark first, version after in parens"
    );
}

#[test]
fn thinking_stream_coalesces_and_closes_on_assistant_text() {
    let mut app = test_app();
    // Throttle window open, so every delta bumps the version.
    app.stream_last_flush = Instant::now() - STREAM_FLUSH_INTERVAL - Duration::from_millis(1);
    for chunk in ["Let me ", "think."] {
        append_sink_line(&mut app, crate::protocol::SinkLine::Thinking(chunk.into()));
    }
    assert!(app.thinking_open, "streaming deltas keep the block open");
    // The thinking bumps reset the shared throttle clock; reopen the
    // window so the assistant delta renders immediately.
    app.stream_last_flush = Instant::now() - STREAM_FLUSH_INTERVAL - Duration::from_millis(1);
    append_sink_line(
        &mut app,
        crate::protocol::SinkLine::Assistant("done".into()),
    );
    assert!(
        !app.thinking_open,
        "assistant text closes the thinking block"
    );
    assert_eq!(app.transcript.len(), 2);
    assert!(matches!(
        &app.transcript[0],
        TranscriptBlock::Thinking { text, .. } if text == "Let me think."
    ));
    assert!(matches!(
        &app.transcript[1],
        TranscriptBlock::Assistant { .. }
    ));
    assert!(app.assistant_open);
}

#[test]
fn thinking_only_deltas_stay_in_one_block() {
    let mut app = test_app();
    for chunk in ["a", "b", "c"] {
        append_sink_line(&mut app, crate::protocol::SinkLine::Thinking(chunk.into()));
    }
    assert_eq!(app.transcript.len(), 1);
    assert!(matches!(&app.transcript[0], TranscriptBlock::Thinking { text, .. } if text == "abc"));
    assert!(app.thinking_open);
}

#[test]
fn assistant_lines_buffer_until_the_throttle_window() {
    let mut app = test_app();
    // Inside a throttle window: just flushed, so nothing renders yet.
    app.stream_last_flush = Instant::now();
    append_sink_line(
        &mut app,
        crate::protocol::SinkLine::Assistant("hello".into()),
    );
    assert_eq!(app.assistant_pending, "hello\n");
    assert!(app.transcript.is_empty(), "nothing renders mid-window");

    // Window elapsed: the next delta flushes.
    app.stream_last_flush = Instant::now() - STREAM_FLUSH_INTERVAL - Duration::from_millis(1);
    append_sink_line(
        &mut app,
        crate::protocol::SinkLine::Assistant("world".into()),
    );
    assert!(app.assistant_pending.is_empty());
    assert!(app.assistant_open);
    // Two complete markdown lines inside one window keep their line
    // break (the daemon coalescer joins with '\n' as well).
    let TranscriptBlock::Assistant { lines, .. } = &app.transcript[0] else {
        panic!("expected assistant block");
    };
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(text.contains("hello"), "{text}");
    assert!(text.contains("world"), "{text}");
    assert!(
        !text.contains("helloworld"),
        "line break lost while buffering: {text}"
    );

    // A non-assistant line drains the buffer before its own block lands.
    // Fresh window again so the tail buffers instead of flushing.
    app.stream_last_flush = Instant::now();
    append_sink_line(
        &mut app,
        crate::protocol::SinkLine::Assistant("tail".into()),
    );
    assert_eq!(app.assistant_pending, "tail\n");
    append_sink_line(
        &mut app,
        crate::protocol::SinkLine::ToolInput {
            id: String::new(),
            input: "bash echo".into(),
        },
    );
    assert!(app.assistant_pending.is_empty());
    assert_eq!(app.transcript.len(), 2);
    let TranscriptBlock::Assistant { lines, .. } = &app.transcript[0] else {
        panic!("expected assistant block");
    };
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
        .collect();
    assert!(text.contains("tail"), "buffered tail flushed first: {text}");
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptBlock::Tool { .. })
    ));
}

#[test]
fn heading_keeps_top_air_across_flush_windows() {
    // A heading at a throttle edge used to butt against the flushed prose
    // (no top air) while the bottom air survived via the trailing blank.
    let mut app = test_app();
    let due = || Instant::now() - STREAM_FLUSH_INTERVAL - Duration::from_millis(1);
    app.stream_last_flush = due();
    append_sink_line(
        &mut app,
        crate::protocol::SinkLine::Assistant("intro text".into()),
    );
    assert!(app.assistant_open);
    app.stream_last_flush = due();
    append_sink_line(
        &mut app,
        crate::protocol::SinkLine::Assistant("## Heading".into()),
    );
    let TranscriptBlock::Assistant { lines, .. } = &app.transcript[0] else {
        panic!("expected assistant block");
    };
    let row = |l: &Line<'static>| {
        l.spans
            .iter()
            .map(|s| s.content.to_string())
            .collect::<String>()
    };
    let rows: Vec<String> = lines.iter().map(row).collect();
    assert_eq!(rows.len(), 3, "top air missing across flush: {rows:?}");
    assert!(rows[0].contains("intro text"), "{rows:?}");
    assert!(rows[1].trim().is_empty(), "{rows:?}");
    assert!(rows[2].contains("Heading"), "{rows:?}");
    assert!(!rows[2].contains('#'), "{rows:?}");
}

#[test]
fn assistant_blank_runs_collapse_to_one_air_row() {
    // Streaming blank lines between paragraphs collapse to a single air
    // row (CommonMark); each used to push its own blank `Line`.
    let mut app = test_app();
    let due = || Instant::now() - STREAM_FLUSH_INTERVAL - Duration::from_millis(1);
    for line in ["para one", "", "", "", "para two"] {
        app.stream_last_flush = due();
        append_sink_line(&mut app, crate::protocol::SinkLine::Assistant(line.into()));
    }
    flush_assistant(&mut app);
    let TranscriptBlock::Assistant { lines, .. } = &app.transcript[0] else {
        panic!("expected assistant block");
    };
    let rows: Vec<String> = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        })
        .collect();
    assert_eq!(rows.len(), 3, "blank run not collapsed: {rows:?}");
    assert!(rows[0].contains("para one"), "{rows:?}");
    assert!(rows[1].trim().is_empty(), "{rows:?}");
    assert!(rows[2].contains("para two"), "{rows:?}");
}

#[test]
fn thinking_closes_on_user_prompt() {
    // Steering-style interleave: a user prompt lands mid-turn.
    let mut app = test_app();
    append_sink_line(&mut app, crate::protocol::SinkLine::Thinking("h".into()));
    assert!(app.thinking_open);
    render_user_prompt(&mut app, "steer");
    assert!(!app.thinking_open);
}

#[test]
fn thinking_close_records_elapsed_duration() {
    let mut app = test_app();
    append_sink_line(&mut app, crate::protocol::SinkLine::Thinking("h".into()));
    assert!(matches!(
        &app.transcript[0],
        TranscriptBlock::Thinking { elapsed: None, .. }
    ));
    close_thinking(&mut app);
    assert!(matches!(
        &app.transcript[0],
        TranscriptBlock::Thinking {
            elapsed: Some(_),
            ..
        }
    ));
}

#[test]
fn activity_block_trails_the_tail_then_settles() {
    let mut app = test_app();
    app.busy = true;
    start_activity(&mut app);
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptBlock::Activity { settled: None, .. })
    ));
    // A tool block lands after it: the indicator must move below it.
    append_sink_line(
        &mut app,
        crate::protocol::SinkLine::ToolInput {
            id: String::new(),
            input: "bash cargo test".into(),
        },
    );
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptBlock::Activity { settled: None, .. })
    ));
    assert!(matches!(
        app.transcript[app.transcript.len() - 2],
        TranscriptBlock::Tool { .. }
    ));
    app.tool_state.last_usage = Some(4200);
    settle_activity(&mut app);
    match app.transcript.last() {
        Some(TranscriptBlock::Activity {
            settled: Some(summary),
            ..
        }) => {
            assert!(summary.contains("Worked for"), "{summary}");
            assert!(summary.contains("4.2k tokens"), "{summary}");
        }
        other => panic!("expected settled activity block, got {other:?}"),
    }
    // Settling again (e.g. a duplicate finish event) is a no-op.
    app.tool_state.last_usage = Some(1);
    settle_activity(&mut app);
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptBlock::Activity { .. })
    ));
    assert_eq!(app.transcript.len(), 2);
}

#[test]
fn thinking_text_caps_oldest_reasoning() {
    let mut app = test_app();
    for i in 0..4000 {
        append_sink_line(
            &mut app,
            crate::protocol::SinkLine::Thinking(format!("{i:06}abcdefghij")),
        );
    }
    let text = match app.transcript.last() {
        Some(TranscriptBlock::Thinking { text, .. }) => text.clone(),
        other => panic!("expected thinking tail, got {other:?}"),
    };
    assert!(
        text.len() <= THINKING_TEXT_CAP + THINKING_TEXT_SLACK,
        "stored thinking capped, got {}",
        text.len()
    );
    assert!(
        text.ends_with("003999abcdefghij"),
        "keeps the newest reasoning"
    );
    assert!(
        !text.contains("000000abcdefghij"),
        "drops the oldest reasoning"
    );
}

#[test]
fn selection_above_retrailed_activity_survives_streaming() {
    // Two rendered blocks (3 + 2 rows, one separator between) and the
    // open spinner after them: it starts at display row 6. A streaming
    // append lands an unwrapped block after the spinner, then the
    // spinner re-trails to the tail.
    for (sel_row, survives) in [(1, true), (6, false), (7, false)] {
        let mut app = test_app();
        app.busy = true;
        app.transcript = vec![
            TranscriptBlock::Assistant {
                stamp: 0,
                lines: vec![],
            },
            TranscriptBlock::Tool {
                stamp: 0,
                input: Line::default(),
                output: None,
                preview: Vec::new(),
                tool_arg: String::new(),
                tool_id: String::new(),
            },
            TranscriptBlock::Activity {
                stamp: 0,
                started: Instant::now(),
                settled: None,
            },
        ];
        let rows = |n| (0..n).map(|_| Line::default()).collect::<Vec<_>>();
        app.wrapped_cache = vec![
            WrappedBlock {
                stamp: 0,
                rows: rows(3),
                src_len: 0,
                open_len: 0,
                open_rows: 0,
                expanded: false,
            },
            WrappedBlock {
                stamp: 0,
                rows: rows(2),
                src_len: 0,
                open_len: 0,
                open_rows: 0,
                expanded: false,
            },
            WrappedBlock {
                stamp: 0,
                rows: rows(1),
                src_len: 0,
                open_len: 0,
                open_rows: 0,
                expanded: false,
            },
        ];
        app.transcript.push(TranscriptBlock::Assistant {
            stamp: 0,
            lines: vec![],
        });
        app.selection = Some(Selection {
            anchor: (sel_row, 0),
            end: (sel_row, 1),
            sticky: false,
            whole_line: false,
        });
        move_activity_to_tail(&mut app);
        // Rows above the spinner keep pointing at unchanged rows; the
        // spinner's own row (7) and the separator above it (6) moved.
        assert_eq!(app.selection.is_some(), survives, "row {sel_row}");
    }
}

#[test]
fn thinking_coalesces_behind_activity_and_closes_with_elapsed() {
    let mut app = test_app();
    app.busy = true;
    start_activity(&mut app);
    append_sink_line(&mut app, crate::protocol::SinkLine::Thinking("a".into()));
    append_sink_line(&mut app, crate::protocol::SinkLine::Thinking("b".into()));
    // One thinking block + trailing spinner, not a fragment per delta.
    assert_eq!(app.transcript.len(), 2);
    assert!(matches!(
        &app.transcript[0],
        TranscriptBlock::Thinking { text, .. } if text == "ab"
    ));
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptBlock::Activity { settled: None, .. })
    ));
    // Non-thinking line settles the duration even with the spinner last.
    append_sink_line(
        &mut app,
        crate::protocol::SinkLine::ToolInput {
            id: String::new(),
            input: "bash x".into(),
        },
    );
    assert!(matches!(
        &app.transcript[0],
        TranscriptBlock::Thinking {
            elapsed: Some(_),
            ..
        }
    ));
}

#[test]
fn tool_output_completes_open_tool_behind_activity() {
    let mut app = test_app();
    app.busy = true;
    start_activity(&mut app);
    append_sink_line(
        &mut app,
        crate::protocol::SinkLine::ToolInput {
            id: String::new(),
            input: "bash cargo test".into(),
        },
    );
    append_sink_line(
        &mut app,
        crate::protocol::SinkLine::ToolOutput {
            name: "bash".into(),
            id: String::new(),
            summary: "ok".into(),
            success: true,
            preview: vec![],
            duration: 0.0,
        },
    );
    // Open tool completed in place; no synthetic duplicate.
    assert_eq!(app.transcript.len(), 2);
    assert!(matches!(
        &app.transcript[0],
        TranscriptBlock::Tool {
            output: Some(_),
            ..
        }
    ));
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptBlock::Activity { settled: None, .. })
    ));
}

#[test]
fn tool_outputs_pair_by_id_when_interleaved() {
    // Parallel batches announce all inputs up front, so outputs land
    // out of order ([A B] [B A]): each output must complete its own
    // open block (matched by call id), never the tail. The reverse
    // completion order is deliberate — tail-pairing would attach B's
    // summary to A's block.
    let mut app = test_app();
    let input = |id: &str, text: &str| crate::protocol::SinkLine::ToolInput {
        id: id.to_string(),
        input: text.to_string(),
    };
    let output = |id: &str, summary: &str| crate::protocol::SinkLine::ToolOutput {
        id: id.to_string(),
        name: "read".into(),
        summary: summary.to_string(),
        success: true,
        preview: vec![],
        duration: 0.0,
    };
    append_sink_line(&mut app, input("call-A", "read a.rs"));
    append_sink_line(&mut app, input("call-B", "read b.rs"));
    append_sink_line(&mut app, output("call-B", "v 1 line"));
    append_sink_line(&mut app, output("call-A", "v 2 lines"));
    // No synthesized `▸ tool` blocks: both outputs found their inputs.
    assert_eq!(app.transcript.len(), 2);
    for (block, (arg, summary)) in app
        .transcript
        .iter()
        .zip([("a.rs", "v 2 lines"), ("b.rs", "v 1 line")])
    {
        let TranscriptBlock::Tool {
            tool_arg,
            tool_id,
            output,
            ..
        } = block
        else {
            panic!("expected Tool block");
        };
        assert_eq!(tool_arg, arg);
        assert!(tool_id == "call-A" || tool_id == "call-B");
        let out = output.as_ref().expect("block completed");
        let text: String = out.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains(summary), "wrong summary on {arg}: {text}");
    }
}

#[test]
fn rebuild_pairs_tool_results_with_assistant_calls_by_id() {
    // Journal replay: the assistant message already opened one block
    // per call, so each tool message must complete that block — not
    // open a second bare one. Rebuilt inputs also use `short_arg`
    // (`read a.rs`), matching the live path for highlighting.
    use crate::protocol::{ChatMessage, FunctionCall, LlmToolCall};
    let call = |id: &str, path: &str| LlmToolCall {
        id: id.to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "read".to_string(),
            arguments: format!(r#"{{"path":"{path}"}}"#),
        },
    };
    let mut app = test_app();
    app.messages = vec![
        ChatMessage::system("sys"),
        ChatMessage::assistant_calls(None, vec![call("call-A", "a.rs"), call("call-B", "b.rs")]),
        ChatMessage::tool_result("call-B", "v 1 line"),
        ChatMessage::tool_result("call-A", "v 2 lines"),
    ];
    rebuild_transcript(&mut app);
    // Exactly two blocks — no duplicate bare inputs — both completed.
    assert_eq!(app.transcript.len(), 2);
    for (block, (arg, id)) in app
        .transcript
        .iter()
        .zip([("a.rs", "call-A"), ("b.rs", "call-B")])
    {
        let TranscriptBlock::Tool {
            tool_arg,
            tool_id,
            output,
            ..
        } = block
        else {
            panic!("expected Tool block");
        };
        assert_eq!(tool_arg, arg);
        assert_eq!(tool_id, id);
        assert!(output.is_some(), "rebuilt block left open for {arg}");
    }
}

#[test]
fn chunked_replay_matches_one_shot_rebuild() {
    // §1: chunked startup replay (one message per chunk — every seam is
    // a chunk boundary) must render exactly what one-shot does: tool
    // ids thread across chunks, assistant text spanning a seam still
    // coalesces (no mid flush).
    use crate::protocol::{ChatMessage, FunctionCall, LlmToolCall};
    use std::collections::HashSet;
    let call = |id: &str| LlmToolCall {
        id: id.to_string(),
        call_type: "function".to_string(),
        function: FunctionCall {
            name: "read".to_string(),
            arguments: r#"{"path":"a.rs"}"#.to_string(),
        },
    };
    let messages = vec![
        ChatMessage::system("sys"),
        ChatMessage::user("hi"),
        ChatMessage::assistant("hello "),
        ChatMessage::assistant_calls(Some("doing".into()), vec![call("call-A")]),
        ChatMessage::assistant("world"),
        ChatMessage::tool_result("call-A", "v 1 line"),
    ];
    fn shape(app: &App) -> Vec<String> {
        app.transcript
            .iter()
            .map(|b| match b {
                TranscriptBlock::User { lines, .. } => format!("user:{}", lines.len()),
                TranscriptBlock::Assistant { lines, .. } => {
                    let text: String = lines
                        .iter()
                        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
                        .collect();
                    format!("assistant:{text}")
                }
                TranscriptBlock::Tool {
                    tool_id, output, ..
                } => format!("tool:{tool_id}:{}", output.is_some()),
                TranscriptBlock::Thinking { text, .. } => format!("thinking:{text}"),
                _ => "other".to_string(),
            })
            .collect()
    }
    let mut one_shot = test_app();
    one_shot.messages = messages.clone();
    rebuild_transcript(&mut one_shot);
    let mut chunked = test_app();
    chunked.messages = messages;
    let msgs = std::mem::take(&mut chunked.messages);
    let mut opened = HashSet::new();
    for chunk in msgs[1..].chunks(1) {
        render_message_slice(&mut chunked, chunk, &mut opened);
    }
    chunked.messages = msgs;
    flush_assistant(&mut chunked);
    chunked.autoscroll = true;
    assert_eq!(shape(&chunked), shape(&one_shot));
    // The tool block completed across the seam; assistant text around
    // the tool call renders in both halves (a tool call always splits
    // assistant blocks, chunked or not).
    let shapes = shape(&chunked);
    assert!(shapes.iter().any(|s| s == "tool:call-A:true"), "{shapes:?}");
    assert!(shapes.iter().any(|s| s.contains("hello")), "{shapes:?}");
    assert!(shapes.iter().any(|s| s.contains("world")), "{shapes:?}");
}

#[test]
fn assistant_coalesces_behind_activity() {
    let mut app = test_app();
    app.busy = true;
    start_activity(&mut app);
    app.stream_last_flush = Instant::now() - std::time::Duration::from_secs(1);
    append_sink_line(&mut app, crate::protocol::SinkLine::Assistant("one".into()));
    app.stream_last_flush = Instant::now() - std::time::Duration::from_secs(1);
    append_sink_line(&mut app, crate::protocol::SinkLine::Assistant("two".into()));
    flush_assistant(&mut app);
    let assistants = app
        .transcript
        .iter()
        .filter(|b| matches!(b, TranscriptBlock::Assistant { .. }))
        .count();
    assert_eq!(assistants, 1);
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptBlock::Activity { settled: None, .. })
    ));
}

#[test]
fn indent_transcript_line_adds_gutter() {
    let line = Line::from("test");
    let indented = indent_transcript_line(line);
    assert_eq!(indented.spans[0].content.as_ref(), transcript_indent());
}

#[test]
fn submitted_prompt_keeps_user_voice() {
    // Submitted words keep the composer's signature color, so your turns
    // read as yours against the assistant's default foreground. No
    // prompt glyph — bare text on the transcript margin.
    let mut app = test_app();
    render_user_prompt(&mut app, "hello\nworld");
    let lines = match app.transcript.last() {
        Some(TranscriptBlock::User { lines, .. }) => lines,
        other => panic!("expected user block, got {other:?}"),
    };
    assert_eq!(lines.len(), 2);
    for line in lines {
        assert!(
            line.spans
                .iter()
                .any(|s| s.style.fg == Some(theme::user_fg())),
            "every submitted row carries the voice color: {line:?}"
        );
        assert!(
            line.spans.iter().all(|s| !s.content.contains("▶")),
            "no prompt glyph in submitted rows: {line:?}"
        );
    }
}

#[test]
fn git_context_returns_empty_on_non_repo() {
    let (branch, dirty) = crate::runtime::format_runtime::git_context("/tmp/not-a-repo-12345");
    assert!(branch.is_none());
    assert!(!dirty);
}

#[test]
fn tool_preview_lines_are_indented_and_dimmed() {
    // Preview formatting is exercised through the Tool block produced by
    // `append_sink_line`; the stored preview lines must be indented and
    // dimmed exactly as before.
    let mut app = test_app();
    append_sink_line(
        &mut app,
        crate::protocol::SinkLine::ToolInput {
            id: String::new(),
            input: "bash echo hi".into(),
        },
    );
    append_sink_line(
        &mut app,
        crate::protocol::SinkLine::ToolOutput {
            name: "bash".into(),
            id: String::new(),
            summary: "v ok".into(),
            success: true,
            preview: vec!["src/main.rs".into(), "… +3 more lines".into()],
            duration: 0.0,
        },
    );
    let TranscriptBlock::Tool { preview, .. } = &app.transcript[0] else {
        panic!("expected Tool block");
    };
    assert_eq!(preview.len(), 2);
    for line in preview {
        assert!(line.spans.len() == 2); // indent gutter + content
        assert_eq!(line.spans[1].style.fg, Some(theme::tool_preview_fg()));
    }
    assert!(preview[0].spans[1].content.as_ref() == "  src/main.rs");
    assert!(preview[1].spans[1].content.as_ref() == "  … +3 more lines");
}

#[test]
fn read_preview_highlights_code_and_keeps_gutter_dim() {
    // Read snippets highlight by extension; the `{:>4}  ` gutter
    // stays dim for alignment.
    let mut app = test_app();
    append_sink_line(
        &mut app,
        crate::protocol::SinkLine::ToolInput {
            id: String::new(),
            input: "read src/main.rs:1-2".into(),
        },
    );
    append_sink_line(
        &mut app,
        crate::protocol::SinkLine::ToolOutput {
            name: "read".into(),
            id: String::new(),
            summary: "v 2 lines".into(),
            success: true,
            preview: vec!["   1  fn main() {}".into(), "… +1 more lines".into()],
            duration: 0.0,
        },
    );
    let TranscriptBlock::Tool { preview, .. } = &app.transcript[0] else {
        panic!("expected Tool block");
    };
    assert_eq!(preview.len(), 2);
    // Code row: indent + dim gutter + at least one highlighted span.
    // spans[0] is the transcript indent (unstyled), spans[1] the gutter.
    let gutter: String = preview[0].spans[1]
        .content
        .as_ref()
        .chars()
        .chain(
            preview[0]
                .spans
                .get(2)
                .map(|s| s.content.as_ref())
                .unwrap_or("")
                .chars(),
        )
        .collect();
    assert!(gutter.contains('1'), "{gutter}");
    assert_eq!(preview[0].spans[1].style.fg, Some(theme::tool_preview_fg()));
    assert!(
        preview[0].spans[2..]
            .iter()
            .any(|s| s.style.fg != Some(theme::tool_preview_fg())),
        "code should highlight, got {:?}",
        preview[0]
    );
    // Tail row stays dim (spans[0] is the unstyled indent gutter).
    assert!(preview[1].spans[1..]
        .iter()
        .all(|s| s.style.fg == Some(theme::tool_preview_fg())));
}

/// Write a minimal persisted session JSONL (same entry shapes
/// `Session::new`/`set_state` produce) so `apply_session_state` can be
/// exercised without touching the real session directory.
fn write_session_file(state_lines: &[&str]) -> PathBuf {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let mut h = DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    let tid = h.finish();
    let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "dex-apply-state-{}-{}-{}-{}.jsonl",
        std::process::id(),
        tid,
        nanos,
        nonce
    ));
    let mut lines = vec![r#"{"type":"session","version":1,"id":"statetest","timestamp":"2020-01-01T00:00:00Z","cwd":"/tmp"}"#.to_string()];
    lines.extend(state_lines.iter().map(|l| l.to_string()));
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

#[test]
fn apply_session_state_restores_model_from_session_file() {
    let path = write_session_file(&[
        r#"{"type":"session_state","id":"1","timestamp":"2020-01-01T00:00:01Z","key":"model","value":"restored-model"}"#,
    ]);

    let mut app = test_app();
    crate::ui::slash::apply_session_state(&mut app, Some(&path));

    assert_eq!(app.config.model, "restored-model");
    assert!(app
        .config
        .available_models
        .contains(&"restored-model".to_string()));
}

#[test]
fn apply_session_state_keeps_config_when_file_has_no_state_entries() {
    let path = write_session_file(&[]);

    let mut app = test_app();
    crate::ui::slash::apply_session_state(&mut app, Some(&path));

    assert_eq!(app.config.model, "test");
}

#[test]
fn apply_session_state_is_noop_without_a_session_path() {
    let mut app = test_app();
    crate::ui::slash::apply_session_state(&mut app, None);

    assert_eq!(app.config.model, "test");
}

#[test]
fn reset_session_state_clears_per_session_state() {
    let mut app = test_app();
    app.messages.push(crate::protocol::ChatMessage::user("hi"));
    app.transcript.push(TranscriptBlock::Info {
        stamp: 0,
        line: Line::from("old"),
    });
    app.tool_state.total_usage = 42;
    app.tool_state.total_cost = 1.5;
    app.tool_state.last_usage = Some(7);
    app.pending_steering.push("steer".into());
    app.pending_followups.push("follow".into());
    app.plan.steps.push(("step".into(), false));
    app.turn_start = 3;
    app.transcript.push(TranscriptBlock::Activity {
        stamp: 0,
        started: Instant::now(),
        settled: None,
    });
    app.scroll = 9;
    app.autoscroll = false;

    crate::ui::slash::reset_session_state(&mut app);

    assert!(app.messages.len() <= 1);
    assert!(app.transcript.is_empty());
    assert!(!app.assistant_open);
    assert!(app.autoscroll);
    assert_eq!(app.scroll, 0);
    assert_eq!(app.tool_state.total_usage, 0);
    assert_eq!(app.tool_state.total_cost, 0.0);
    assert!(app.tool_state.last_usage.is_none());
    assert!(app.pending_steering.is_empty());
    assert!(app.pending_followups.is_empty());
    assert!(app.plan.is_empty());
    assert_eq!(app.turn_start, 0);
    assert!(!app
        .transcript
        .iter()
        .any(|b| matches!(b, TranscriptBlock::Activity { .. })));
}

#[test]
fn slash_new_refuses_while_busy() {
    let mut app = test_app();
    app.busy = true;
    crate::ui::slash::handle_slash(&mut app, "/new");
    assert!(
            app.transcript
                .iter()
                  .any(|b| matches!(b, TranscriptBlock::Info { line, .. } if line.spans.iter().any(|s| s.content.contains("turn is running"))))
        );
}

#[test]
fn enter_expands_bare_picker_commands_instead_of_submitting() {
    // `/model`, `/provider`, `/resume` open a popup picker: Enter on the
    // bare form must expand to `"<cmd> "` (popup stays open) rather than
    // submit and print info into the transcript.
    for cmd in ["/model", "/provider", "/resume"] {
        let mut app = test_app();
        app.input = crate::ui::input::InputField::from_text(cmd);
        app.slash_selected = 5;
        assert!(
            crate::ui::slash::expand_bare_command(&mut app),
            "{cmd} must expand"
        );
        assert_eq!(app.input.text(), format!("{cmd} "));
        assert_eq!(app.slash_selected, 0);
    }
}

#[test]
fn enter_expansion_leaves_other_input_untouched() {
    // Argument-less commands (`/clear`), partial prefixes (`/mod`), and
    // inputs that already carry an argument keep the old path: the Enter
    // handler's completion step (not the bare-command expansion) owns
    // them.
    for input in ["/clear", "/mod", "/model foo", "/resume 0", "hello"] {
        let mut app = test_app();
        app.input = crate::ui::input::InputField::from_text(input);
        assert!(
            !crate::ui::slash::expand_bare_command(&mut app),
            "{input} must not expand"
        );
        assert_eq!(app.input.text(), input);
    }
}

#[test]
fn command_prefix_completes_to_bare_picker_form() {
    // `/mod` + completion → `/model `: the Enter handler sees a bare
    // picker form in `EXPAND_ON_ENTER` and holds the submit so the popup
    // stays open for the actual choice.
    let mut app = test_app();
    app.input = crate::ui::input::InputField::from_text("/mod");
    assert!(crate::ui::slash::complete_slash(&mut app));
    assert_eq!(app.input.text(), "/model ");
    assert!(crate::ui::slash::EXPAND_ON_ENTER.contains(&"/model"));
}

#[test]
fn picker_rows_show_items_not_repeated_commands() {
    // Inside a picker the popup rows show just the item (`gpt-5`), not
    // the repeated command (`/model gpt-5`); outside a picker the
    // command itself stays the label.
    let label = crate::ui::slash::suggestion_label;
    assert_eq!(label("/model ", "/model gpt-5"), "gpt-5");
    assert_eq!(label("/model gp", "/model gpt-5"), "gpt-5");
    assert_eq!(label("/provider ", "/provider opencode"), "opencode");
    assert_eq!(label("/resume ", "/resume 0"), "0");
    assert_eq!(label("/resume", "/resume 2"), "2");
    assert_eq!(label("/resume old", "/resume 2"), "2");
    assert_eq!(label("/", "/model"), "/model");
    assert_eq!(label("/cl", "/clear"), "/clear");
}

#[test]
fn esc_dismiss_discards_draft_and_closes_popup() {
    // Esc on an open popup discards the drafted slash command (which is
    // what closes the popup — it is derived from the input) and resets
    // the highlight, without submitting anything.
    let mut app = test_app();
    app.input = crate::ui::input::InputField::from_text("/mod");
    app.slash_selected = 2;
    assert!(crate::ui::slash::dismiss_slash(&mut app));
    assert_eq!(app.input.text(), "");
    assert_eq!(app.slash_selected, 0);
    assert!(crate::ui::slash::slash_suggestions(&app).is_empty());
    assert!(app.transcript.is_empty());
}

#[test]
fn slash_typing_narrows_to_matching_command() {
    // `/` lists every command; typing `r` narrows to `/resume` — the
    // popup filters on the input text, arrows only move the highlight.
    let mut app = test_app();
    app.input = crate::ui::input::InputField::from_text("/");
    let all = crate::ui::slash::slash_suggestions(&app);
    assert!(all.len() > 1);
    app.input = crate::ui::input::InputField::from_text("/r");
    let filtered = crate::ui::slash::slash_suggestions(&app);
    assert_eq!(
        filtered.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>(),
        vec!["/resume"]
    );
}

#[test]
fn resume_lists_sessions_regardless_of_content() {
    // `/resume` picks by index/time: a header-only session (no messages
    // yet) is listed too — resuming it just shows an empty transcript.
    let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("dex-resume-empty-{}", std::process::id()));
    let _env = crate::session::EnvGuard(vec![("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]);
    std::env::set_var("XDG_DATA_HOME", &dir);
    let session = crate::session::Session::new("/tmp".into(), Some("empty-one".into())).unwrap();
    let name = session.name().unwrap().to_string();
    drop(session);
    let mut app = test_app();
    app.input = crate::ui::input::InputField::from_text("/resume ");
    let suggestions = crate::ui::slash::slash_suggestions(&app);
    assert!(
        suggestions.iter().any(|(_, desc)| desc.contains(&name)),
        "header-only session must be listed: {suggestions:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn esc_dismiss_reports_when_no_popup_open() {
    // Plain text (or empty input) shows no popup: nothing to discard.
    for input in ["hello", ""] {
        let mut app = test_app();
        app.input = crate::ui::input::InputField::from_text(input);
        assert!(!crate::ui::slash::dismiss_slash(&mut app));
        assert_eq!(app.input.text(), input);
    }
}

#[test]
fn history_walk_resets_slash_selection() {
    // A walk replaces the composer wholesale; a stale popup highlight
    // would strand past filtered results once the user types again.
    let mut app = test_app();
    app.history.push("/clear".into());
    app.history.push("plain".into());
    app.slash_selected = 3;
    app.history_up();
    assert_eq!(app.slash_selected, 0);
    app.slash_selected = 3;
    app.history_down();
    assert_eq!(app.slash_selected, 0);
}
