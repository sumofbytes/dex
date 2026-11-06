//! Right-hand viewer pane: the project's files with a syntax-coloured preview, and its
//! uncommitted changes as a diff.

use rune::{El, ScrollSpec, col, row, text, text_input};
use rune_kit::diff::DiffView;
use rune_kit::shell::{Sidebar, SidebarRow};
use rune_kit::syntax::{self, SyntaxTheme};
use rune_kit::{Button, ButtonVariant, IconButton, IconButtonVariant, Kit};
use rune_theme_opencode::icons;

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
    let tab = |label: &str, t: VTab, key: &str| {
        Button::new(label)
            .variant(if s.vtab == t {
                ButtonVariant::Neutral
            } else {
                ButtonVariant::Ghost
            })
            .on_press(move |s: &mut State| {
                s.vtab = t;
                s.rescan();
            })
            .build(k)
            .key(key.to_string())
    };
    let changes = s.diff.files().len();
    let bar = row()
        .w_full()
        .h(48.0)
        .px(12.0)
        .gap(6.0)
        .items_center()
        .child(tab("Files", VTab::Files, "vt-files"))
        .child(tab(
            &if changes > 0 {
                format!("Changes {changes}")
            } else {
                "Changes".into()
            },
            VTab::Changes,
            "vt-changes",
        ))
        .child(col().grow(1.0))
        .child(
            IconButton::new(&icons::CLOSE)
                .variant(IconButtonVariant::Ghost)
                .label("Close viewer")
                .on_press(|s: &mut State| s.viewer = false)
                .build(k)
                .key("vt-close"),
        );
    let body = match s.vtab {
        VTab::Files => files(s, k),
        VTab::Changes => changes_view(s, k),
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
            IconButton::new(&icons::CLOSE)
                .variant(IconButtonVariant::Ghost)
                .label("Back to files")
                .on_press(|s: &mut State| s.open_file = None)
                .build(k)
                .key("back"),
        )
        .child(
            text(f.path.clone())
                .font(k.ty.ui_medium)
                .color(k.c.text_base)
                .truncate()
                .grow(1.0)
                .min_w(0.0),
        );
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
    let code = rune::rich(body, spans).font(k.ty.mono).color(k.c.text_base);
    col().w_full().grow(1.0).min_h(0.0).child(head).child(
        col()
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .child(col().w_full().padding(12.0).child(code)),
    )
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
