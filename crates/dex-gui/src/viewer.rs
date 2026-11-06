//! Right-hand viewer pane: the project's files with a syntax-coloured preview, and its
//! uncommitted changes as a diff.

use rune::{El, ScrollSpec, col, row, text, text_input};
use rune_kit::diff::DiffView;
use rune_kit::shell::{Sidebar, SidebarRow};
use rune_kit::syntax::{self, SyntaxTheme};
use rune_kit::{IconButton, IconButtonVariant, Kit};

use crate::state::{State, VTab};

pub const WIDTH: f32 = 460.0;
const MAX_ROWS: usize = 200;
const MAX_LINES: usize = 3000;

fn lang(path: &str) -> Option<&'static str> {
    Some(match path.rsplit('.').next()? {
        "rs" => "rust",
        "js" | "mjs" | "jsx" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "json" => "json",
        "py" => "python",
        "go" => "go",
        "swift" => "swift",
        "sh" | "bash" => "shell",
        "sql" => "sql",
        "html" | "xml" => "html",
        "css" => "css",
        "yaml" | "yml" => "yaml",
        "diff" | "patch" => "diff",
        _ => return None,
    })
}

pub fn viewer(s: &State, k: &Kit) -> El {
    // A segmented control: the active tab sits on a raised fill, as in the Paper design.
    let tab = |label: String, icon: &'static rune::Icon, t: VTab, key: &str| {
        let on = s.vtab == t;
        let ink = if on { k.c.text_base } else { k.c.text_muted };
        let seg = row()
            .key(key.to_string())
            .h(28.0)
            .px(10.0)
            .gap(6.0)
            .items_center()
            .radius(k.radius.md)
            .fill(if on {
                k.c.surface_raised
            } else {
                rune::Color::TRANSPARENT
            })
            .child(rune::icon(icon, 14.0).color(ink))
            .child(text(label).font(k.ty.ui_medium).color(ink));
        k.interactive(seg).on_press(move |s: &mut State| {
            s.vtab = t;
            s.rescan();
        })
    };
    let changes = s.diff.files().len();
    let segments = row()
        .padding(2.0)
        .gap(2.0)
        .radius(k.radius.lg)
        .border(1.0, k.c.border_base)
        .child(tab(
            "Files".into(),
            &crate::icons::FILE,
            VTab::Files,
            "vt-files",
        ))
        .child(tab(
            if changes > 0 {
                format!("Changes {changes}")
            } else {
                "Changes".into()
            },
            &crate::icons::COMPARE,
            VTab::Changes,
            "vt-changes",
        ))
        .child(tab(
            "Terminal".into(),
            &crate::icons::TERMINAL,
            VTab::Terminal,
            "vt-term",
        ));
    let bar = row()
        .w_full()
        .h(48.0)
        .px(12.0)
        .gap(6.0)
        .items_center()
        .child(segments)
        .child(col().grow(1.0))
        .child(
            IconButton::new(&crate::icons::X)
                .variant(IconButtonVariant::Ghost)
                .label("Close viewer")
                .on_press(|s: &mut State| s.viewer = false)
                .build(k)
                .key("vt-close"),
        );
    let body = match s.vtab {
        VTab::Files => files(s, k),
        VTab::Changes => changes_view(s, k),
        VTab::Terminal => terminal(s, k),
    };
    col()
        .key("viewer")
        .w(WIDTH)
        .h_full()
        .fill(k.c.page)
        .child(bar)
        .child(body)
}

fn empty(k: &Kit, msg: &str) -> El {
    col()
        .w_full()
        .py(48.0)
        .items_center()
        .child(text(msg.to_string()).font(k.ty.ui).color(k.c.text_muted))
}

fn files(s: &State, k: &Kit) -> El {
    if let Some(f) = &s.open_file {
        return preview(f, k);
    }
    let q = s.file_filter.text.to_lowercase();
    let mut list = Sidebar::new();
    let mut shown = 0;
    let mut matched = 0;
    for p in s.files.iter().filter(|p| p.to_lowercase().contains(&q)) {
        matched += 1;
        if shown >= MAX_ROWS {
            continue;
        }
        shown += 1;
        let path = p.clone();
        list = list.child(
            SidebarRow::new(p.clone())
                .on_select(move |s: &mut State, _| s.open_path(&path))
                .build(k)
                .key(format!("f:{p}")),
        );
    }
    let mut c = col().w_full().grow(1.0).min_h(0.0).gap(8.0).px(12.0);
    c = c.child(
        text_input(&s.file_filter, k.ty.ui.lh(20.0))
            .key("file-filter")
            .placeholder("Filter files…", k.c.text_faint)
            .caret(k.c.text_base, k.c.border_focus.alpha(0.35))
            .color(k.c.text_base)
            .single_line()
            .on_edit(|s: &mut State| &mut s.file_filter)
            .w_full()
            .px(10.0)
            .py(6.0),
    );
    if s.files.is_empty() {
        return c.child(empty(k, "No files (not a git project?)"));
    }
    let list = col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(list.build(k));
    c = c.child(list);
    if matched > shown {
        c = c.child(
            text(format!("{} more — refine the filter", matched - shown))
                .font(k.ty.ui)
                .color(k.c.text_muted),
        );
    }
    c
}

fn preview(f: &crate::state::OpenFile, k: &Kit) -> El {
    let head = row()
        .w_full()
        .px(12.0)
        .gap(6.0)
        .items_center()
        .child(
            IconButton::new(&crate::icons::LEFT)
                .variant(IconButtonVariant::Ghost)
                .label("Back to files")
                .on_press(|s: &mut State| s.open_file = None)
                .build(k)
                .key("back"),
        )
        .child(breadcrumb(&f.path, k).grow(1.0).min_w(0.0));
    // Only the head of a long file is drawn.
    let mut body = String::new();
    for (i, l) in f.text.lines().take(MAX_LINES).enumerate() {
        if i > 0 {
            body.push('\n');
        }
        body.push_str(l);
    }
    let found = syntax::highlight(lang(&f.path), &body);
    let spans = SyntaxTheme::from_kit(k).spans(&found, 0);
    // A line-number gutter beside the code; both use one line height so rows line up.
    let line_count = body.lines().count().max(1);
    let numbers = (1..=line_count)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let gutter = text(numbers)
        .font(k.ty.mono.lh(20.0))
        .color(k.c.text_faint)
        .w(40.0);
    let code = rune::rich(body, spans)
        .font(k.ty.mono.lh(20.0))
        .color(k.c.text_base);
    col().w_full().grow(1.0).min_h(0.0).child(head).child(
        col()
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(
                row()
                    .w_full()
                    .gap(10.0)
                    .padding(12.0)
                    .child(gutter)
                    .child(code),
            ),
    )
}

/// `dir / file` as the design draws it: the directory muted, the file name strong.
fn breadcrumb(path: &str, k: &Kit) -> El {
    let (dir, name) = match path.rsplit_once('/') {
        Some((d, n)) => (format!("{d} /"), n.to_string()),
        None => (String::new(), path.to_string()),
    };
    row()
        .gap(6.0)
        .items_center()
        .child(text(dir).font(k.ty.ui).color(k.c.text_faint).truncate())
        .child(
            text(name)
                .font(k.ty.ui_medium)
                .color(k.c.text_base)
                .shrink(),
        )
}

fn terminal(s: &State, k: &Kit) -> El {
    let dir = s.term_cwd.clone().unwrap_or_else(|| s.cwd());
    let out = if s.term.is_empty() {
        "Run a command. One-shot shell commands only (no interactive programs).".to_string()
    } else {
        s.term.join("\n")
    };
    let input = text_input(&s.term_input, k.ty.mono.lh(20.0))
        .key("term-input")
        .placeholder(
            if s.term_running {
                "running…"
            } else {
                "command"
            },
            k.c.text_faint,
        )
        .caret(k.c.text_base, k.c.border_focus.alpha(0.35))
        .color(k.c.text_base)
        .single_line()
        .on_edit(|s: &mut State| &mut s.term_input)
        .on_submit(|s: &mut State, _| s.run_term())
        .w_full()
        .px(10.0)
        .py(8.0);
    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .scroll(ScrollSpec { stick_bottom: true })
                .child(
                    col()
                        .w_full()
                        .padding(12.0)
                        .child(text(out).font(k.ty.mono).color(k.c.text_base).wrap()),
                ),
        )
        .child(
            text(dir)
                .font(k.ty.ui)
                .color(k.c.text_muted)
                .truncate()
                .px(12.0),
        )
        .child(input)
}

fn changes_view(s: &State, k: &Kit) -> El {
    if s.scanned.is_none() {
        return empty(k, "Scanning…");
    }
    if s.diff.files().is_empty() {
        return empty(k, "No uncommitted changes");
    }
    DiffView::new(|s: &State| &s.diff, |s: &mut State| &mut s.diff)
        .build(s, k)
        .grow(1.0)
        .min_h(0.0)
}
