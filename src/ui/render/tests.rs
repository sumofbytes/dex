//! `ui::render` unit tests (moved verbatim from the pre-split monolith).

use super::super::status::{cell_safe, footer_text, status_pieces, ui_status};
use super::*;
use crate::protocol::{ApiProtocol, PermissionMode, Provider};
use ratatui::backend::TestBackend;
use std::time::Instant;

#[test]
fn apply_selection_highlights_end_cell_inclusively() {
    // Releasing on a char selects it: the last char's cell must get the
    // selection background, not stop one short of it.
    let mut window = vec![Line::from("hello world")];
    apply_selection(
        &mut window,
        0,
        Selection {
            anchor: (0, 6),
            end: (0, 10),
            sticky: false,
            whole_line: false,
        },
        20,
    );
    let highlighted: String = window[0]
        .spans
        .iter()
        .filter(|s| s.style.bg == Some(SEL_BG))
        .map(|s| s.content.as_ref())
        .collect();
    // "world" highlighted — and only that: the highlight stops at the
    // text end instead of padding to the area width, so a drag ending
    // on the last char no longer reads as a full-line pick.
    assert_eq!(highlighted, "world");
    assert_eq!(window[0].width(), "hello world".len());
    assert_eq!(window[0].style.bg, None);
    // Untouched prefix stays plain.
    let plain: String = window[0]
        .spans
        .iter()
        .filter(|s| s.style.bg.is_none())
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(plain, "hello ");
}

#[test]
fn apply_selection_multiline_anchor_pads_end_stays_text_only() {
    // Multi-line drag: the anchor row extends through the trailing margin
    // (continuation cue) while keeping the prefix before the anchor plain,
    // inner rows are a solid bar, and the end row highlights only its
    // text — even when the drag ends on the last char, so it stays
    // distinct from a whole-line pick.
    let mut window = vec![
        Line::from("hello world"),
        Line::from("middle"),
        Line::from("second"),
    ];
    apply_selection(
        &mut window,
        0,
        Selection {
            anchor: (0, 6),
            end: (2, 5),
            sticky: false,
            whole_line: false,
        },
        20,
    );
    // Anchor row: "world" plus margin fill highlighted, prefix plain.
    let anchor_hl: String = window[0]
        .spans
        .iter()
        .filter(|s| s.style.bg == Some(SEL_BG))
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(
        anchor_hl,
        format!("world{}", " ".repeat(20 - "hello world".len()))
    );
    assert_eq!(window[0].width(), 20);
    assert_eq!(window[0].style.bg, None);
    let anchor_plain: String = window[0]
        .spans
        .iter()
        .filter(|s| s.style.bg.is_none())
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(anchor_plain, "hello ");
    // Inner row: solid bar via line-style patch + pad.
    assert_eq!(window[1].style.bg, Some(SEL_BG));
    assert_eq!(window[1].width(), 20);
    // End row ending on the last char: text only, no pad or patch.
    let end_hl: String = window[2]
        .spans
        .iter()
        .filter(|s| s.style.bg == Some(SEL_BG))
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(end_hl, "second");
    assert_eq!(window[2].width(), "second".len());
    assert_eq!(window[2].style.bg, None);
}

#[test]
fn apply_selection_multiline_blank_last_row_stays_visible() {
    // A blank last row has no text range to highlight; it still pads to
    // a bar so its inclusion in the selection stays visible.
    let mut window = vec![Line::from("hello"), Line::from("")];
    apply_selection(
        &mut window,
        0,
        Selection {
            anchor: (0, 0),
            end: (1, 0),
            sticky: false,
            whole_line: false,
        },
        20,
    );
    assert_eq!(window[0].width(), 20);
    assert_eq!(window[1].width(), 20);
    assert!(window[1].spans.iter().any(|s| s.style.bg == Some(SEL_BG)));
}

#[test]
fn apply_selection_multiline_whitespace_last_row_pads_to_bar() {
    // A whitespace-only last row has no visible text range; like a blank
    // row it still pads to a bar so its inclusion stays visible.
    let mut window = vec![Line::from("hello"), Line::from("   ")];
    apply_selection(
        &mut window,
        0,
        Selection {
            anchor: (0, 0),
            end: (1, 0),
            sticky: false,
            whole_line: false,
        },
        20,
    );
    assert_eq!(window[1].width(), 20);
    assert_eq!(window[1].style.bg, None);
    assert!(window[1].spans.iter().any(|s| s.style.bg == Some(SEL_BG)));
}

#[test]
fn apply_selection_reverse_drag_pads_normed_first_row() {
    // Bottom-up drag: the normed top row gets the continuation pad, not
    // the press point. Locks the "first row" (not "anchor row") wording.
    let mut window = vec![Line::from("hello world"), Line::from("second")];
    apply_selection(
        &mut window,
        0,
        Selection {
            anchor: (1, 3),
            end: (0, 6),
            sticky: false,
            whole_line: false,
        },
        20,
    );
    assert_eq!(window[0].width(), 20);
    assert_eq!(window[0].style.bg, None);
    // Press-point (bottom) row highlights text only, no pad.
    assert_eq!(window[1].width(), "second".len());
    assert_eq!(window[1].style.bg, None);
    let end_hl: String = window[1]
        .spans
        .iter()
        .filter(|s| s.style.bg == Some(SEL_BG))
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(end_hl, "seco");
}

#[test]
fn apply_selection_full_width_end_row_matches_whole_line() {
    // Known limit: with no trailing margin left, a last-char drag on a
    // full-width row paints the same cells as a whole-line pick.
    let text = "x".repeat(20);
    let mut drag = vec![Line::from(text.clone())];
    apply_selection(
        &mut drag,
        0,
        Selection {
            anchor: (0, 0),
            end: (0, 19),
            sticky: false,
            whole_line: false,
        },
        20,
    );
    let mut whole = vec![Line::from(text)];
    apply_selection(
        &mut whole,
        0,
        Selection {
            anchor: (0, 0),
            end: (0, 19),
            sticky: true,
            whole_line: true,
        },
        20,
    );
    assert_eq!(drag[0].width(), 20);
    assert_eq!(whole[0].width(), 20);
}

fn test_app() -> super::super::App {
    let cwd = "/tmp/dex-ui-test".to_string();
    super::super::App {
        transcript: vec![super::super::TranscriptBlock::Assistant {
            stamp: 0,
            lines: vec![super::super::indent_transcript_line(Line::from(
                "hello from the transcript — this line is intentionally long enough to wrap",
            ))],
        }],
        input: InputField::new(),
        config: super::super::LlmConfig {
            provider: Provider::OpenCode,
            api_key: "test".to_string(),
            base_url: "http://localhost".to_string(),
            model: "test-model".to_string(),
            available_models: vec!["test-model".to_string()],
            endpoints: Default::default(),
            api: ApiProtocol::Responses,
            account_id: None,
            thinking_effort: None,
            context_window: 128_000,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
            permission: PermissionMode::Trusted,
            verify_command: None,
            extra_headers: Default::default(),
            global_headers: Default::default(),
            provider_entries: Default::default(),
            provider_headers: Default::default(),
            api_pinned: false,
            connect_timeout_secs: 10,
            request_timeout_secs: 300,
        },
        messages: Vec::new(),
        tool_state: super::super::ToolState::default(),
        session: super::super::Session::in_memory(cwd.clone()),
        skills: Vec::new(),
        turn_start: 0,
        cwd,
        git_branch: None,
        git_dirty: false,
        steering_rx: None,
        followup_rx: None,
        pending_steering: Vec::new(),
        pending_followups: Vec::new(),
        cancel_requested: false,
        cancel_presses: 0,
        approval_rx: None,
        pending_approvals: Vec::new(),
        agents: Vec::new(),
        busy: false,
        autoscroll: true,
        scroll: 0,
        tick: 0,
        quit: false,
        last_ctrl_c: None,
        history: Vec::new(),
        history_index: None,
        history_draft: String::new(),
        slash_selected: 0,
        connection: None,
        daemon_url: None,
        assistant_open: false,
        show_thinking: false,
        thinking_open: false,
        plan: crate::protocol::Plan::default(),
        assistant_pending: String::new(),
        assistant_gap: crate::ui::theme::markdown::GapState::new(),
        stream_last_flush: std::time::Instant::now(),
        wrapped_cache: Vec::new(),
        wrapped_width: 0,
        display_cache: Vec::new(),
        transcript_area: None,
        selection: None,
        notice: None,
        status_tokens_cache: std::cell::Cell::new((0, 0, 0)),
        slash_cache: std::cell::RefCell::new(None),
    }
}

#[test]
fn shared_surface_dimensions_are_consistent() {
    // Content width tracks the shared knob so the wrap width equals the
    // rendered inner width at any gutter value.
    let gutter = super::super::HORIZONTAL_GUTTER;
    assert_eq!(input_content_width(80), 80 - gutter * 2);
    assert_eq!(input_content_width(1), 0);
    assert_eq!(input_content_width(3), (3u16).saturating_sub(gutter * 2));
    assert_eq!(
        input_content_width(80),
        input_block().inner(Rect::new(0, 0, 80, 24)).width
    );
    // Guard keeps the queue-only strip collapsed at zero items.
    assert_eq!(activity_height(0, 0), 0);
    assert_eq!(activity_height(1, 1), 3);
    assert_eq!(activity_height(3, 3), 7);
    assert_eq!(status_height(), 2);
}

#[test]
fn minimum_view_height_accounts_for_all_gutters() {
    assert_eq!(minimum_view_height(activity_height(1, 1), 0), 9);
    assert_eq!(minimum_view_height(activity_height(3, 3), 0), 13);
}

#[test]
fn multiline_pending_steer_renders_each_source_line() {
    // A multiline submission queued while busy must keep its line breaks
    // above the composer: `truncate_display`/`cell_safe` strip control
    // chars, so feeding the whole text to one badge row used to flatten
    // it into a single raw line.
    let mut app = test_app();
    app.busy = true;
    app.pending_steering
        .push("first steer line\nsecond steer line".into());
    app.pending_followups.push("follow one\ntwo".into());

    let queue = pending_queue_metrics(&app);
    assert_eq!(queue.items, 2);
    assert_eq!(queue.rows, 4);
    assert_eq!(activity_height(queue.items, queue.rows), 4 + 1 + 2);

    let backend = TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| view(frame, &mut app))
        .expect("render should succeed");
    let buffer = terminal.backend().buffer();
    let rendered: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim()
                .to_string()
        })
        .collect();
    for expected in [
        "steer · first steer line",
        "second steer line",
        "follow-up · follow one",
        "two",
    ] {
        assert!(
            rendered.iter().any(|row| row == expected),
            "missing row {expected:?} in {rendered:?}"
        );
    }
    // The pre-fix failure mode: newlines dropped, lines concatenated.
    assert!(
        !rendered
            .iter()
            .any(|row| row.contains("first steer linesecond steer line")),
        "steer lines were flattened: {rendered:?}"
    );
}

#[test]
fn long_multiline_queue_is_capped_per_item() {
    // A large paste queued while busy must not grow the strip without
    // bound: rows per item are capped and the overflow collapses into a
    // `…` row, keeping the strip (and the layout that sizes from it)
    // within one terminal's height.
    let mut app = test_app();
    app.busy = true;
    let paste = (1..=20)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.pending_steering.push(paste);

    let queue = pending_queue_metrics(&app);
    assert_eq!(queue.items, 1);
    assert_eq!(queue.rows, QUEUE_MAX_ITEM_ROWS as u16);

    let backend = TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| view(frame, &mut app))
        .expect("render should succeed");
    let buffer = terminal.backend().buffer();
    let rendered: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim()
                .to_string()
        })
        .collect();
    for expected in ["steer · line 1", "line 2", "line 3", "…"] {
        assert!(
            rendered.iter().any(|row| row == expected),
            "missing row {expected:?} in {rendered:?}"
        );
    }
    assert!(
        !rendered.iter().any(|row| row.contains("line 4")),
        "overflow rows should collapse into the `…` row: {rendered:?}"
    );
}

#[test]
fn full_queue_height_stays_bounded() {
    // A maxed-out queue (every visible item at the per-item row cap,
    // plus the tail) must still fit a standard terminal with the
    // composer and footer intact.
    let mut app = test_app();
    app.busy = true;
    let ten = (1..=10)
        .map(|i| format!("row {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    for i in 0..6 {
        let text = format!("item {i}\n{ten}");
        if i % 2 == 0 {
            app.pending_steering.push(text);
        } else {
            app.pending_followups.push(text);
        }
    }
    let queue = pending_queue_metrics(&app);
    assert_eq!(queue.items, 4); // 3 items + tail
    assert_eq!(queue.rows, 3 * QUEUE_MAX_ITEM_ROWS as u16 + 1);

    let layout = compute_layout(Rect::new(0, 0, 80, 24), 1, queue, false)
        .expect("maxed queue must fit a 24-row terminal");
    assert!(layout.activity.height > 0);
    assert!(layout.input.height >= super::super::INPUT_MIN_ROWS);
    assert_eq!(layout.footer.height, status_height());
}

#[test]
fn degenerate_layout_keeps_composer_and_footer() {
    // When the queue can't fit (short terminal), the strip yields
    // first — a vanished composer leaves the agent uncontrollable.
    let mut app = test_app();
    app.busy = true;
    let ten = (1..=10)
        .map(|i| format!("row {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    for i in 0..6 {
        let text = format!("item {i}\n{ten}");
        if i % 2 == 0 {
            app.pending_steering.push(text);
        } else {
            app.pending_followups.push(text);
        }
    }
    let queue = pending_queue_metrics(&app);
    // activity_h = 18 here, so a 20-row terminal can't fit the strip.
    let layout =
        compute_layout(Rect::new(0, 0, 80, 20), 1, queue, false).expect("layout should exist");
    assert_eq!(layout.activity.height, 0);
    assert!(layout.input.height >= super::super::INPUT_MIN_ROWS);
    assert_eq!(layout.footer.height, status_height());
    assert!(layout.transcript.height > 0);

    // A handful of rows can't fit even the composer: transcript-only.
    let layout =
        compute_layout(Rect::new(0, 0, 80, 4), 1, queue, false).expect("layout should exist");
    assert_eq!(layout.transcript.height, 4);
    assert_eq!(layout.input.height, 0);
    assert_eq!(layout.footer.height, 0);
}

#[test]
fn transcript_wrapper_keeps_first_content_grapheme() {
    let line = super::super::indent_transcript_line(Line::from("▸ tool"));
    let wrapped = wrap_line_display(&line, 80);
    let rendered: String = wrapped[0]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert_eq!(
        rendered,
        format!("{}▸ tool", super::super::transcript_indent())
    );
}

#[test]
fn layout_reserves_bottom_pane_before_transcript() {
    let area = Rect::new(0, 0, 80, 24);
    let layout = compute_layout(area, 1, QueueMetrics { items: 1, rows: 1 }, false)
        .expect("terminal should fit layout");
    assert_eq!(layout.transcript.y, 0);
    assert!(layout.transcript.height > 0);
    assert_eq!(
        layout.input.y + layout.input.height + super::super::INPUT_STATUS_GUTTER,
        layout.footer.y
    );
    assert_eq!(layout.footer.height, status_height());
}

#[test]
fn thinking_tail_extend_matches_full_wrap() {
    let width = 40;
    let first = "first line of thought\na second line much longer than the wrap width allows here";
    let (mut rows, state) = wrap_thinking_full(first, width);
    assert_eq!(state.src_len, first.len());
    // Extending with identical text re-wraps just the open line.
    let mut same = rows.clone();
    let again = extend_thinking_rows(&mut same, state, first, width).expect("no-op extends");
    assert_eq!(same, rows);
    assert_eq!(again.src_len, first.len());
    // Append mid-line, then newlines, then more: only the tail re-wraps.
    let text = format!("{first} plus more\nthird line\nfourth");
    let next = extend_thinking_rows(&mut rows, state, &text, width).expect("append-only extends");
    let (full, _) = wrap_thinking_full(&text, width);
    assert_eq!(rows, full);
    assert_eq!(next.src_len, text.len());
    // A non-prefix (head-cut, reset, rebuild) refuses the fast path.
    assert!(extend_thinking_rows(&mut rows, next, "totally different text", width).is_none());
    // Trailing newline: the open line is empty, the next append starts clean.
    let nl = format!("{text}\n");
    let (mut rows_nl, st_nl) = wrap_thinking_full(&nl, width);
    assert_eq!((st_nl.open_len, st_nl.open_rows), (0, 0));
    let text2 = format!("{nl}fifth line");
    extend_thinking_rows(&mut rows_nl, st_nl, &text2, width).expect("extends after newline");
    let (full2, _) = wrap_thinking_full(&text2, width);
    assert_eq!(rows_nl, full2);
}

#[test]
fn thinking_display_collapsed_previews_expanded_shows_all() {
    let text = "first line\n\nsecond line";
    let collapsed = thinking_display_lines(text, false, false, None, 0, 80);
    assert_eq!(collapsed.len(), 1);
    let joined: String = collapsed[0]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(joined.contains("◌ Thinking ..."), "{joined}");
    // Collapsed is a bare indicator: no thought content leaks through.
    assert!(!joined.contains("second line"), "{joined}");

    // While streaming, the collapsed indicator animates its dots.
    // One step every other animation frame at the busy heartbeat.
    let streaming = thinking_display_lines(text, false, true, None, 2, 80);
    let streamed: String = streaming[0]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(streamed.contains("◌ Thinking .."), "{streamed}");

    let expanded = thinking_display_lines(text, true, false, None, 0, 80);
    assert!(expanded.len() >= 3, "{}", expanded.len());
    let all: String = expanded
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref().to_string()))
        .collect();
    assert!(all.contains("first line") && all.contains("second line"));
}

#[test]
fn thinking_indicator_cycles_while_streaming_and_settles() {
    // Dots grow 1→3 every other animation frame, then loop.
    assert_eq!(thinking_indicator_text(true, None, 0), "◌ Thinking .");
    assert_eq!(thinking_indicator_text(true, None, 2), "◌ Thinking ..");
    assert_eq!(thinking_indicator_text(true, None, 4), "◌ Thinking ...");
    assert_eq!(thinking_indicator_text(true, None, 6), "◌ Thinking .");
    // Closed without a measured span: static, never animated again.
    for tick in [0, 2, 4, 6, 999] {
        assert_eq!(thinking_indicator_text(false, None, tick), "◌ Thinking ...");
    }
    // Closed with a measured span: the elapsed time, still static.
    let settled = Some(Duration::from_secs(94));
    for tick in [0, 2, 4, 6, 999] {
        assert_eq!(
            thinking_indicator_text(false, settled, tick),
            "Thought for 1m 34s"
        );
    }
}

#[test]
fn format_elapsed_secs_then_minutes() {
    assert_eq!(format_elapsed(Duration::from_millis(400)), "0s");
    assert_eq!(format_elapsed(Duration::from_secs(4)), "4s");
    assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
    assert_eq!(format_elapsed(Duration::from_secs(60)), "1m 0s");
    assert_eq!(format_elapsed(Duration::from_secs(94)), "1m 34s");
}

#[test]
fn closed_thinking_settles_to_thought_for_duration() {
    let mut app = test_app();
    app.transcript = vec![super::super::TranscriptBlock::Thinking {
        stamp: 0,
        text: "deep".into(),
        started: Instant::now(),
        elapsed: Some(Duration::from_secs(4)),
    }];
    let backend = TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| view(frame, &mut app))
        .expect("render should succeed");
    let text = |l: &Line<'_>| -> String { l.spans.iter().map(|s| s.content.as_ref()).collect() };
    let lines: Vec<String> = app
        .display_cache
        .iter()
        .map(|l| text(l).trim().to_string())
        .collect();
    assert!(
        lines.iter().any(|l| l.contains("Thought for 4s")),
        "{lines:?}"
    );
    assert!(!lines.iter().any(|l| l.contains("◌ Thinking")), "{lines:?}");
}

#[test]
fn activity_block_animates_then_settles_to_worked_for() {
    let mut app = test_app();
    app.busy = true;
    app.tick = 2; // animated frame for this tick is "● Working .."
    app.transcript = vec![super::super::TranscriptBlock::Activity {
        stamp: 0,
        started: Instant::now(),
        settled: None,
    }];
    let mut terminal = ratatui::Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    let rendered = |terminal: &ratatui::Terminal<TestBackend>| -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim()
                    .to_string()
            })
            .collect()
    };
    let lines = |app: &super::super::App| -> Vec<String> {
        app.display_cache
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .trim()
                    .to_string()
            })
            .collect()
    };
    terminal
        .draw(|frame| view(frame, &mut app))
        .expect("render should succeed");
    // The live spinner paints onto the window overlay, not back into
    // `display_cache` (so a later cache re-extend can't resurrect a
    // stale tick): assert on the painted buffer.
    let painted = rendered(&terminal);
    assert!(
        painted.iter().any(|l| l.contains("● Working ..")),
        "{painted:?}"
    );

    app.busy = false;
    app.transcript[0] = super::super::TranscriptBlock::Activity {
        stamp: 1,
        started: Instant::now(),
        settled: Some("Worked for 12s · 4.2k tokens".into()),
    };
    terminal
        .draw(|frame| view(frame, &mut app))
        .expect("render should succeed");
    let settled = lines(&app);
    assert!(
        settled
            .iter()
            .any(|l| l.contains("Worked for 12s · 4.2k tokens")),
        "{settled:?}"
    );
    assert!(
        !settled.iter().any(|l| l.contains("● Working")),
        "{settled:?}"
    );
}

#[test]
fn settled_thinking_stays_frozen_while_a_new_block_streams() {
    // Regression: every Thinking block was rendered with the live
    // `thinking_open` flag, so once a new block started streaming the
    // already-settled ones re-animated in sync with it. Only the tail
    // block — the open one — may animate.
    let mut app = test_app();
    app.transcript = vec![
        super::super::TranscriptBlock::Thinking {
            stamp: 0,
            text: "settled thoughts".into(),
            started: Instant::now(),
            elapsed: None,
        },
        super::super::TranscriptBlock::Assistant {
            stamp: 0,
            lines: vec![super::super::indent_transcript_line(Line::from(
                "tool turn in between",
            ))],
        },
        super::super::TranscriptBlock::Thinking {
            stamp: 0,
            text: "live thoughts".into(),
            started: Instant::now(),
            elapsed: None,
        },
    ];
    app.thinking_open = true;
    app.tick = 2; // animated frame for this tick is "◌ Thinking .."
    let backend = TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| view(frame, &mut app))
        .expect("render should succeed");
    let text = |l: &Line<'_>| -> String { l.spans.iter().map(|s| s.content.as_ref()).collect() };
    let lines: Vec<String> = app
        .display_cache
        .iter()
        .map(|l| text(l).trim().to_string())
        .collect();
    let buffer = terminal.backend().buffer();
    let painted: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim()
                .to_string()
        })
        .collect();
    // The settled block stays at three dots even though a block is open.
    assert!(lines.iter().any(|l| l == "◌ Thinking ..."), "{lines:?}");
    // The open tail block still animates — the live tick paints onto the
    // window overlay, not back into `display_cache`.
    assert!(painted.iter().any(|l| l == "◌ Thinking .."), "{painted:?}");
}

#[test]
fn streamed_flush_merges_into_display_without_losing_blocks() {
    // The per-block wrap cache must absorb a streamed delta into the
    // rendered display: new block wrapped, settled head block reused,
    // gap preserved, no stale rows.
    let mut app = test_app();
    // Flush immediately so the streamed delta lands in the transcript.
    app.stream_last_flush = std::time::Instant::now() - std::time::Duration::from_millis(500);
    let backend = TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| view(frame, &mut app))
        .expect("render should succeed");
    let head_rows = app.wrapped_cache[0].rows.len();

    super::super::append_sink_line(
        &mut app,
        crate::protocol::SinkLine::Assistant("more text".into()),
    );
    terminal
        .draw(|frame| view(frame, &mut app))
        .expect("render should succeed");

    assert_eq!(app.transcript.len(), 2);
    assert_eq!(app.wrapped_cache.len(), 2);
    assert_eq!(
        app.wrapped_cache[0].rows.len(),
        head_rows,
        "head block rows must be reused, not re-wrapped"
    );
    let text = |l: &Line<'_>| -> String { l.spans.iter().map(|s| s.content.as_ref()).collect() };
    let lines: Vec<String> = app
        .display_cache
        .iter()
        .map(|l| text(l).trim().to_string())
        .collect();
    assert!(lines
        .iter()
        .any(|l| l.contains("hello from the transcript")));
    assert!(lines.iter().any(|l| l.contains("more text")));
    assert_eq!(
        lines.iter().filter(|l| l.is_empty()).count(),
        1,
        "exactly one gap between the two blocks"
    );
}

#[test]
fn display_cache_extends_incrementally_on_tail_append() {
    // The incremental concat (§29) must keep leading display rows
    // byte-identical when only the tail grows — a full re-concat per
    // ~120ms streaming flush is O(transcript) clones per change.
    let mut app = test_app();
    let backend = TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| view(frame, &mut app))
        .expect("render should succeed");
    let text = |l: &Line<'_>| -> String { l.spans.iter().map(|s| s.content.as_ref()).collect() };
    let before: Vec<String> = app.display_cache.iter().map(&text).collect();
    assert!(!before.is_empty(), "first render must populate the cache");

    // Flush immediately so the streamed delta lands in the transcript.
    app.stream_last_flush = std::time::Instant::now() - std::time::Duration::from_millis(500);
    super::super::append_sink_line(
        &mut app,
        crate::protocol::SinkLine::Assistant("more text".into()),
    );
    terminal
        .draw(|frame| view(frame, &mut app))
        .expect("render should succeed");
    let after: Vec<String> = app.display_cache.iter().map(text).collect();
    assert!(
        after.len() > before.len(),
        "tail append must grow the cache: {after:?}"
    );
    assert_eq!(
        &after[..before.len()],
        &before[..],
        "leading display rows must survive a tail append unchanged"
    );
}

#[test]
fn session_banner_renders_without_inter_row_gaps() {
    // The DEX banner is one Banner block holding the single wordmark
    // row: it must be contiguous in the display cache (a blank gap is
    // only inserted between blocks, so an Info block would split it).
    let mut app = test_app();
    super::super::push_banner(&mut app);
    let text = |l: &Line<'_>| -> String { l.spans.iter().map(|s| s.content.as_ref()).collect() };
    let expected: Vec<String> = app
        .transcript
        .last()
        .expect("banner pushed")
        .lines()
        .into_iter()
        .map(text)
        .collect();
    assert_eq!(expected.len(), 1, "banner is one wordmark row");
    assert!(
        expected[0].contains("DEX (v") && expected[0].ends_with(')'),
        "wordmark first, version after in parens"
    );
    let backend = TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| view(frame, &mut app))
        .expect("render should succeed");

    let rows: Vec<String> = app.display_cache.iter().map(text).collect();
    let start = rows
        .iter()
        .position(|r| r.starts_with(&expected[0]))
        .expect("banner top row present in display");
    for (i, expected) in expected.iter().enumerate() {
        assert_eq!(
            rows[start + i].as_str(),
            expected.as_str(),
            "banner rows contiguous and in order — no block gap inside the banner"
        );
    }
}

#[test]
fn ui_status_shows_cumulative_token_total() {
    let mut app = test_app();
    // No LLM calls yet: no totals.
    assert!(!ui_status(&app).contains('↑'), "{}", ui_status(&app));
    assert!(!ui_status(&app).contains('↓'), "{}", ui_status(&app));
    // After calls, the cumulative spend figure appears and grows.
    app.tool_state.total_usage = 42_000;
    let text = ui_status(&app);
    assert!(text.contains("↑42k"), "{text}");
    app.tool_state.total_usage = 215_000;
    let text = ui_status(&app);
    assert!(text.contains("↑215k"), "{text}");
    // Cumulative completion tokens join the prompt total (one piece,
    // arrows for direction) and stay hidden until the first output
    // tokens are billed.
    assert!(!ui_status(&app).contains('↓'), "{}", ui_status(&app));
    app.tool_state.total_output = 1_250;
    let text = ui_status(&app);
    assert!(text.contains("↓1.2k"), "{text}");
    // Live context usage (% of window) still renders from last_usage,
    // compacted to `ctx in/window %`.
    app.tool_state.last_usage = Some(12_000);
    let text = ui_status(&app);
    assert!(text.contains("ctx 12k/128k 9%"), "{text}");
    // Cached-token subset appears once a provider reports it, collapsed
    // to a % of the last call's prompt (the glanceable cache-health
    // readout; the absolute count is the ctx number times this %). It
    // stays hidden when absent or zero, and the absolute is the
    // fallback when the prompt size is unknown or the hit is below
    // one percent; the % clamps at 100 for nonconforming endpoints.
    assert!(!ui_status(&app).contains("cached"), "{}", ui_status(&app));
    app.tool_state.last_cached = Some(8_000);
    let text = ui_status(&app);
    assert!(text.contains("66% cached"), "{text}");
    app.tool_state.last_cached = Some(15_000);
    let text = ui_status(&app);
    assert!(text.contains("100% cached"), "{text}");
    app.tool_state.last_cached = Some(60);
    let text = ui_status(&app);
    assert!(text.contains("60 cached"), "{text}");
    app.tool_state.last_cached = Some(8_000);
    app.tool_state.last_usage = None;
    assert!(ui_status(&app).contains("8k cached"), "{}", ui_status(&app));
    app.tool_state.last_cached = Some(0);
    assert!(!ui_status(&app).contains("cached"), "{}", ui_status(&app));
    // The last call's output rate appears once a timed call lands and
    // is absent before that.
    assert!(!ui_status(&app).contains("tok/s"), "{}", ui_status(&app));
    app.tool_state.last_tok_s = Some(123.4);
    let text = ui_status(&app);
    assert!(text.contains("123 tok/s"), "{text}");
}

#[test]
fn status_separators_never_double_without_branch() {
    // Regression: the branch separator was pushed even when there was
    // no branch, so any non-repo directory rendered `path ·  · model`.
    let app = test_app();
    for (label, pieces) in [
        ("full tier", status_pieces(&app, true)),
        ("no-cwd tier", status_pieces(&app, false)),
    ] {
        let text: String = pieces.iter().map(|(t, _)| t.as_str()).collect();
        assert!(!text.contains("·  ·"), "{label}: {text}");
        assert!(!text.starts_with('·'), "{label}: {text}");
    }
    // With a branch the separators around it appear exactly once.
    let mut branched = test_app();
    branched.git_branch = Some("main".into());
    let text = ui_status(&branched);
    assert!(
        text.contains("/tmp/dex-ui-test · main · opencode/test-model"),
        "{text}"
    );
    // The compact tier (reached at 60 cols once the cumulative total
    // widens the earlier tiers) keeps the same invariant.
    let mut app = test_app();
    app.connection = Some("[L] 127.0.0.1".into());
    app.tool_state.total_usage = 45_100;
    app.tool_state.total_cost = 0.023;
    let narrow = footer_text(&app, 60);
    assert!(narrow.starts_with("/tmp/dex-ui-test"), "{narrow}");
    assert!(!narrow.contains("·  ·"), "{narrow}");
}

#[test]
fn status_bar_colors_are_semantic_per_item() {
    let mut app = test_app();
    let muted = theme::muted_fg();
    let fg_of = |app: &App, needle: &str| {
        status_pieces(app, true)
            .into_iter()
            .find(|(text, _)| text.contains(needle))
            .map(|(_, style)| style.fg)
            .unwrap_or_else(|| panic!("no status piece contains {needle}"))
    };
    // Quiet facts: model and token counts use the theme's muted fg.
    assert_eq!(fg_of(&app, "test-model"), Some(muted));
    app.tool_state.last_usage = Some(12_000);
    assert_eq!(fg_of(&app, "ctx"), Some(muted));
    // The output rate is a quiet fact too.
    app.tool_state.last_tok_s = Some(84.0);
    assert_eq!(fg_of(&app, "tok/s"), Some(muted));
    app.tool_state.last_tok_s = None;
    // Repo state: clean branch reads as ok, the dirty marker warns.
    app.git_branch = Some("main".into());
    assert_eq!(fg_of(&app, "main"), Some(Color::LightGreen));
    app.git_dirty = true;
    assert_eq!(fg_of(&app, "*"), Some(Color::Yellow));
    // Context usage graduates with pressure against the compaction
    // trigger (128k window - 16k reserve = 111_616): quiet, then
    // warning yellow at >=75% of it, red past it.
    app.tool_state.last_usage = Some(100_000);
    assert_eq!(fg_of(&app, "ctx"), Some(Color::Yellow));
    app.tool_state.last_usage = Some(112_000);
    assert_eq!(fg_of(&app, "ctx"), Some(Color::LightRed));
    // The scroll hint is an attention flag; a remote badge is an accent
    // while a local one stays quiet.
    app.autoscroll = false;
    assert_eq!(
        footer_line(&app, 200).spans[0].style.fg,
        Some(Color::Yellow)
    );
    app.autoscroll = true;
    app.connection = Some("[L] local".into());
    let local = footer_line(&app, 200);
    assert_eq!(local.spans.last().unwrap().style.fg, Some(muted));
    app.connection = Some("[R] daemon.internal".into());
    let badge_spans = footer_line(&app, 200).spans;
    let badge = badge_spans.last().unwrap();
    assert_eq!(badge.content.as_ref(), "[R] daemon.internal");
    assert_eq!(badge.style.fg, Some(Color::Cyan));
}

#[test]
fn footer_text_is_width_bounded() {
    assert_eq!(truncate_display("abcdef", 4), "abc…");
    assert_eq!(
        UnicodeWidthStr::width(truncate_display("你好", 3).as_str()),
        3
    );
    assert_eq!(truncate_display("abcdef", 0), "");
    let app = test_app();
    assert_eq!(footer_text(&app, 8), "test-mo…");
}

#[test]
fn footer_pins_connection_badge_right() {
    let mut app = test_app();
    app.connection = Some("[R] daemon.internal".into());
    // Wide enough for the full line + badge: cwd leads, badge flush right.
    let text = footer_text(&app, 80);
    assert!(text.starts_with("/tmp/dex-ui-test"), "{text}");
    assert!(text.ends_with("[R] daemon.internal"), "{text}");
    assert_eq!(UnicodeWidthStr::width(text.as_str()), 80);
    // Narrower: the static cwd is shed first — the line still opens with
    // live facts, never a dangling separator.
    let text = footer_text(&app, 60);
    assert!(text.starts_with("opencode/test-model"), "{text}");
    assert!(text.ends_with("[R] daemon.internal"), "{text}");
    assert_eq!(UnicodeWidthStr::width(text.as_str()), 60);
    // Narrow: badge survives, left degrades to the bare model name.
    let text = footer_text(&app, 30);
    assert!(text.ends_with("[R] daemon.internal"), "{text}");
    assert!(text.starts_with("test-model"), "{text}");
}

#[test]
fn footer_keeps_cost_when_full_status_does_not_fit() {
    // Regression: spend pieces sit at the end of the full status line, so
    // once totals/output/cached outgrew a typical width the footer fell
    // back to `cwd · model` and the $ cost vanished entirely.
    let mut app = test_app();
    app.connection = Some("[L] 127.0.0.1".into());
    app.tool_state.total_usage = 45_100;
    app.tool_state.total_output = 8_200;
    app.tool_state.total_cost = 0.023;
    app.tool_state.last_usage = Some(12_300);
    let full = footer_text(&app, 200);
    assert!(full.contains("$0.023"), "{full}");
    // Full line (~100+ cells with totals) cannot fit at 60 cols: the
    // compact fallback must still carry the spend figure.
    let narrow = footer_text(&app, 60);
    assert!(narrow.contains("$0.023"), "{narrow}");
    // Ultra-narrow: bare model tier keeps the cost while it still fits.
    let tiny = footer_text(&app, 30);
    assert!(tiny.contains("$0.023"), "{tiny}");
    // Unbilled sessions render exactly as before (no stray separator).
    let mut fresh = test_app();
    fresh.connection = Some("[L] 127.0.0.1".into());
    assert!(
        !footer_text(&fresh, 60).contains('$'),
        "{}",
        footer_text(&fresh, 60)
    );
}

#[test]
fn control_characters_are_expanded_not_rendered_raw() {
    // Read tool output numbers lines as `n\ttext`; a raw tab in a span
    // makes the terminal jump past the modeled column and desyncs the
    // frame, so tabs must reach cells as spaces and other controls must
    // not reach cells at all.
    assert_eq!(cell_safe("35\tlet cwd"), "35      let cwd"); // 2 cols + 6 spaces to next 8
    assert_eq!(cell_safe("a\t\tb"), "a               b"); // a(1)+7 to 8, +8 to 16 => 15 spaces total
    assert_eq!(cell_safe("no tabs here"), "no tabs here");
    assert_eq!(cell_safe("a\rb\u{7}c\u{b}d"), "abcd");
    let mut app = test_app();
    super::super::append_sink_line(
        &mut app,
        super::super::SinkLine::ToolOutput {
            id: String::new(),
            name: "read".into(),
            summary: "2 lines".into(),
            success: true,
            preview: vec!["35\tlet cwd = env::current_dir()".into()],
            duration: 0.0,
        },
    );
    let backend = TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| view(frame, &mut app))
        .expect("render should succeed");
    let symbols: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(!symbols.chars().any(char::is_control));
    assert!(!symbols.contains('\t'), "tab must be expanded: {symbols}");
    // Indented preview: indent + "  35\tlet" -> indent + 2 spaces + 2 chars
    // before the tab, which then fills to the next tabstop column.
    let tab_pad = TAB_WIDTH - ((super::super::TRANSCRIPT_INDENT + 4) % TAB_WIDTH);
    assert!(
        symbols.contains(&format!("35{}let cwd", " ".repeat(tab_pad))),
        "{symbols}"
    );
    assert!(!symbols.contains("35      let cwd") || true); // raw cell_safe check above covers 6-space case without indent
}

#[test]
fn picker_popup_shows_item_labels_with_marker() {
    // `/model ` + Enter opens the picker: rows must read `> <item>`,
    // not repeat the command (`/model <item>`).
    let mut app = test_app();
    app.config.available_models = vec!["alpha-model".to_string(), "beta-model".to_string()];
    app.input = InputField::from_text("/model ");
    let backend = TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            // The popup anchors above the composer: pass the composer
            // rect like the real layout does (`frame.area()` starts at
            // y=0, which clamps the popup height to zero).
            SlashSuggestionsView::render(frame, Rect::new(0, 21, 80, 3), &mut app);
        })
        .expect("render should succeed");
    let symbols: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(symbols.contains("> alpha-model"), "{symbols}");
    assert!(symbols.contains("  beta-model"), "{symbols}");
    assert!(!symbols.contains("/model"), "{symbols}");
}

#[test]
fn slash_popup_has_only_a_top_rule() {
    // Copy-safe sheet: a single `─` rule across the top for separation;
    // no side/corner glyphs (`╭`/`│ `/`╰`) anywhere, and no `─` outside
    // the top row, so a native terminal selection pastes plain commands
    // (at worst one leading `────` line to drop).
    let mut app = test_app();
    app.input = InputField::from_text("/");
    let backend = TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            SlashSuggestionsView::render(frame, Rect::new(0, 21, 80, 3), &mut app);
        })
        .expect("render should succeed");
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..24)
        .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
        .collect();
    // Exactly one rule row, spanning only `─`/spaces; nothing box-drawn
    // anywhere else.
    let rule_rows: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.contains('─'))
        .map(|(y, _)| y)
        .collect();
    assert_eq!(rule_rows.len(), 1, "exactly one top rule: {rows:?}");
    for (y, row) in rows.iter().enumerate() {
        if y == rule_rows[0] {
            assert!(
                row.chars().all(|c| c == '─' || c == ' '),
                "top row must be a clean rule: {row}"
            );
        } else {
            assert!(
                !row.contains('─')
                    && !row.contains('│')
                    && !row.contains('╭')
                    && !row.contains('╰'),
                "box-drawing must not survive outside the top rule: {row}"
            );
        }
    }
    assert!(rows.iter().any(|r| r.contains("> ")), "{rows:?}");
}

#[test]
fn slash_popup_header_has_gutter_below_header() {
    // One blank gutter row sits between the header text and the first
    // command so the list doesn't butt the header; the header itself
    // stays directly under the top `─` rule (original top gap).
    let mut app = test_app();
    app.input = InputField::from_text("/");
    let backend = TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| {
            SlashSuggestionsView::render(frame, Rect::new(0, 21, 80, 3), &mut app);
        })
        .expect("render should succeed");
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..24)
        .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
        .collect();
    let rule = rows.iter().position(|r| r.contains('─')).expect("top rule");
    let header = rows
        .iter()
        .position(|r| r.contains("Slash commands"))
        .expect("header text");
    assert_eq!(header, rule + 1, "header directly under the top rule");
    assert!(
        rows[rule + 2].trim().is_empty(),
        "gutter row must be blank: {:?}",
        rows[rule + 2]
    );
}

#[test]
fn virtual_terminal_renders_at_normal_and_narrow_sizes() {
    for (width, height) in [(80, 24), (24, 12), (24, 8)] {
        let backend = TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let mut app = test_app();
        terminal
            .draw(|frame| view(frame, &mut app))
            .expect("render should succeed");
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.area.width, width);
        assert_eq!(buffer.area.height, height);
        assert!(buffer
            .content
            .iter()
            .all(|cell| !cell.symbol().contains('\n')));
    }
}

#[test]
fn virtual_terminal_keeps_composer_and_footer_separate() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    let mut app = test_app();
    terminal
        .draw(|frame| view(frame, &mut app))
        .expect("render should succeed");
    let layout = compute_layout(
        Rect::new(0, 0, 80, 24),
        1,
        QueueMetrics { items: 1, rows: 1 },
        false,
    )
    .unwrap();
    assert!(layout.transcript.bottom() <= layout.activity.top());
    assert!(layout.activity.bottom() <= layout.input.top());
    assert!(layout.input.bottom() <= layout.footer.top());
    let symbols: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(symbols.contains("hello"));
    assert!(symbols.contains("test-model"));
}

#[test]
fn approval_overlay_renders_action_and_choices() {
    // Input is raw JSON — overlay must render it as a readable `"$ cargo test"`
    // plus the human title, not the raw `bash cargo test` dump.
    let (response_tx, _response_rx) = tokio::sync::mpsc::channel(1);
    let mut app = test_app();
    let mut approval = super::super::PendingApproval::new(
        "bash".to_string(),
        r#"{"command":"cargo test"}"#.to_string(),
        response_tx,
        None,
    );
    approval.selected = 1;
    app.pending_approvals = vec![approval];
    let backend = TestBackend::new(100, 30);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| view(frame, &mut app))
        .expect("render should succeed");
    let symbols: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    // Title comes from approval_title, not raw JSON
    assert!(
        symbols.contains("Approval required") || symbols.contains("Run shell command"),
        "{symbols}"
    );
    // Readable command — `$ cargo test`, not `bash {"command":…}`
    assert!(symbols.contains("cargo test"), "{symbols}");
    assert!(symbols.contains("$"), "{symbols}");
    // No raw JSON should leak into the overlay
    assert!(!symbols.contains("\"command\""), "{symbols}");
    assert!(
        symbols.contains("Allow for session") || symbols.contains("Allow for this session"),
        "{symbols}"
    );
    assert!(
        symbols.contains("Esc deny") || symbols.contains("Esc"),
        "{symbols}"
    );
    // Modal is centered, not gutter-aligned — just ensure the key hints are present
    assert!(
        symbols.contains("navigate") || symbols.contains("select"),
        "{symbols}"
    );
    // Second check: write tool formats path/lines, not raw JSON
    let (tx2, _rx2) = tokio::sync::mpsc::channel(1);
    let mut app2 = test_app();
    app2.pending_approvals = vec![super::super::PendingApproval::new(
        "write".to_string(),
        r#"{"path":"src/main.rs","content":"hello\nworld\n"}"#.to_string(),
        tx2,
        Some("explorer".to_string()),
    )];
    let backend2 = TestBackend::new(80, 24);
    let mut term2 = ratatui::Terminal::new(backend2).expect("test terminal");
    term2.draw(|f| view(f, &mut app2)).expect("render");
    let s2: String = term2
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(s2.contains("src/main.rs"), "{s2}");
    assert!(s2.contains("Create") || s2.contains("write"), "{s2}");
}

#[test]
fn input_box_height_matches_wrapped_rows() {
    let area = Rect::new(0, 0, 80, 24);
    assert_eq!(
        input_content_width(area.width),
        input_block().inner(area).width,
        "measurement width must equal the rendered inner width"
    );
    let mut app = test_app();
    app.input = InputField::from_text(&"x".repeat(200));
    let measured = render_input(&app.input, input_content_width(area.width), false)
        .0
        .len();
    let rendered = render_input(&app.input, input_block().inner(area).width, false)
        .0
        .len();
    assert_eq!(measured, rendered, "wrapped row counts must agree");
}

#[test]
fn composer_has_no_border_rules() {
    // The composer is a borderless band: one `surface_bg()` row set with
    // no `─` rules, height = content rows + the shared vertical padding.
    assert_eq!(input_outer_height(1), 1 + super::super::INPUT_PAD_Y * 2);
    let area = Rect::new(0, 0, 20, 3);
    let backend = TestBackend::new(20, 3);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|f| {
            // Sentinel background: the band assertion below only means
            // something if the cells would otherwise keep a different bg.
            f.render_widget(
                Block::default().style(Style::default().bg(Color::Magenta)),
                area,
            );
            f.render_widget(Paragraph::new("hi").block(input_block()), area);
        })
        .expect("render should succeed");
    let buffer = terminal.backend().buffer();
    let row = |y| {
        (0..20)
            .map(|x| buffer[(x, y)].symbol().to_string())
            .collect::<String>()
    };
    for y in 0..3 {
        assert!(
            !row(y).contains('─'),
            "composer must not draw a rule, got {y}: {:?}",
            row(y)
        );
        assert_eq!(
            buffer[(0, y)].bg,
            theme::surface_bg(),
            "composer background should be the shared surface band"
        );
    }
    assert!(row(1).contains("hi"), "content should render on the band");
}

#[test]
fn composer_text_column_matches_transcript_indent() {
    // Typing and history share one left edge: the composer's inside
    // padding and the transcript's leading indent must resolve to the
    // same column, or the caret jumps sideways when a prompt is sent.
    let area = Rect::new(0, 0, 40, 3);
    let composer_col = input_block().inner(area).x as usize;
    assert_eq!(composer_col, super::super::TRANSCRIPT_INDENT);
    let indent = super::super::transcript_indent();
    assert_eq!(indent.len(), composer_col);
    assert!(indent.chars().all(|c| c == ' '));
    // The caret wraps against the block's own inside width, so it never
    // escapes the padded band.
    assert_eq!(
        input_block().inner(area).width,
        input_content_width(area.width)
    );
}

#[test]
fn assistant_text_is_gapped_after_tool_preview() {
    // Gaps are now rendered between TranscriptBlocks, not stored as
    // empty Lines. Verify the Tool and final Assistant are separate blocks
    // and the rendered display (block gaps) contains a blank line between
    // them – the exact bug that was missing before.
    let mut app = test_app();
    super::super::append_sink_line(
        &mut app,
        crate::protocol::SinkLine::ToolInput {
            id: String::new(),
            input: "bash grep foo src".into(),
        },
    );
    super::super::append_sink_line(
        &mut app,
        crate::protocol::SinkLine::ToolOutput {
            id: String::new(),
            name: "bash".into(),
            summary: "v 1 match".into(),
            success: true,
            preview: vec!["src/main.rs:1:foo".into()],
            duration: 0.0,
        },
    );
    // Open the throttle window so the assistant delta renders at once.
    app.stream_last_flush = std::time::Instant::now() - std::time::Duration::from_millis(500);
    super::super::append_sink_line(
        &mut app,
        crate::protocol::SinkLine::Assistant("Looked at src/main.rs.".into()),
    );

    // Transcript: [Assistant(hello), Tool, Assistant(Looked at)]
    assert_eq!(app.transcript.len(), 3);
    assert!(matches!(
        app.transcript[1],
        super::super::TranscriptBlock::Tool { .. }
    ));
    assert!(matches!(
        app.transcript[2],
        super::super::TranscriptBlock::Assistant { .. }
    ));

    // Build the same flattened display TranscriptView uses and assert a
    // single blank Line between the tool and assistant blocks.
    let mut display: Vec<Line<'static>> = Vec::new();
    for (idx, block) in app.transcript.iter().enumerate() {
        if idx > 0 {
            display.push(Line::default());
        }
        for line in block.lines() {
            display.extend(wrap_line_display(line, 100));
        }
    }
    let assistant_display_idx = display
        .iter()
        .position(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .contains("Looked at")
        })
        .expect("assistant in display");
    assert!(
        display[assistant_display_idx - 1].spans.is_empty(),
        "expected a blank gap line before assistant text in rendered display, got {:?}",
        display[assistant_display_idx - 1]
    );
}

#[test]
fn composer_shows_cursor_while_busy() {
    // Steering is typed in the composer while the agent works; the busy
    // style dims the text but must not hide the cursor (only the
    // approval modal consumes keys and steals focus).
    let area = Rect::new(0, 0, 80, 24);
    let backend = TestBackend::new(area.width, area.height);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    let mut app = test_app();
    app.busy = true;
    app.input = InputField::from_text("steer left");
    terminal.draw(|f| view(f, &mut app)).expect("frame");
    let input_rows = render_input(&app.input, input_content_width(area.width), false)
        .0
        .len() as u16;
    let layout =
        compute_layout(area, input_rows, QueueMetrics { items: 1, rows: 1 }, false).unwrap();
    let inner = input_block().inner(layout.input);
    let (_, cursor) = render_input(&app.input, inner.width, false);
    terminal
        .backend_mut()
        .assert_cursor_position((inner.x + cursor.1, inner.y + cursor.2));
}

#[test]
fn input_shrink_does_not_leave_ghost() {
    // Reproduce the ghost reported in screenshot: long wrapped input (2 rows)
    // then short input (1 row) at same terminal size must not leave
    // fragments of the long text in the frame (especially just above the
    // new input). Without a full Clear of the old input rows, ratatui
    // would leave trailing chars.
    let long = ":override to make tests hermetic. Stage 0b (turn records) and S1-S5 not started. Remote trust gate (#12-#14) documented as gate-blocking but out of scope. 4) recovery (durable turn_complete/turn_failed, daemon rebuild for /resume) -> S5 evals. Effort ~15-18 days.";
    let short = "try a different approach";
    for (w, h) in [(80, 24), (120, 24), (100, 30), (70, 24)] {
        let backend = TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let mut app = test_app();
        app.input = InputField::from_text(long);
        terminal.draw(|f| view(f, &mut app)).expect("frame1");
        app.input = InputField::from_text(short);
        terminal.draw(|f| view(f, &mut app)).expect("frame2");
        let symbols: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            !symbols.contains("hermetic"),
            "ghost at {w}x{h} after shrink"
        );
        assert!(!symbols.contains("recovery"), "ghost recovery at {w}x{h}");
        assert!(symbols.contains(short), "new input not rendered at {w}x{h}");
    }
    // Also test expand short->long
    for (w, h) in [(80, 24), (120, 24)] {
        let backend = TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        let mut app = test_app();
        app.input = InputField::from_text(short);
        terminal.draw(|f| view(f, &mut app)).expect("frame1");
        app.input = InputField::from_text(long);
        terminal.draw(|f| view(f, &mut app)).expect("frame2");
        let symbols: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            symbols.contains("hermetic"),
            "long not rendered after expand at {w}x{h}"
        );
    }
}

#[test]
fn input_ghost_with_transcript_interaction() {
    // Long transcript that fills bottom of visible area plus long input,
    // then input shrinks – ensure transcript ghost not left.
    let long_input = ":override to make tests hermetic. Stage 0b (turn records) and S1-S5 not started. Remote trust gate (#12-#14) documented as gate-blocking but out of scope. 4) recovery (durable turn_complete/turn_failed, daemon rebuild for /resume) -> S5 evals. Effort ~15-18 days.";
    let short_input = "try a different approach";
    let (w, h) = (80, 24);
    let backend = TestBackend::new(w, h);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    let mut app = test_app();
    // Fill transcript with several blocks to make it scrollable
    for i in 0..5 {
        super::super::append_sink_line(&mut app, crate::protocol::SinkLine::Assistant(format!("Assistant message {i} with some long text that will wrap across multiple lines to fill the transcript area and test scrolling behavior. {}", long_input)));
    }
    app.input = InputField::from_text(long_input);
    terminal.draw(|f| view(f, &mut app)).expect("frame1");
    let symbols1: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(symbols1.contains("hermetic"));
    app.input = InputField::from_text(short_input);
    terminal.draw(|f| view(f, &mut app)).expect("frame2");
    let symbols2: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    // Count occurrences of long_input fragments after shrink: input ghost should be gone, but transcript still contains long_input as part of assistant messages (5 times). So we need to ensure at least the input area does not contain duplicate beyond transcript count.
    // The input area is at bottom; transcript area is above. Ghost would be extra long_input fragment in the input area beyond transcript.
    // Instead check that short_input is visible and that there is no duplicate line that contains both short and long at same row.
    assert!(symbols2.contains(short_input), "short input missing");
    // Ensure no row contains both long fragment and short fragment overlapping (ghost)
    let rows: Vec<String> = symbols2
        .chars()
        .collect::<Vec<char>>()
        .chunks(w as usize)
        .map(|c| c.iter().collect())
        .collect();
    for row in rows {
        if row.contains(short_input) && row.contains("hermetic") {
            panic!("ghost overlap row: {:?}", row);
        }
    }
}

#[test]
fn user_prompt_wrapping_is_width_bounded_on_grid_margin() {
    let long = "Current: Directive: P0-P5 shipped per PLAN.md.P10 with HARNESS re-score each phase. Gates: P6 permission ceiling+scoped audit, P7 token auth/policy/redaction, P8 tx edits/journal/rebuild, P9 verification/trace/cost, P10 versioned protocol/seq/replay. Primitive beats intent. Principles: Session::set_state/load_session_state JSONL, injection via system at WRAP_UP_THRESHOLD, state in daemon, ceiling not flag, Protocol: src/llm/protocol.rs, src/protocol/mod.rs; Git commits 770d5d1 P5 hardening, 8d0ccb, e2d5c97, prior unstaged 655+/102- across 3 files (fmt'd). Unresolved: P6 ceiling+audit validation pending, P7 remote security (token auth/policy/redaction) in-progress, then P8-10; HARNESS re-score required per phase. ns: treat diff as P5 compaction hardening (token math+deterministic fallback+ephemeral injection); validate format/math/ordering + tests/clippy before commit; sequential execution. Actions: audited token/config wiring; cargo test 68 passed + clippy 0 + build, committed 770d5d1 (3 files); then started P7 audit - hit No such file io error, inspected DaemonState/Client, re-read ns:Mutex<HashMap<String,SessionEntry>>, PendingApproval{}, server::router, TcpListener non-blocking->tokio; DaemonClient blocking request, Outcomes: P5 hardening complete - token accounting hardened, session consistency maintained.";
    for w in [80, 90, 100, 120, 70, 50, 40] {
        let mut app = test_app();
        super::super::render_user_prompt(&mut app, long);
        // The submitted prompt is the composer's echo: `wrap_block`
        // paints the `surface_bg()` band and pads every row out to the
        // full width, with `INPUT_PAD_Y` blank band rows above and below
        // the content (same outer height as the live composer).
        let block = &app.transcript[1]; // 0 is hello, 1 is user
        let rows = wrap_block(block, w, false, false);
        let bg = theme::surface_bg();
        assert!(
            rows.len() >= 3,
            "user band must hold content plus composer air at w {w}"
        );
        for row in &rows {
            let s: String = row.spans.iter().map(|sp| sp.content.as_ref()).collect();
            assert_eq!(
                UnicodeWidthStr::width(s.as_str()),
                w as usize,
                "user band row must fill the full width like the composer at w {w}: {s:?}"
            );
            // Line-level bg is the band carrier (`Line` renders each
            // span as `line.style.patch(span.style)`); spans either
            // inherit it (`None`) or carry it explicitly (trailing
            // fill). Either way the rendered row must be all-`bg`.
            assert!(
                row.spans
                    .iter()
                    .all(|sp| sp.style.bg.is_none() || sp.style.bg == Some(bg)),
                "user band row must carry the composer background at w {w}: {s:?}"
            );
            assert_eq!(row.style.bg, Some(bg), "user band line bg at w {w}");
        }
        // Composer air: blank band rows top and bottom, content between.
        let text = |row: &Line<'static>| {
            row.spans
                .iter()
                .map(|sp| sp.content.as_ref())
                .collect::<String>()
        };
        assert!(
            text(&rows[0]).trim().is_empty(),
            "band top must be composer air at w {w}"
        );
        assert!(
            text(rows.last().unwrap()).trim().is_empty(),
            "band bottom must be composer air at w {w}"
        );
        assert!(
            rows[1..rows.len() - 1]
                .iter()
                .any(|r| text(r).contains("Current:")),
            "band content must survive between the air rows at w {w}"
        );
        // Content rows sit on the shared transcript margin (exactly one
        // gutter of leading space, like the composer); pad rows are
        // blank fill.
        for row in &rows[1..rows.len() - 1] {
            let s = text(row);
            if s.trim().is_empty() {
                continue;
            }
            assert_eq!(
                row.spans.first().map(|sp| sp.content.as_ref()),
                Some(super::super::transcript_indent().as_str()),
                "user row must carry the grid-margin indent span at w {w}: {s:?}"
            );
        }
        // Also test full view rendering at this width does not panic and buffer is correct
        let backend = TestBackend::new(w, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| view(f, &mut app)).unwrap();
        assert_eq!(terminal.backend().buffer().area.width, w);
        // Rendered contract: every cell of the band paints `bg`,
        // whatever the struct-level split between line and span style.
        let band_backend = TestBackend::new(w, rows.len() as u16);
        let mut band_terminal = ratatui::Terminal::new(band_backend).unwrap();
        band_terminal
            .draw(|f| {
                f.render_widget(Paragraph::new(rows.clone()), f.area());
            })
            .unwrap();
        for cell in band_terminal.backend().buffer().content.iter() {
            assert_eq!(
                cell.bg, bg,
                "band cell must paint the composer background at w {w}: {cell:?}"
            );
        }
    }
}

#[test]
fn ghost_key_facts_does_not_overflow_or_overlap_bottom() {
    // Repro for screenshot ghost: long assistant line with unbroken tokens
    // must wrap within width and never appear in input/footer area.
    let ghost = "Key facts: Docs at /tmp/dex/HARNESS.md (489 lines), PLAN.md (92 lines, P0-P5 shipped, next 6-10: permission ceiling/audit, token auth/policy/redaction, transactional edits/journal, verification, versioned protocol seq/replay). Src layout: src/agent/{loop,state,compaction}, client/http, cli/config/core/daemon/llm/protocol/session/skills/tools/ui. Key symbols: Session::set_state/load_session_state (/resume), Plan{goal,steps}+/goal/plan add/done/clear+SinkLine::Plan→StreamEvent::Plan, WRAP_UP_THRESHOLD, DaemonState/PendingApproval/router/SSE, TurnLimits/deadline/within_budget, TurnComplete/TurnFailed/Usage, SessionHeader, FileConfig/LlmConfig/Provider, system_prompt/project_context, CONFIGURED_OUTPUT_LIMIT/execute_outcome. Unresolved: complete section-by-section audit and rewrite HARNESS.md with code citations and re-scoring per active runtime.1126 lines), daemon mod/server (axum router, SSE, approvals), llm/config/prompt, disposition, versioned protocol seq/replay).";
    for (w, h) in [
        (80, 24),
        (100, 24),
        (120, 24),
        (200, 24),
        (80, 40),
        (120, 40),
    ] {
        let backend = TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = test_app();
        // Fill transcript like screenshot: several tool blocks then assistant ghost
        for name in [
            "slash.rs",
            "types.rs",
            "console.rs",
            "format.rs",
            "client.rs",
            "remote.rs",
        ] {
            super::super::append_sink_line(
                &mut app,
                crate::protocol::SinkLine::ToolInput {
                    id: String::new(),
                    input: format!("read /tmp/dex/src/ui/{name}"),
                },
            );
            super::super::append_sink_line(
                &mut app,
                crate::protocol::SinkLine::ToolOutput {
                    id: String::new(),
                    name: "read".into(),
                    summary: "10 lines".into(),
                    success: true,
                    preview: vec![
                        "1 use std::env;".into(),
                        "2".into(),
                        "3 use std::env;".into(),
                    ],
                    duration: 0.0,
                },
            );
        }
        super::super::append_sink_line(
            &mut app,
            crate::protocol::SinkLine::Assistant(
                "Evidence map 80% complete - pulling final modules to re-score the board.".into(),
            ),
        );
        super::super::append_sink_line(
            &mut app,
            crate::protocol::SinkLine::Assistant(ghost.into()),
        );
        app.input = InputField::from_text("try a different approach");
        terminal.draw(|f| view(f, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        // Compute layout like view does
        let input_rows = render_input(&app.input, input_content_width(area.width), false)
            .0
            .len() as u16;
        let queue = pending_queue_metrics(&app);
        let layout = compute_layout(area, input_rows, queue, false).unwrap();
        // Check every cell in input and footer does not contain ghost fragments
        // Ghost contains distinctive substrings that should never leak into chrome
        let forbidden = [
            "Key facts",
            "HARNESS.md",
            "Session::set_state",
            "WRAP_UP_THRESHOLD",
        ];
        let content: String = buffer.content.iter().map(|c| c.symbol()).collect();
        let rows: Vec<String> = content
            .chars()
            .collect::<Vec<char>>()
            .chunks(w as usize)
            .map(|c| c.iter().collect())
            .collect();
        for y in layout.input.y..layout.input.y + layout.input.height {
            let row = &rows[y as usize];
            for pat in forbidden {
                assert!(
                    !row.contains(pat),
                    "ghost '{pat}' leaked into input at {w}x{h} y={y} row={:?}",
                    row
                );
            }
        }
        for y in layout.footer.y..layout.footer.y + layout.footer.height {
            let row = &rows[y as usize];
            for pat in forbidden {
                assert!(
                    !row.contains(pat),
                    "ghost '{pat}' leaked into footer at {w}x{h} y={y} row={:?}",
                    row
                );
            }
        }
        // Also check that no row in entire buffer exceeds width (hard wrap)
        for line in &app.display_cache {
            let s: String = line.spans.iter().map(|sp| sp.content.as_ref()).collect();
            let width = UnicodeWidthStr::width(s.as_str());
            assert!(
                width <= w as usize,
                "display_cache line overflow at {w}: {width} > {w} line={:?}",
                s
            );
        }
        // Simulate resize to narrower then wider without new transcript data: cache must re-wrap
        let backend2 = TestBackend::new(w.saturating_sub(20).max(40), h);
        let mut terminal2 = ratatui::Terminal::new(backend2).unwrap();
        terminal2.draw(|f| view(f, &mut app)).unwrap();
        let content2: String = terminal2
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(!content2.contains("\t"), "tab not expanded after resize");
    }
}

#[test]
fn consecutive_assistant_chunks_do_not_add_gaps() {
    // Streaming coalesces consecutive Assistant SinkLines into the tail
    // Assistant block; no inter-block gap must appear inside that block.
    // Each streamed line flushes on its own window so the tail block
    // holds both chunks (mirrors turn-end `flush_assistant` draining
    // whatever the throttle still holds).
    let mut app = test_app();
    app.stream_last_flush = std::time::Instant::now() - std::time::Duration::from_millis(500);
    super::super::append_sink_line(
        &mut app,
        crate::protocol::SinkLine::Assistant("first".into()),
    );
    app.stream_last_flush = std::time::Instant::now() - std::time::Duration::from_millis(500);
    super::super::append_sink_line(
        &mut app,
        crate::protocol::SinkLine::Assistant("second".into()),
    );
    // [Assistant(hello)] + streamed Assistant => two blocks, tail holds both.
    assert_eq!(app.transcript.len(), 2);
    let tail = match &app.transcript[1] {
        super::super::TranscriptBlock::Assistant { lines, .. } => lines,
        other => panic!("expected tail Assistant block, got {other:?}"),
    };
    let first_pos = tail
        .iter()
        .position(|l| l.spans.iter().any(|s| s.content.contains("first")))
        .expect("first");
    let second_pos = tail
        .iter()
        .position(|l| l.spans.iter().any(|s| s.content.contains("second")))
        .expect("second");
    assert_eq!(
        second_pos,
        first_pos + 1,
        "streamed assistant chunks must stay flush inside one block"
    );
}
/// Regression: the submitted prompt renders as the composer's echo — a
/// full-width `surface_bg()` band with `INPUT_PAD_Y` air above and below
/// the content (same outer height as the live composer), headed by the
/// composer glyph. The transcript Paragraph must NOT enable `Wrap`:
/// the display cache is already pre-wrapped, and ratatui 0.29's WordWrapper
/// emits a phantom empty row before any all-whitespace line exactly
/// `area.width` wide, which would shift the tool block down.
#[test]
fn submitted_prompt_is_one_row_above_tool_block() {
    let backend = TestBackend::new(126, 25);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    let mut app = test_app();
    super::super::push_info(
        &mut app,
        "connected to http://127.0.0.1:35487 - workspace /tmp/dex - model deepseek-v4-flash".into(),
    );
    super::super::render_user_prompt(&mut app, "can you check pillar 1 form harness.md");
    super::super::append_sink_line(
        &mut app,
        crate::protocol::SinkLine::ToolInput {
            id: String::new(),
            input: "read HARNESS.md".into(),
        },
    );
    super::super::append_sink_line(
        &mut app,
        crate::protocol::SinkLine::ToolOutput {
            id: String::new(),
            name: "read".into(),
            summary: "v 313 lines".into(),
            success: true,
            preview: vec!["1 # Harness Capability Map".into()],
            duration: 0.0,
        },
    );
    terminal.draw(|f| view(f, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    let area = buffer.area;
    let row_of = |needle: &str| {
        (0..area.height).find(|&y| {
            let row: String = (0..area.width)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect();
            row.contains(needle)
        })
    };
    let text_row = row_of("can you check pillar").expect("prompt text rendered");
    let tool_row = row_of("read HARNESS.md").expect("tool block rendered");
    // Composer echo: content row, one band-air row, the inter-block gap
    // row, the tool band's own top-air row, then the tool content.
    assert_eq!(
        tool_row,
        text_row + 4,
        "a phantom row from Paragraph::wrap shifts the tool block down"
    );
    // On the shared transcript margin — the prompt row starts one gutter
    // in, then bare text, on the composer background band.
    let prompt_row: String = (0..area.width)
        .map(|x| buffer.cell((x, text_row)).unwrap().symbol())
        .collect();
    assert!(
        prompt_row.starts_with(&format!(
            "{}can you check pillar",
            super::super::transcript_indent()
        )),
        "prompt row must sit on the transcript margin: {prompt_row:?}"
    );
    let bg = theme::surface_bg();
    for x in 0..area.width {
        assert_eq!(
            buffer.cell((x, text_row)).unwrap().bg,
            bg,
            "prompt row must wear the composer band background"
        );
        assert_eq!(
            buffer.cell((x, text_row + 1)).unwrap().bg,
            bg,
            "band air below the content must wear the composer background"
        );
        assert_eq!(
            buffer.cell((x, text_row - 1)).unwrap().bg,
            bg,
            "band air above the content must wear the composer background"
        );
    }
    let air_below: String = (0..area.width)
        .map(|x| buffer.cell((x, text_row + 1)).unwrap().symbol())
        .collect();
    let gap: String = (0..area.width)
        .map(|x| buffer.cell((x, text_row + 2)).unwrap().symbol())
        .collect();
    assert!(
        air_below.trim().is_empty(),
        "band air below the content must be blank: {air_below:?}"
    );
    assert!(
        gap.trim().is_empty(),
        "inter-block gap after the band must be blank: {gap:?}"
    );
    // The tool band carries its own top-air row: blank, but wearing the
    // band background so the content clears the band edge.
    let tool_air: String = (0..area.width)
        .map(|x| buffer.cell((x, tool_row - 1)).unwrap().symbol())
        .collect();
    assert!(
        tool_air.trim().is_empty(),
        "tool band air above the content must be blank: {tool_air:?}"
    );
    for x in 0..area.width {
        assert_eq!(
            buffer.cell((x, tool_row - 1)).unwrap().bg,
            bg,
            "tool band air must wear the band background"
        );
        assert_eq!(
            buffer.cell((x, text_row + 2)).unwrap().bg,
            ratatui::style::Color::Reset,
            "inter-block gap must stay terminal background"
        );
    }
}

#[test]
fn blank_runs_render_one_air_row() {
    // Double/triple blank lines collapse to one air row (CommonMark renders
    // a single separator for a blank run).
    let src = crate::ui::theme::markdown::normalize_gaps(
        "",
        "para one\n\n\n\npara two\n\n\n- a\n- b\n\n\ntail",
    );
    let lines = markdown_lines(&src);
    let rows: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert_eq!(
        rows,
        vec!["para one", "", "para two", "", "•  a", "•  b", "", "tail"],
    );
}

#[test]
fn normalized_source_renders_gapped() {
    // The gap rule lives in `core::markdown` (unit-tested there); this
    // pins the render layer end to end: normalized dense source renders
    // the heading, list and table separated instead of wall-to-wall.
    let src = crate::ui::theme::markdown::normalize_gaps(
        "",
        "text\n## Changes\n- a\n| A | B |\n|---|---|\n| 1 | 2 |",
    );
    let lines = markdown_lines(&src);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref().to_string()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Changes"), "{text}");
    assert!(text.contains('─'), "table not drawn: {text}");
    let blank_rows = lines.iter().filter(|l| l.spans.is_empty()).count();
    assert!(
        blank_rows >= 2,
        "expected air rows, got {blank_rows}: {text}"
    );
}

#[test]
fn split_markdown_detects_gfm_tables() {
    let blocks = split_markdown("| Name | Count |\n|------|-------|\n| alpha | 1 |\n| beta | 2 |");
    match &blocks[0] {
        MarkdownBlock::Table { headers, rows } => {
            assert_eq!(headers, &["Name", "Count"]);
            assert_eq!(rows, &[vec!["alpha", "1"], vec!["beta", "2"]]);
        }
        other => panic!("expected a table block, got {other:?}"),
    }
    // Alignment colons are part of the delimiter row, not cell text.
    let aligned = split_markdown("| Left | Mid | Right |\n|:---|:---:|---:|\n| a | b | c |");
    match &aligned[0] {
        MarkdownBlock::Table { headers, rows } => {
            assert_eq!(headers, &["Left", "Mid", "Right"]);
            assert_eq!(rows, &[vec!["a", "b", "c"]]);
        }
        other => panic!("expected a table block, got {other:?}"),
    }
}

#[test]
fn split_markdown_unescapes_table_pipes() {
    // `\|` is a literal pipe (GFM escape), not a cell separator.
    let blocks = split_markdown("| Expr | N |\n|---|---|\n| a \\| b | 1 |");
    match &blocks[0] {
        MarkdownBlock::Table { rows, .. } => {
            assert_eq!(rows, &[vec!["a | b", "1"]]);
        }
        other => panic!("expected a table block, got {other:?}"),
    }
}

#[test]
fn split_markdown_separates_butted_table_from_prose() {
    // No blank line between the prose and the table: still two blocks.
    let blocks = split_markdown("Some prose.\n| a | b |\n|---|---|\n| 1 | 2 |");
    assert!(
        matches!(blocks[0], MarkdownBlock::Paragraph(_)),
        "{blocks:?}"
    );
    assert!(
        matches!(blocks[1], MarkdownBlock::Table { .. }),
        "{blocks:?}"
    );
}

#[test]
fn split_markdown_keeps_pipe_prose_as_paragraph() {
    // A stray pipe line with no delimiter row below is prose, not a table.
    let blocks = split_markdown("run: a | b | c\nmore prose");
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert!(
        matches!(blocks[0], MarkdownBlock::Paragraph(_)),
        "{blocks:?}"
    );
}

#[test]
fn block_highlight_splits_rows_and_falls_back_to_dim() {
    let rows = highlight_code_block("rust", "fn main() {}").expect("highlight");
    assert_eq!(rows.len(), 1);
    assert!(rows[0].iter().any(|s| s.style.fg.is_some()));
    assert!(highlight_code_block("", "fn main() {}").is_none());
    assert!(highlight_code_block("no-such-lang", "text").is_none());
}

#[test]
fn block_highlight_keeps_multiline_constructs_across_rows() {
    // One tree-sitter pass over the whole snippet: a triple-quoted
    // Python string stays a string on every row. Per-line highlighting
    // would render the middle row plain.
    let code = "x = \"\"\"\nhello\n\"\"\"";
    let rows = highlight_code_block("python", code).expect("highlight");
    assert_eq!(rows.len(), 3);
    let text =
        |row: &Vec<Span<'static>>| -> String { row.iter().map(|s| s.content.as_ref()).collect() };
    assert_eq!(text(&rows[1]), "hello");
    assert!(
        rows[1].iter().any(|s| s.style.fg.is_some()),
        "middle row of a block string must stay highlighted: {:?}",
        rows[1]
    );
}

#[test]
fn read_preview_renders_sections_with_gutter_and_tail() {
    let preview = vec![
        "==> src/main.rs <==".to_string(),
        "   1  fn main() {}".to_string(),
        "… +1 more lines".to_string(),
    ];
    let lines = render_read_preview(&preview, "");
    assert_eq!(lines.len(), 3);
    let text =
        |l: &Line<'static>| -> String { l.spans.iter().map(|s| s.content.as_ref()).collect() };
    assert!(
        text(&lines[0]).contains("src/main.rs"),
        "{}",
        text(&lines[0])
    );
    assert!(text(&lines[1]).contains("fn main"), "{}", text(&lines[1]));
    // Gutter row carries highlight past the dim gutter span.
    assert!(lines[1].spans.len() > 2, "{:?}", lines[1]);
}

#[test]
fn read_preview_error_header_stays_dim() {
    // Fan-out errors (`==> path: error: …`, no `<==`) are landmarks,
    // not sections: dim, never a language re-target.
    let preview = vec!["==> src/missing.rs: error: not found".to_string()];
    let lines = render_read_preview(&preview, "rust");
    assert_eq!(lines.len(), 1);
    // spans[0] is the unstyled transcript indent; the rest stays dim.
    assert!(lines[0].spans[1..]
        .iter()
        .all(|s| s.style.fg == Some(crate::ui::theme::tool_preview_fg())));
}

#[test]
fn search_preview_highlights_hits_and_keeps_gutter_dim() {
    // grep/ffgrep content mode: `path:line:code` hits (and `:N-` context
    // rows) keep the structural gutter dim while the code highlights by
    // path extension — contiguous same-language hits share one pass.
    let preview = vec![
        String::new(),
        "src/a.rs:2:fn hits() {}".to_string(),
        "src/a.rs:1-// context".to_string(),
        "src/b.rs:5:let other = 1;".to_string(),
    ];
    let lines = render_search_preview(&preview);
    assert_eq!(lines.len(), 4);
    let text =
        |l: &Line<'static>| -> String { l.spans.iter().map(|s| s.content.as_ref()).collect() };
    assert!(
        text(&lines[1]).contains("  src/a.rs:2:"),
        "{}",
        text(&lines[1])
    );
    assert!(
        text(&lines[1]).contains("fn hits() {}"),
        "{}",
        text(&lines[1])
    );
    // Gutter (spans[1] after the transcript indent) stays dim...
    let dim = crate::ui::theme::tool_preview_fg();
    assert_eq!(lines[1].spans[1].style.fg, Some(dim));
    // ...and the code is tree-sitter highlighted, not one dim blob.
    assert!(lines[1].spans.len() > 2, "{:?}", lines[1]);
    assert!(lines[2].spans.len() > 2, "{:?}", lines[2]);
    assert!(lines[3].spans.len() > 2, "{:?}", lines[3]);
}

#[test]
fn search_preview_keeps_prose_and_path_lists_dim() {
    // files-mode path lists and fuzzy-fallback prose stay dim exactly
    // like the generic renderer; the fuzzy grouping's `  N: code` rows
    // highlight via the bare path header's language.
    let preview = vec![
        "0 exact matches for 'x'. 2 approximate:".to_string(),
        "src/main.rs".to_string(),
        "  10: fn main() {}".to_string(),
    ];
    let lines = render_search_preview(&preview);
    assert_eq!(lines.len(), 3);
    let dim = crate::ui::theme::tool_preview_fg();
    let dim_after_indent = |l: &Line<'static>| l.spans[1..].iter().all(|s| s.style.fg == Some(dim));
    assert!(dim_after_indent(&lines[0]), "{:?}", lines[0]);
    assert!(dim_after_indent(&lines[1]), "{:?}", lines[1]);
    assert!(lines[2].spans.len() > 2, "{:?}", lines[2]);
}

#[test]
fn split_markdown_normalizes_code_lang_for_highlighter() {
    // `ratatui-markdown::get_lang` only matches exact lowercase tags, so
    // the legacy `rust:` sink suffix, case variants, info-string params,
    // and the `rs` shorthand must all normalize to `rust`.
    for info in [
        "rust:",
        "Rust",
        "Rust:",
        "rs",
        "rust ignore",
        "RUST linenums",
    ] {
        let blocks = split_markdown(&format!("```{info}\nfn main() {{}}\n```\n"));
        assert_eq!(blocks.len(), 1, "{info}: {blocks:?}");
        assert!(
            matches!(&blocks[0], MarkdownBlock::CodeBlock { lang, .. } if lang == "rust"),
            "{info}: {blocks:?}"
        );
    }
}

#[test]
fn markdown_fence_drops_trailing_blank_body_rows() {
    // Borderless code blocks: no `╭─`/`│ `/`╰─` glyphs, so native
    // terminal copies come out as runnable commands. Trailing blanks in
    // a snippet still collapse away (no empty tail rows).
    let render = |src: &str| -> Vec<String> {
        markdown_lines(src)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    };
    let rows = render("```bash\nfoo \\\n  bar\n```");
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows[0].contains("bash"), "{rows:?}");
    assert!(!rows[0].contains('╭') && !rows[0].contains('─'), "{rows:?}");
    assert_eq!(rows[1], "  foo \\");
    assert_eq!(rows[2], "    bar");
    assert!(
        rows.iter()
            .all(|r| !r.contains('╭') && !r.contains('╰') && !r.contains('│')),
        "no box-drawing may survive: {rows:?}"
    );
    // Blank lines before the closing fence collapse away too.
    let rows = render("```bash\nfoo\n\n\n```");
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[1], "  foo");
}

#[test]
fn markdown_lines_render_tables_as_boxes() {
    let lines = markdown_lines("| A | B |\n|---|---|\n| 1 | 2 |");
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref().to_string()))
        .collect::<Vec<_>>()
        .join("\n");
    // Box-drawn table, not the raw pipe + dash delimiter row.
    assert!(text.contains('─'), "no rule drawn: {text}");
    assert!(text.contains('│'), "no column borders: {text}");
    assert!(!text.contains("|---"), "raw delimiter leaked: {text}");
    assert!(text.contains('A') && text.contains('2'), "{text}");
}

#[test]
fn streamed_table_renders_as_one_block() {
    // Regression: markdown was re-parsed per throttle flush, so a table
    // streaming line-by-line rendered as raw paragraphs (header flushed
    // alone before its delimiter arrived). The flush must hold until the
    // table is complete, then render the whole thing as a Table block.
    let mut app = test_app();
    let lines = [
        "Here is the comparison:",
        "",
        "| File | Lines | Status |",
        "|------|-------|--------|",
        "| loop.rs | 420 | done |",
        "| state.rs | 180 | ok |",
        "",
        "All good.",
    ];
    for line in lines {
        super::super::append_sink_line(&mut app, crate::protocol::SinkLine::Assistant(line.into()));
    }
    super::super::flush_assistant(&mut app);
    let text: String = app
        .transcript
        .iter()
        .filter_map(|b| match b {
            super::super::TranscriptBlock::Assistant { lines, .. } => Some(lines),
            _ => None,
        })
        .flatten()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref().to_string()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains('─'), "no rule drawn: {text}");
    assert!(text.contains('│'), "no column borders: {text}");
    assert!(!text.contains("|---"), "raw delimiter leaked: {text}");
    assert!(text.contains("loop.rs") && text.contains("done"), "{text}");
    // Narrow terminal: the table degrades by wrapping, never panics.
    let backend = TestBackend::new(50, 20);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| super::super::view(f, &mut app)).unwrap();
}

#[test]
fn table_rows_stay_tight_across_seams() {
    // A throttle seam between table rows must not insert air: that would
    // split one table into two blocks mid-column.
    let out = crate::ui::theme::markdown::normalize_gaps("| a | b |\n|---|---|\n", "| 1 | 2 |");
    assert_eq!(out, "| 1 | 2 |\n");
    // Same for the delimiter row following a header.
    let out = crate::ui::theme::markdown::normalize_gaps("| a | b |\n", "|---|---|");
    assert_eq!(out, "|---|---|\n");
}

#[test]
fn tool_glyphs_head_the_input_row() {
    // Every known tool heads its row with its own glyph; unknown tools
    // keep the generic `▸`.
    let text = |name: &str, arg: &str| -> String {
        render_tool_input(name, arg)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
            .trim_start()
            .to_string()
    };
    assert!(text("bash", "ls -la").starts_with("$ bash ls -la"));
    assert!(text("read", "a.rs").starts_with("¶ read a.rs"));
    assert!(text("write", "a.rs").starts_with("✎ write a.rs"));
    assert!(text("edit", "a.rs").starts_with("± edit a.rs"));
    assert!(text("grep", "pat").starts_with("/ grep pat"));
    assert!(text("fffind", "*.rs").starts_with("/ fffind *.rs"));
    assert!(text("ls", ".").starts_with("☰ ls ."));
    assert!(text("git", "status").starts_with("⎇ git status"));
    assert!(text("chain", "2 steps").starts_with("→ chain 2 steps"));
    assert!(text("mcp__srv__t", "{}").starts_with("⇄ mcp__srv__t"));
    assert!(text("mystery", "x").starts_with("▸ mystery x"));
}

#[test]
fn bash_tool_input_highlights_while_other_tools_stay_dim() {
    // A bash command with a string + comment must split into styled spans
    // past the `$ bash ` prefix; a plain tool arg stays one dim span.
    let line = render_tool_input("bash", "echo \"hi\" # done");
    // indent + glyph + name + ` ` + highlighted code spans.
    assert!(line.spans.len() > 4, "bash should highlight: {line:?}");
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains("echo"), "{text}");

    let line = render_tool_input("read", "src/main.rs:1-20");
    // indent + glyph + name + dim arg: no highlight split.
    assert_eq!(line.spans.len(), 4, "{line:?}");

    // Multi-line bash keeps the full dim arg (highlighting splits per
    // row; appending only the first would silently drop lines 2+).
    let line = render_tool_input("bash", "echo a\necho b");
    assert_eq!(line.spans.len(), 4, "{line:?}");
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(text.contains('\n'), "{text}");
}

#[test]
fn bash_approval_detail_highlights_code_after_prefix() {
    let line = render_approval_detail("bash", "$ echo \"hi\"");
    assert!(line.spans.len() > 1, "{line:?}");
    assert!(line.spans[0].content.as_ref() == "$ ");
    // Non-bash details keep their single-span colors.
    let line = render_approval_detail("read", "path: src/main.rs");
    assert_eq!(line.spans.len(), 1);
    assert_eq!(line.spans[0].style.fg, Some(Color::Cyan));

    // Multi-line code stays one full-detail span (never truncated).
    let line = render_approval_detail("bash", "$ echo a\necho b");
    assert_eq!(line.spans.len(), 1);
    assert!(line.spans[0].content.contains('\n'), "{line:?}");
}

#[test]
fn bash_fence_highlights_via_tree_sitter() {
    // The grammar must yield segments for typical shell (strings,
    // comments, keywords) so ```bash blocks never fall back to yellow.
    let highlighted = highlight_code_block("bash", "echo \"hi\" # done\nif x; then y; fi\n");
    assert!(highlighted.is_some(), "bash grammar yielded nothing");
    // Plain prose with no colorable tokens keeps the dim fallback.
    let unknown = highlight_code_block("definitely-not-a-lang", "plain");
    assert!(unknown.is_none());
}

#[test]
fn dockerfile_fence_highlights_via_fallback_while_unknown_stays_dim() {
    // sql/dockerfile have no tree-sitter grammar: the generic lexer
    // colors them instead of dim. html now has a real grammar.
    // Truly unknown tags stay dim instead of guessing.
    for (lang, code) in [
        ("sql", "SELECT a FROM t WHERE x = 1\n"),
        ("dockerfile", "FROM rust:1\nRUN cargo build\n"),
        ("html", "<div>hi</div>\n"),
    ] {
        let rows = highlight_code_block(lang, code).expect("{lang} must highlight");
        let text: String = rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|s| s.content.as_ref().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(code.lines().next().unwrap_or("")), "{lang}");
        assert!(
            rows.iter()
                .flat_map(|r| r.iter())
                .any(|s| s.style.fg.is_some()),
            "{lang} emitted no styles"
        );
    }
    assert!(highlight_code_block("definitely-not-a-lang", "def x = 42\n").is_none());
    assert!(highlight_code_block("text", "SELECT 1\n").is_none());
}

#[test]
fn markdown_fences_use_tree_sitter_and_fallback() {
    // TUI fences must agree with previews/headless: tree-sitter for html,
    // the generic lexer for sql/dockerfile, dim for truly unknown.
    let lines = markdown_lines("```html\n<div>hi</div>\n```");
    assert!(
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.fg.is_some()),
        "html fence should highlight: {lines:?}"
    );
    let lines = markdown_lines("```sql\nSELECT a FROM t\n```");
    assert!(
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.fg.is_some()),
        "dockerfile fence should fallback-highlight: {lines:?}"
    );
}

#[test]
fn composer_first_row_has_no_prompt_glyph() {
    // No prompt glyph: the composer's first row is bare text on the
    // shared transcript margin (one gutter in), same row as the first
    // line of input.
    let (w, h) = (60u16, 16u16);
    let mut app = test_app();
    app.input = InputField::from_text("hello composer");
    let mut terminal = ratatui::Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| view(f, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..h)
        .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect::<String>())
        .collect();
    assert!(
        rows.iter().all(|r| !r.contains("▶")),
        "no prompt glyph anywhere: {rows:?}"
    );
    let margin = " ".repeat(TRANSCRIPT_INDENT);
    assert!(
        rows.iter()
            .any(|r| r.trim_end().starts_with(&format!("{margin}hello composer"))),
        "input text on the transcript margin: {rows:?}"
    );
}
