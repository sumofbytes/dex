//! Views: sidebar, transcript, composer, footer. Pure functions of `State`.

use dex_client::protocol::ApprovalDecision;
use rune::{Cx, El, ScrollSpec, col, el, row, spacer, text, text_input};
use rune_kit::markdown::{Markdown, MarkdownStyle};
use rune_kit::{Button, ButtonVariant, IconButton, IconButtonVariant, Kit, Spinner};
use rune_theme_opencode::icons;

use crate::state::{Cmd, Item, Page, State, VTab};
use crate::style::{space, typo};

const COLUMN: f32 = 760.0;
/// Left indent of what opens under a quiet row: tool calls and a thought's text share it.
const NESTED: f32 = 14.0;
/// The content line: text, bubbles and the footer sit this far inside the composer card's edge,
/// where the input's own text starts, so everything shares a left and a right edge.
const CONTENT: f32 = 12.0;

pub fn view(s: &State, _cx: &mut Cx<State>) -> El {
    let k = crate::theme::kit();
    let mut layer = Vec::new();
    let page = match s.page {
        Page::Home => crate::home::home(s, k),
        Page::Thread => main_pane(s, k, &mut layer),
    };
    let body = if s.sidebar {
        row()
            .w_full()
            .h_full()
            .fill(k.c.page)
            .child(crate::sidebar::sidebar(s, k).w(crate::sidebar::tok::WIDTH))
            .child(el().w(1.0).h_full().fill(k.c.border_base))
            .child(page.grow(1.0).min_w(0.0))
            .children(viewer_col(s, k))
    } else {
        row().w_full().h_full().fill(k.c.page).child(page.grow(1.0))
    };
    crate::theme_picker::picker(s, k, &mut layer);
    rune_kit::root(body, layer)
}

/// The viewer pane with its divider, on thread pages only.
fn viewer_col(s: &State, k: &Kit) -> Vec<El> {
    if !(s.viewer && s.page == Page::Thread) {
        return Vec::new();
    }
    vec![
        el().w(1.0).h_full().fill(k.c.border_base),
        crate::viewer::viewer(s, k),
    ]
}

fn main_pane(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let card = col()
        .key("main")
        .h_full()
        .fill(k.c.page)
        .child(header(s, k))
        .child(transcript(s, k))
        .child(composer(s, k, layer))
        .child(footer(s, k));
    card.grow(1.0).min_h(0.0)
}

fn header(s: &State, k: &Kit) -> El {
    let toggle = IconButton::new(&crate::icons::PANEL)
        .variant(IconButtonVariant::Ghost)
        .label("Toggle sidebar")
        .on_press(|s: &mut State| s.sidebar = !s.sidebar)
        .build(k)
        .key("toggle-sidebar");
    let title = text(s.title())
        .font(k.ty.ui_medium)
        .color(k.c.text_base)
        .truncate();
    let wt = crate::worktree::info(&s.cwd());
    let mut stats = row().gap(space::MD).items_center();
    if let Some(w) = &wt {
        stats = stats
            .child(text(w.branch.clone()).font(k.ty.mono).color(k.c.text_muted))
            .child(
                Button::new("Merge")
                    .variant(ButtonVariant::Submit)
                    .on_press(|s: &mut State| s.finish_worktree(true))
                    .build(k)
                    .key("wt-merge"),
            )
            .child(if s.confirm_discard {
                Button::new("Discard work?")
                    .variant(ButtonVariant::Danger)
                    .on_press(|s: &mut State| s.finish_worktree(false))
                    .build(k)
                    .key("wt-discard-yes")
            } else {
                Button::new("Discard")
                    .variant(ButtonVariant::Ghost)
                    .on_press(|s: &mut State| s.confirm_discard = true)
                    .build(k)
                    .key("wt-discard")
            });
    } else if s.current.is_none() {
        stats = stats.child(
            Button::new("Worktree")
                .variant(if s.use_worktree {
                    ButtonVariant::Neutral
                } else {
                    ButtonVariant::Ghost
                })
                .on_press(|s: &mut State| s.use_worktree = !s.use_worktree)
                .build(k)
                .key("wt-toggle"),
        );
    }
    if s.busy {
        stats = stats.child(Spinner::new().build(k));
    }
    if s.tokens > 0 {
        stats = stats.child(
            text(format!("{}  ${:.2}", human(s.tokens), s.cost))
                .font(k.ty.ui)
                .color(k.c.text_muted),
        );
    }
    row()
        .w_full()
        .h(48.0)
        .px(space::XL)
        .gap(space::MD)
        .items_center()
        .child(toggle)
        .child(title.grow(1.0).min_w(0.0))
        .child(stats)
        .child(
            IconButton::new(&crate::icons::SLIDERS)
                .variant(IconButtonVariant::Ghost)
                .label("Theme (Ctrl+,)")
                .on_press(|s: &mut State| s.theme_open = true)
                .build(k)
                .key("open-theme"),
        )
        .child(
            IconButton::new(&crate::icons::TERMINAL)
                .variant(IconButtonVariant::Ghost)
                .label("Terminal (Ctrl+`)")
                .on_press(|s: &mut State| s.show_viewer(VTab::Terminal))
                .build(k)
                .key("open-term"),
        )
        .child(
            IconButton::new(&crate::icons::FOLDER)
                .variant(IconButtonVariant::Ghost)
                .label("Files (Ctrl+E)")
                .on_press(|s: &mut State| s.show_viewer(VTab::Files))
                .build(k)
                .key("open-files"),
        )
}

fn human(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn transcript(s: &State, k: &Kit) -> El {
    // Text leads: items sit 14 points apart, and a turn's user message gets a little more room.
    let mut list = col()
        .w_full()
        .max_w(COLUMN)
        .gap(space::XXL)
        .py(space::XXL)
        .px(CONTENT);
    if s.items.is_empty() {
        list = list.child(
            col()
                .w_full()
                .py(120.0)
                .items_center()
                .gap(space::SM)
                .child(
                    text("What should we work on?")
                        .font(k.ty.body_medium)
                        .color(k.c.text_base),
                )
                .child(text(s.cwd()).font(k.ty.ui).color(k.c.text_muted)),
        );
    }
    let mut i = 0;
    while i < s.items.len() {
        if crate::state::is_work(&s.items[i]) {
            // A run of thoughts and tool calls folds into one "Worked" block.
            let start = i;
            while i < s.items.len() && crate::state::is_work(&s.items[i]) {
                i += 1;
            }
            list = list.child(work_block(s, k, start, i));
            continue;
        }
        let mut el = item(i, &s.items[i], k).key(format!("item{i}"));
        // Rhythm: 16 between blocks; +16 around a user message (32), +4 from tools to the reply (20).
        if i > 0 && (matches!(s.items[i], Item::User(_)) || matches!(s.items[i - 1], Item::User(_)))
        {
            el = col().w_full().gap(0.0).child(el.pt(space::XXL));
        } else if i > 0
            && matches!(s.items[i], Item::Assistant { .. })
            && crate::state::is_work(&s.items[i - 1])
        {
            el = col().w_full().gap(0.0).child(el.pt(space::XS));
        }
        list = list.child(el);
        i += 1;
    }
    // The viewport must be a column: a row would stretch its child to the viewport's height,
    // so the content would never overflow and nothing could scroll.
    col()
        .key("transcript")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec { stick_bottom: true })
        .child(
            row()
                .w_full()
                .justify_center()
                .px(space::XXL)
                .child(list.grow(1.0)),
        )
}

/// A run of tool calls (`start..end`): a quiet summary line that opens to the rows.
/// A run of thoughts and tool calls (`start..end`) as one block. Done: a quiet row, `› Worked
/// for 14s  2 thoughts · 5 commands`, collapsed; opened it nests the rows in order, each thought
/// still folding on its own. Live (the run is still being worked): a spinner and "Working", open.
fn work_block(s: &State, k: &Kit, start: usize, end: usize) -> El {
    let live = s.busy && end == s.items.len();
    let open = live || s.open_work.contains(&start);
    let (thoughts, commands, secs) = crate::state::work_summary(&s.items[start..end]);
    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let detail = [
        (thoughts > 0).then(|| plural(thoughts, "thought", "thoughts")),
        (commands > 0).then(|| plural(commands, "command", "commands")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let head = row()
        .key(format!("work{start}"))
        .h(20.0)
        .gap(space::SM)
        .items_center();
    let head = if live {
        // The first thought's start gives a running time; without one the label stands alone.
        let label = match s.items[start..end].iter().find_map(|i| match i {
            Item::Thinking {
                started: Some(t), ..
            } => Some(t.elapsed().as_secs()),
            _ => None,
        }) {
            Some(n) if n >= 1 => format!("Working · {n}s"),
            _ => "Working".to_string(),
        };
        head.child(Spinner::new().size(12.0).build(k))
            .child(text(label).font(k.ty.ui_medium).color(k.c.text_muted))
    } else {
        let chevron = if open {
            &crate::icons::DOWN
        } else {
            &crate::icons::RIGHT
        };
        let label = if secs.round() >= 1.0 {
            format!("Worked for {}s", secs.round() as u64)
        } else {
            "Worked".to_string()
        };
        head.child(rune::icon(chevron, 12.0).color(k.c.text_faint))
            .child(text(label).font(typo::meta(k)).color(k.c.text_faint))
            .child(
                text(detail)
                    .font(typo::meta(k))
                    .color(k.c.text_faint.alpha(0.75)),
            )
    };
    let head = if live {
        head
    } else {
        k.interactive(head).on_press(move |s: &mut State| {
            if !s.open_work.remove(&start) {
                s.open_work.insert(start);
            }
        })
    };
    let mut group = col().w_full().gap(space::XS).child(head);
    if open {
        for i in start..end {
            // Thoughts inside a block are plain rows: the block's header carries the spinner.
            group = group.child(item(i, &s.items[i], k).key(format!("item{i}")).pl(NESTED));
        }
    }
    group
}

/// The reply's type, per the transcript design: 16px on a 24px line, lists with a 22px marker
/// column and 6px between items.
fn reply_style(k: &Kit) -> MarkdownStyle {
    let heading = |size: f32| typo::heading(k, size);
    MarkdownStyle::from_kit(k)
        .body_font(typo::reply(k))
        .heading_font(1, heading(18.0))
        .heading_font(2, heading(16.0))
        .heading_font(3, heading(14.0))
        .list(22.0, 6.0)
}

/// A model thought inside a work block: `› Thought · 4s`, the same quiet row as everything
/// else; opening turns the chevron and shows the text below, indented like tool rows.
fn thought(i: usize, k: &Kit, text_: &str, open: bool, secs: Option<u64>) -> El {
    let chevron = if open {
        &crate::icons::DOWN
    } else {
        &crate::icons::RIGHT
    };
    let label = match secs {
        Some(n) if n >= 1 => format!("Thought · {n}s"),
        _ => "Thought".to_string(),
    };
    let head = row()
        .key(format!("thought{i}"))
        .h(20.0)
        .gap(space::SM)
        .items_center()
        .child(rune::icon(chevron, 12.0).color(k.c.text_faint))
        .child(text(label).font(typo::meta(k)).color(k.c.text_faint));
    let mut group = col()
        .w_full()
        .gap(space::XS)
        .child(k.interactive(head).on_press(move |s: &mut State| {
            if let Some(Item::Thinking { open, .. }) = s.items.get_mut(i) {
                *open = !*open;
            }
        }));
    if open {
        group = group.child(
            text(text_.to_string())
                .font(typo::meta(k))
                .color(k.c.text_faint)
                .wrap()
                .pl(NESTED),
        );
    }
    group
}

fn item(i: usize, it: &Item, k: &Kit) -> El {
    match it {
        Item::User(t) => row().w_full().justify_end().child(
            col()
                .max_w(560.0)
                .px(space::XXL)
                .py(space::XL)
                .radius(k.radius.xl)
                .fill(k.c.layer_02)
                .child(
                    text(t.clone())
                        .font(typo::reply(k))
                        .color(k.c.text_base)
                        .wrap(),
                ),
        ),
        Item::Assistant { text: t, streaming } => Markdown::new(t.clone())
            .style(reply_style(k))
            .streaming(*streaming)
            .build(k)
            .w_full(),
        Item::Thinking {
            text: t,
            open,
            secs,
            ..
        } => thought(i, k, t, *open, *secs),
        Item::Tool(t) => {
            // A compact row, as in the transcript design: the tool's name, its arguments and the
            // duration. Its output follows dimmed under a `↳`: the first line when closed, all of it
            // when opened.
            let failed = matches!(&t.done, Some(d) if !d.ok);
            let secs = match &t.done {
                Some(d) => format!("{:.1}s", d.secs),
                None => String::new(),
            };
            let label = row()
                .gap(0.0)
                .child(
                    text(t.name.clone())
                        .font(k.ty.mono)
                        .color(crate::theme::tool_label()),
                )
                .child(text(":").font(k.ty.mono).color(crate::theme::tool_label()));
            let head = row()
                .key(format!("tool{i}"))
                .w_full()
                .h(24.0)
                .gap(space::SM)
                .items_center()
                .child(label)
                .child(
                    text(t.args.clone())
                        .font(k.ty.mono)
                        .color(k.c.text_muted)
                        .truncate()
                        .grow(1.0)
                        .min_w(0.0),
                )
                .child(text(secs).font(k.ty.mono).color(if failed {
                    k.c.danger.fg
                } else {
                    k.c.text_faint
                }));
            let mut c = col()
                .w_full()
                .gap(space::XXS)
                .child(k.interactive(head).on_press(move |s: &mut State| {
                    if let Some(Item::Tool(t)) = s.items.get_mut(i) {
                        t.open = !t.open;
                    }
                }));
            if let Some(d) = &t.done {
                let ink = if failed {
                    k.c.danger.fg
                } else {
                    k.c.text_faint
                };
                let lines: Vec<&String> = if t.open {
                    std::iter::once(&d.summary)
                        .chain(d.preview.iter())
                        .collect()
                } else {
                    d.preview.iter().take(1).collect()
                };
                for (n, l) in lines.into_iter().enumerate() {
                    let arrow = if n == 0 { "↳" } else { " " };
                    c = c.child(
                        row()
                            .w_full()
                            .gap(space::MD)
                            .pl(space::XS)
                            .child(text(arrow).font(k.ty.mono).color(k.c.text_faint))
                            .child(
                                text(l.clone())
                                    .font(k.ty.mono)
                                    .color(ink)
                                    .truncate()
                                    .grow(1.0)
                                    .min_w(0.0),
                            ),
                    );
                }
            }
            c
        }
        Item::Notice(t) => text(t.clone())
            .font(typo::meta(k))
            .color(k.c.text_faint)
            .w_full()
            .wrap(),
        Item::Error(t) => col()
            .w_full()
            .px(space::XL)
            .py(space::MD)
            .radius(k.radius.md)
            .fill(k.c.danger.bg)
            .child(text(t.clone()).font(k.ty.ui).color(k.c.danger.fg)),
        Item::Approval {
            request_id,
            name,
            input,
            decided,
        } => {
            let head = text(format!("Allow {name}?"))
                .font(k.ty.ui_medium)
                .color(k.c.text_base);
            let detail: String = input.chars().take(400).collect();
            let mut c = col()
                .w_full()
                .px(14.0)
                .py(space::XL)
                .gap(space::MD)
                .radius(k.radius.lg)
                .fill(k.c.layer_02)
                .border(1.0, k.c.border_base)
                .child(head)
                .child(text(detail).font(k.ty.mono).color(k.c.text_muted));
            c = match decided {
                Some(d) => c.child(
                    text(match d {
                        ApprovalDecision::Deny => "Denied",
                        ApprovalDecision::AllowOnce => "Allowed once",
                        ApprovalDecision::AllowSession => "Allowed for session",
                    })
                    .font(typo::meta(k))
                    .color(k.c.text_faint),
                ),
                None => {
                    let btn = |label: &'static str, v: ButtonVariant, d: ApprovalDecision| {
                        let rid = request_id.clone();
                        Button::new(label)
                            .variant(v)
                            .on_press(move |s: &mut State| {
                                if let Some(Item::Approval { decided, .. }) = s.items.get_mut(i) {
                                    *decided = Some(d);
                                }
                                s.send_cmd(Cmd::Approve {
                                    request_id: rid.clone(),
                                    decision: d,
                                });
                            })
                            .build(k)
                            .key(label)
                    };
                    c.child(
                        row()
                            .gap(space::MD)
                            .child(btn(
                                "Allow once",
                                ButtonVariant::Contrast,
                                ApprovalDecision::AllowOnce,
                            ))
                            .child(btn(
                                "Allow for session",
                                ButtonVariant::Neutral,
                                ApprovalDecision::AllowSession,
                            ))
                            .child(btn("Deny", ButtonVariant::Ghost, ApprovalDecision::Deny)),
                    )
                }
            };
            c
        }
    }
}

fn composer(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let input = text_input(&s.edit, k.ty.body.lh(22.0))
        .key("composer-input")
        .placeholder("Ask anything…", k.c.text_faint)
        .caret(k.c.text_base, k.c.border_focus.alpha(0.35))
        .color(k.c.text_base)
        .on_edit(|s: &mut State| &mut s.edit)
        .on_submit(|s: &mut State, _| s.submit())
        .on_input_key(|s: &mut State, e: &rune::KeyEvent, _| {
            // Alt+Enter queues a follow-up that runs after the current turn.
            if matches!(e.key, rune::Key::Enter) && e.mods.alt {
                s.queue_followup();
                return true;
            }
            false
        })
        .w_full()
        .px(space::MD)
        .py(space::MD)
        .min_h(44.0)
        .max_h(220.0);

    let mode = crate::composer_menu::mode(s, k, layer);
    let model = crate::composer_menu::model(s, k, layer);

    let action = if s.busy {
        IconButton::new(&icons::STOP)
            .variant(IconButtonVariant::Ghost)
            .label("Stop")
            .on_press(|s: &mut State| s.send_cmd(Cmd::Cancel))
            .build(k)
    } else {
        IconButton::new(&crate::icons::UP)
            .variant(IconButtonVariant::Submit)
            .label("Send")
            .on_press(|s: &mut State| s.submit())
            .build(k)
    }
    .key("send");

    let card = col()
        .w_full()
        .max_w(COLUMN)
        .padding(space::XS)
        .gap(space::XXS)
        .radius(k.radius.xl)
        .fill(k.c.layer_02)
        .border(1.0, k.c.border_base)
        .child(input)
        .child(
            // Chips carry their own 8 padding and the button sits 8 in, so chip text, the send
            // button and the input text all land on the content line (card edge + 12).
            row()
                .gap(space::MD)
                .items_center()
                .pr(space::MD)
                .pb(space::MD)
                .child(mode)
                .child(model)
                .child(spacer())
                .child(action),
        );
    let mut stack = col().w_full().max_w(COLUMN).gap(space::SM);
    for (i, q) in s.queue.iter().enumerate() {
        let label = if q.followup { "Queued" } else { "Steering" };
        stack = stack.child(
            row()
                .key(format!("queued{i}"))
                .w_full()
                .px(space::XL)
                .py(space::XS)
                .gap(space::MD)
                .items_center()
                .radius(k.radius.lg)
                .border(1.0, k.c.border_muted)
                .child(text(label).font(typo::meta(k)).color(k.c.text_faint))
                .child(
                    text(q.text.clone())
                        .font(k.ty.ui)
                        .color(k.c.text_muted)
                        .truncate()
                        .grow(1.0)
                        .min_w(0.0),
                )
                .child(
                    IconButton::new(&icons::CLOSE)
                        .variant(IconButtonVariant::Ghost)
                        .label("Take back")
                        .on_press(move |s: &mut State| s.recall(i))
                        .build(k)
                        .key("recall"),
                ),
        );
    }
    let stack = stack.child(card);
    row()
        .w_full()
        .justify_center()
        .px(space::XXL)
        .pb(space::MD)
        .child(stack.grow(1.0))
}

/// The line under the composer: the project path and branch on the left, the agent's status
/// on the right. Transcript, composer text and this line share one content line (`CONTENT` inside the card).
fn footer(s: &State, k: &Kit) -> El {
    let meta = typo::footer(k);
    let mut left = row()
        .grow(1.0)
        .min_w(0.0)
        .gap(space::SM)
        .items_center()
        .child(
            text(s.cwd())
                .font(meta)
                .color(k.c.text_faint)
                .truncate()
                .shrink(),
        );
    if let Some(b) = &s.info.branch {
        left = left
            .child(text("·").font(typo::caption(k)).color(k.c.border_strong))
            .child(rune::icon(&crate::icons::BRANCH, 12.0).color(k.c.text_faint))
            .child(text(b.clone()).font(meta).color(k.c.text_faint).truncate());
    }
    let ready = s.status == "ready";
    let dot = if ready {
        crate::theme::done()
    } else {
        crate::theme::waiting()
    };
    let status = row()
        .gap(space::SM)
        .items_center()
        .child(crate::style::dot(dot, 6.0))
        .child(
            text(format!("{} · {}", s.info.provider, s.status))
                .font(typo::caption(k))
                .color(k.c.text_faint),
        );
    row().w_full().justify_center().px(space::XXL).child(
        row()
            .w_full()
            .max_w(COLUMN)
            .h(36.0)
            .px(CONTENT)
            .pb(space::MD)
            .gap(space::MD)
            .items_center()
            .child(left)
            .child(status),
    )
}
