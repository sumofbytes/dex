use super::super::append_sink_line;
use super::super::close_thinking;
use super::super::flush_assistant;
use super::super::last_col;
use super::super::line_selection_text;
use super::super::mouse_display_cell;
use super::super::push_info;
use super::super::render_user_prompt;
use super::super::scroll_transcript;
use super::super::selection_text;
use super::super::settle_activity;
use super::super::word_bounds;
use super::super::AgentChip;
use super::super::PendingApproval;
use super::super::Selection;
use super::pollers::refresh_git_async;
use super::pollers::spawn_approval_poster;
use super::state::RemoteApp;
use crate::protocol::ApprovalDecision;
use crate::protocol::SinkLine;
use crate::protocol::StreamEvent;
use crate::session::Session;
use crossterm::event;
use crossterm::event::MouseButton;
use crossterm::event::MouseEventKind;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc;

/// Window between two left presses on the same transcript cell that counts
/// as a repeated click (double = word select, triple = line select).
const MULTI_CLICK: Duration = Duration::from_millis(500);

/// Mouse routing: wheel scrolls the transcript; left press/drag/release
/// selects transcript text with a visible highlight and copies it to the
/// system clipboard (OSC 52) on release. A click clears; Shift+drag still
/// bypasses mouse reporting for native selection. Repeated presses on the
/// same cell within `MULTI_CLICK` select more: double-click the word under
/// it, triple-click the whole line (and dragging a line select extends it
/// by whole rows). Both keep the highlight until the next click.
pub(crate) fn handle_mouse(remote: &mut RemoteApp, m: event::MouseEvent) {
    match m.kind {
        MouseEventKind::ScrollUp => scroll_transcript(&mut remote.app, -3),
        MouseEventKind::ScrollDown => scroll_transcript(&mut remote.app, 3),
        MouseEventKind::Down(MouseButton::Left) => {
            // Pressing outside the transcript (composer, activity line)
            // clears any live selection instead of starting a new one.
            let cell = mouse_display_cell(
                remote.app.scroll,
                remote.app.transcript_area,
                remote.app.display_cache.len(),
                &m,
            );
            let clicks = match (remote.last_click, cell) {
                (Some((at, last, n)), Some(c)) if at.elapsed() <= MULTI_CLICK && last == c => {
                    (n + 1).min(3)
                }
                _ => 1,
            };
            remote.app.selection = match cell {
                Some((row, col)) if clicks == 2 => word_bounds(&remote.app.display_cache[row], col)
                    .map(|(c0, c1)| Selection {
                        anchor: (row, c0),
                        end: (row, c1),
                        sticky: true,
                        whole_line: false,
                    }),
                Some((row, _)) if clicks == 3 => Some(Selection {
                    anchor: (row, 0),
                    end: (row, last_col(&remote.app.display_cache[row])),
                    sticky: true,
                    whole_line: true,
                }),
                Some(cell) => Some(Selection {
                    anchor: cell,
                    end: cell,
                    sticky: false,
                    whole_line: false,
                }),
                None => None,
            };
            remote.last_click = cell.map(|c| (Instant::now(), c, clicks));
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(sel) = remote.app.selection.as_mut() {
                if let Some(cell) = mouse_display_cell(
                    remote.app.scroll,
                    remote.app.transcript_area,
                    remote.app.display_cache.len(),
                    &m,
                ) {
                    if sel.whole_line {
                        // Line selects extend by whole rows; the anchor row
                        // (the press point) stays put, xterm-style.
                        sel.end = (cell.0, last_col(&remote.app.display_cache[cell.0]));
                    } else {
                        sel.end = cell;
                    }
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            // Sticky selections (double-click word picks, triple-click line
            // picks) stay highlighted after release until the next press;
            // drag selections clear.
            let sticky = remote.app.selection.is_some_and(|s| s.sticky);
            // A released drag breaks the multi-click chain: the next press
            // starts a fresh count instead of compounding into word/line
            // picks from where the drag happened to begin.
            let dragged = remote
                .app
                .selection
                .is_some_and(|s| !s.sticky && !s.is_empty());
            if let Some(sel) = remote.app.selection {
                if sel.sticky || !sel.is_empty() {
                    let text = if sel.whole_line {
                        let r0 = sel.anchor.0.min(sel.end.0);
                        let r1 = sel.anchor.0.max(sel.end.0);
                        line_selection_text(&remote.app.display_cache, r0, r1)
                    } else {
                        let ((r0, c0), (r1, c1)) = sel.norm();
                        selection_text(&remote.app.display_cache, (r0, c0), (r1, c1))
                    };
                    remote.app.copy_selection(&text);
                }
            }
            if !sticky {
                remote.app.selection = None;
            }
            if dragged {
                remote.last_click = None;
            }
        }
        _ => {}
    }
}

/// Output tokens/s of one LLM call: completion tokens over the
/// daemon-measured wall-clock duration. Needs both a non-zero output and a
/// positive duration; `None` otherwise (old daemon, untimed summarizer
/// call, or an empty completion).
pub(crate) fn output_rate(output: u64, gen_ms: Option<u64>) -> Option<f64> {
    match gen_ms {
        Some(ms) if ms > 0 && output > 0 =>
        {
            #[allow(clippy::cast_precision_loss)]
            Some(output as f64 * 1000.0 / ms as f64)
        }
        _ => None,
    }
}

pub(crate) fn handle_stream_event(remote: &mut RemoteApp, event: StreamEvent) {
    match event {
        StreamEvent::AssistantText(text) => {
            append_sink_line(&mut remote.app, SinkLine::Assistant(text));
        }
        StreamEvent::Thinking(text) => {
            append_sink_line(&mut remote.app, SinkLine::Thinking(text));
        }
        StreamEvent::ToolCall { name, args, id } => {
            let preview = args.as_str().unwrap_or_default().to_string();
            append_sink_line(
                &mut remote.app,
                SinkLine::ToolInput {
                    id,
                    input: format!("{name} {preview}"),
                },
            );
        }
        StreamEvent::ToolResult {
            name,
            summary,
            success,
            preview,
            duration,
            id,
        } => {
            append_sink_line(
                &mut remote.app,
                SinkLine::ToolOutput {
                    id,
                    name,
                    summary,
                    success,
                    preview,
                    duration,
                },
            );
        }
        StreamEvent::SteeringAccepted { content } => {
            let app = &mut remote.app;
            // Drop the accepted badge (exact match, then trim-insensitive so a
            // whitespace echo difference doesn't strand it). Never pop an
            // unrelated item: a recall may have already removed this one, and
            // the accepted copy still renders below.
            let trimmed = content.trim();
            if let Some(pos) = app
                .pending_steering
                .iter()
                .position(|c| c == &content)
                .or_else(|| {
                    app.pending_steering
                        .iter()
                        .position(|c| c.trim() == trimmed)
                })
            {
                app.pending_steering.remove(pos);
            }
            render_user_prompt(app, &content);
        }
        StreamEvent::FollowupAccepted { content } => {
            let app = &mut remote.app;
            let trimmed = content.trim();
            if let Some(pos) = app
                .pending_followups
                .iter()
                .position(|c| c == &content)
                .or_else(|| {
                    app.pending_followups
                        .iter()
                        .position(|c| c.trim() == trimmed)
                })
            {
                app.pending_followups.remove(pos);
            }
            render_user_prompt(app, &content);
        }
        StreamEvent::ApprovalRequired {
            request_id,
            name,
            input,
            agent,
        } => {
            // Queue (V1b): child agents park labeled prompts that outlive
            // the parent turn, so several can be answerable at once. The
            // overlay resolves the front; each entry POSTs its own decision.
            let (response, decision_rx) = mpsc::channel::<ApprovalDecision>(1);
            remote
                .app
                .pending_approvals
                .push(PendingApproval::new(name, input, response, agent));
            spawn_approval_poster(
                remote.client.clone(),
                remote.session_id.clone(),
                request_id,
                decision_rx,
            );
        }
        // V1b typed child lifecycle (§15): the transcript keeps rendering
        // the V1a System lines; these update the status-bar chips.
        StreamEvent::AgentSpawned { agent_id, name } => {
            remote.app.agents.push(AgentChip {
                id: agent_id,
                name,
                tool: None,
            });
            remote.live_children.store(true, Ordering::SeqCst);
        }
        StreamEvent::AgentProgress {
            agent_id,
            current_tool,
            ..
        } => {
            if let Some(chip) = remote.app.agents.iter_mut().find(|a| a.id == agent_id) {
                chip.tool = current_tool;
            }
        }
        StreamEvent::AgentCompleted { agent_id, .. } => {
            remote.app.agents.retain(|a| a.id != agent_id);
            if remote.app.agents.is_empty() {
                remote.live_children.store(false, Ordering::SeqCst);
            }
        }
        StreamEvent::TurnComplete { usage, cached, .. } => {
            if let Some(usage) = usage {
                remote.app.tool_state.last_usage = Some(usage);
            }
            remote.app.tool_state.last_cached = cached;
        }
        StreamEvent::Usage {
            tokens,
            cached,
            cost,
            output,
            gen_ms,
        } => {
            // Live context usage: emitted by the daemon after every LLM call
            // so the status bar updates mid-turn, not just at completion.
            remote.app.tool_state.last_usage = Some(tokens);
            remote.app.tool_state.last_cached = cached;
            // Cumulative spend across turns, priced once daemon-side
            // (catalog or DEX_COST_PER_1K) so client and daemon agree.
            // TurnComplete.usage repeats the final call's count, so only
            // Usage events accumulate.
            remote.app.tool_state.total_usage =
                remote.app.tool_state.total_usage.saturating_add(tokens);
            remote.app.tool_state.total_output =
                remote.app.tool_state.total_output.saturating_add(output);
            remote.app.tool_state.total_cost += cost;
            // Output rate of this call; untimed calls (compaction
            // summarizer, old daemon) hide the rate instead of keeping a
            // stale one.
            remote.app.tool_state.last_tok_s = output_rate(output, gen_ms);
        }
        StreamEvent::TurnFailed { error } => {
            append_sink_line(&mut remote.app, SinkLine::Error(error));
        }
        StreamEvent::System(msg) => {
            append_sink_line(&mut remote.app, SinkLine::System(msg));
        }
        StreamEvent::Error(msg) => {
            append_sink_line(&mut remote.app, SinkLine::Error(msg));
        }
        StreamEvent::Plan {
            goal,
            steps,
            constraints,
            acceptance,
        } => {
            remote.app.plan = crate::protocol::Plan {
                goal,
                steps,
                constraints,
                acceptance,
            };
        }
    }
}

/// Find the local JSONL for a daemon session id when files are shared
/// (default co-located daemon). Matches exact id first, then id prefix,
/// then file-stem — `Session::resume` only handles index/path, so an id
/// lookup through it silently misses and left `/resume` with no file to
/// rebuild from (blank terminal).
pub(crate) fn find_local_session_file(sid: &str) -> Option<std::path::PathBuf> {
    // Fast path first: the id is the JSONL filename, so filename matching
    // (one header read per hit) replaces the workspace-wide open+parse of
    // every session (§1). The legacy scan below only serves renamed/legacy
    // files whose stem no longer names the id.
    if let Some(path) = Session::find_by_id_filename(sid) {
        return Some(path);
    }
    let all = Session::list_all().unwrap_or_default();
    if let Some((p, _)) = all.iter().find(|(_, h)| h.id() == sid) {
        return Some(p.clone());
    }
    let q = sid.to_ascii_lowercase();
    if let Some((p, _)) = all.iter().find(|(_, h)| {
        h.id().to_ascii_lowercase().starts_with(&q) || q.starts_with(&h.id().to_ascii_lowercase())
    }) {
        return Some(p.clone());
    }
    all.into_iter()
        .find(|(p, _)| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem == sid || stem.starts_with(sid))
        })
        .map(|(p, _)| p)
}

/// Messages rendered per paint during a chunked startup replay (§1).
const REPLAY_PAINT_CHUNK: usize = 200;

/// Rebuild the transcript from the persisted JSONL messages (complete:
/// includes the user prompts the events journal never records). Returns
/// true when anything was rendered. Mirrors the local `/resume` path.
/// Progressive (§1): renders head→tail in chunks with a `paint` between,
/// so a huge history streams into the already-painted frame instead of
/// blocking first paint. `render_message_slice` threads ids across chunks
/// with no mid flush, so the final transcript is byte-identical to one-shot
/// (`app.messages` is taken for the render and restored after — the footer
/// token estimate reads it, so mid-replay frames show a stale count that
/// corrects on the final paint).
/// No-op replay paint for the mid-loop `/resume` path (the event loop's
/// own draws pick the rebuilt transcript up).
pub(crate) fn no_paint(_: &mut RemoteApp) {}

pub(crate) fn rebuild_remote_from_messages(
    remote: &mut RemoteApp,
    path: &std::path::Path,
    mut paint: impl for<'r> FnMut(&'r mut RemoteApp),
) -> bool {
    // Messages + plan ride one scan (§1): the plan used to cost a second
    // full pass right after the messages load.
    let Ok((loaded, plan)) = crate::session::load_messages_and_plan(path) else {
        return false;
    };
    if loaded.is_empty() {
        return false;
    }
    let system = remote
        .app
        .messages
        .first()
        .cloned()
        .unwrap_or(crate::protocol::ChatMessage::system(String::new()));
    remote.app.messages.clear();
    remote.app.messages.push(system);
    remote.app.messages.extend(loaded);
    let msgs = std::mem::take(&mut remote.app.messages);
    remote.app.transcript.clear();
    // Stamps restart at 0 — drop cached rows + selection (see `reset_session_state`).
    remote.app.wrapped_cache.clear();
    remote.app.display_cache.clear();
    remote.app.selection = None;
    remote.app.assistant_pending.clear();
    remote.app.assistant_gap.reset();
    remote.app.assistant_open = false;
    remote.app.thinking_open = false;
    let mut opened = std::collections::HashSet::new();
    if msgs.len() > 1 {
        // Skip the leading system message (never rendered).
        for chunk in msgs[1..].chunks(REPLAY_PAINT_CHUNK) {
            super::super::render_message_slice(&mut remote.app, chunk, &mut opened);
            paint(remote);
        }
    }
    remote.app.messages = msgs;
    super::super::flush_assistant(&mut remote.app);
    remote.app.autoscroll = true;
    if !plan.is_empty() {
        remote.app.plan = plan;
    }
    true
}

/// Replay the daemon's events journal into the transcript (best effort when
/// no local file is available, e.g. true remote). Skips parked approvals;
/// flushes the throttled assistant buffer so the replay is visible.
/// Paged (§1): the server caps pages, so each iteration is one bounded RTT
/// plus a render with a `paint` between — a giant journal streams in
/// instead of arriving as one huge slurp, and pages always advance.
pub(crate) fn replay_remote_events(
    remote: &mut RemoteApp,
    session_id: &str,
    mut paint: impl for<'r> FnMut(&'r mut RemoteApp),
) {
    let mut since = 0u64;
    loop {
        let prev = since;
        match remote.client.events(session_id, since) {
            Ok(resp) => {
                for env in resp.events {
                    // A parked parent-turn approval is dead (denied at its
                    // turn's teardown); a child approval (V1b) stays
                    // answerable and must surface.
                    if !matches!(env.event, StreamEvent::ApprovalRequired { agent: None, .. }) {
                        handle_stream_event(remote, env.event);
                    }
                }
                since = resp.next_seq;
                // The idle poller resumes from here.
                remote
                    .events_cursor
                    .fetch_max(resp.next_seq, Ordering::SeqCst);
                paint(remote);
                // EOF is raw-journal progress (`next_seq` advances on raw
                // rows, even unknown-type ones the filter above skips), not
                // the filtered count: a full page of unknown rows serves 0
                // events while the tail is unfetched, so `served < LIMIT`
                // would break early. No progress means drained — the extra
                // empty fetch this costs old daemons on exact-divide totals
                // is the documented backup, not a bug.
                if since <= prev {
                    break;
                }
            }
            Err(e) => {
                push_info(&mut remote.app, format!("replay failed: {e}"));
                break;
            }
        }
    }
    flush_assistant(&mut remote.app);
    close_thinking(&mut remote.app);
    remote.app.autoscroll = true;
    remote.app.scroll = 0;
}

pub(crate) fn finish_turn(remote: &mut RemoteApp, error: Option<String>) {
    if let Some(error) = error {
        append_sink_line(&mut remote.app, SinkLine::Error(error));
    }
    let app = &mut remote.app;
    // Drain any assistant deltas still in the throttle buffer so the final
    // text is in the transcript before the turn is torn down.
    flush_assistant(app);
    // If the turn was cancelled, restore queued steering/follow-ups to the
    // composer so the user can retry (mirrors old local `event.rs` logic).
    if app.cancel_requested {
        let mut restored = Vec::new();
        restored.append(&mut app.pending_steering);
        restored.append(&mut app.pending_followups);
        if !restored.is_empty() {
            app.input = crate::ui::input::InputField::from_text(&restored.join("\n"));
        }
    }
    app.busy = false;
    app.cancel_requested = false;
    app.cancel_presses = 0;
    // A turn can end right after thinking (cancel, failure before any text);
    // settle the indicator instead of leaving the dots animating forever.
    close_thinking(app);
    remote.cancel_flag.store(false, Ordering::SeqCst);
    settle_activity(app);
    // Tools (bash/git/write/edit) may have switched branches or dirtied the
    // tree mid-turn; refresh the footer now rather than waiting for the next
    // background poll. Async so the UI thread never blocks on HTTP (the
    // daemon's 5s git cache keeps it cheap).
    refresh_git_async(remote.client.clone(), remote.worker_tx.clone());
}
