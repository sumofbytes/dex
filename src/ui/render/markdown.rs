use super::super::style::fg;
use crate::render::theme;
use crate::render::theme::highlight;
use crate::render::theme::markdown as md;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui_markdown::markdown::MarkdownBlock;
use ratatui_markdown::markdown::MarkdownRenderer;
use ratatui_markdown::markdown::RenderHooks;
use ratatui_markdown::ThemeConfig;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

pub(crate) fn split_markdown(s: &str) -> Vec<MarkdownBlock> {
    let lines: Vec<&str> = s.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim_start();
        if t.starts_with("```") {
            let lang = crate::render::theme::lang::normalize_code_lang(t.trim_start_matches('`'));
            let mut body = String::new();
            i += 1;
            while i < lines.len() && !lines[i].trim_start().starts_with("```") {
                body.push_str(lines[i]);
                body.push('\n');
                i += 1;
            }
            i += 1;
            // The highlight hook path splits the body on `\n`, so the
            // newline every fence body ends with (plus any blank lines the
            // model leaves before the closing fence) rendered as trailing
            // empty rows inside the box. Trailing blanks in a snippet are
            // never worth a row of air.
            blocks.push(MarkdownBlock::code_block(lang, body.trim_end_matches('\n')));
        } else if let Some(rest) = md::heading_text(t) {
            // H1-H3 map to their block; H4-H6 render as H3 (crate has no
            // H4+ variant — same as upstream parsers and glow/mdcat).
            match md::heading_level(t) {
                1 => blocks.push(MarkdownBlock::Heading1(rest.to_string())),
                2 => blocks.push(MarkdownBlock::Heading2(rest.to_string())),
                _ => blocks.push(MarkdownBlock::Heading3(rest.to_string())),
            }
            i += 1;
        } else if md::is_hr(t) {
            blocks.push(MarkdownBlock::HorizontalRule);
            i += 1;
        } else if let Some(rest) = md::blockquote_text(t) {
            blocks.push(MarkdownBlock::blockquote_text(rest.to_string()));
            i += 1;
        } else if let Some((rest, checked)) = md::task_text(t) {
            blocks.push(MarkdownBlock::TaskItem {
                text: rest.to_string(),
                indent: 0,
                checked,
            });
            i += 1;
        } else if let Some(rest) = t.strip_prefix("- ") {
            blocks.push(MarkdownBlock::ListItem(rest.to_string(), 0));
            i += 1;
        } else if let Some(rest) = t.strip_prefix("* ") {
            blocks.push(MarkdownBlock::ListItem(rest.to_string(), 0));
            i += 1;
        } else if let Some(rest) = t.strip_prefix("+ ") {
            blocks.push(MarkdownBlock::ListItem(rest.to_string(), 0));
            i += 1;
        } else if t.is_empty() {
            blocks.push(MarkdownBlock::BlankLine);
            i += 1;
        } else if md::is_table_start(&lines, i) {
            // Header + delimiter confirmed: buffer the whole table. Rows
            // accumulate until a non-table line (or a blank line) ends it.
            let mut buf = vec![lines[i].trim().to_string()];
            i += 1;
            while i < lines.len() && md::is_table_line(lines[i].trim()) {
                buf.push(lines[i].trim().to_string());
                i += 1;
            }
            blocks.push(
                build_table_block(&buf).unwrap_or_else(|| MarkdownBlock::Paragraph(buf.clone())),
            );
        } else {
            let mut para = Vec::new();
            while i < lines.len()
                && !lines[i].trim_start().is_empty()
                && !md::is_block_start(lines[i].trim_start())
                && !md::is_table_start(&lines, i)
            {
                para.push(lines[i].to_string());
                i += 1;
            }
            if para.is_empty() {
                para.push(lines[i].to_string());
                i += 1;
            }
            blocks.push(MarkdownBlock::Paragraph(para));
        }
    }
    blocks
}

/// Split a table row into cells: outer pipes are structural, `\|` is a
/// literal pipe (GFM escape), everything else separates cells. Cell text is
/// trimmed; empty rows stay empty rather than collapsing away.
fn split_table_row(line: &str) -> Vec<String> {
    let t = line.trim();
    let inner = if t.starts_with('|') && t.ends_with('|') && t.len() > 1 {
        &t[1..t.len() - 1]
    } else if let Some(rest) = t.strip_prefix('|') {
        rest
    } else if t.ends_with('|') && t.len() > 1 {
        &t[..t.len() - 1]
    } else {
        t
    };
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.peek() == Some(&'|') {
            chars.next();
            cur.push('|');
        } else if c == '|' {
            cells.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

/// Build a `MarkdownBlock::Table` from buffered table lines. The first
/// delimiter row separates the header from the body; a table whose first
/// line is itself a delimiter is not a table (fall back to a paragraph).
fn build_table_block(buf: &[String]) -> Option<MarkdownBlock> {
    let sep = buf.iter().position(|l| md::is_table_delimiter(l))?;
    if sep == 0 {
        return None;
    }
    let headers = split_table_row(&buf[sep - 1]);
    let rows: Vec<Vec<String>> = buf[sep + 1..]
        .iter()
        .filter(|l| !md::is_table_delimiter(l))
        .map(|l| split_table_row(l))
        .collect();
    Some(MarkdownBlock::Table { headers, rows })
}

/// Copy-safe code-block hooks: no box-drawing (`╭─`/`│ `/`╰─`) so native
/// terminal copies of the body rows come out as runnable commands — no
/// per-line prefix to strip. Grouping comes from a dim language label plus
/// a two-space indent and syntax colors. The label row is metadata, not
/// code: skip it when copying, as with any header. Highlighting reuses
/// `highlight_code_block` (one tree-sitter pass with sorted segments plus
/// the generic-lexer fallback) so TUI snippets match read previews and
/// headless output with no second highlighting path to drift.
struct CopySafeCodeHooks;

impl RenderHooks for CopySafeCodeHooks {
    fn render_code_block(&self, lang: &str, content: &str) -> Option<Vec<Line<'static>>> {
        let dim = fg(theme::muted_fg());
        let mut lines = Vec::new();
        if !lang.is_empty() {
            lines.push(Line::from(Span::styled(format!("  {lang}"), dim)));
        }
        // One shared helper (sorted segments, byte-safe split): the same
        // rows `render_tool_input` highlights and headless output prints.
        let highlighted = highlight_code_block(lang, content);
        match highlighted {
            Some(rows) => {
                for spans in rows {
                    if spans.is_empty() {
                        lines.push(Line::from(String::new()));
                    } else {
                        let mut line = Line::from(Span::raw("  ".to_string()));
                        line.spans.extend(spans);
                        lines.push(line);
                    }
                }
            }
            None => {
                for row in content.lines() {
                    if row.trim().is_empty() {
                        lines.push(Line::from(String::new()));
                    } else {
                        lines.push(Line::from(Span::styled(format!("  {row}"), dim)));
                    }
                }
            }
        }
        // Line-level bg marks the block's rows; `wrap_block` re-applies it to
        // every wrapped row and fills out to the full width.
        let bg = theme::code_bg();
        for line in &mut lines {
            line.style.bg = Some(bg);
        }
        Some(lines)
    }
}

/// Width the markdown renders tables at when the transcript width is unknown
/// (before the first frame, headless tests).
pub(crate) const DEFAULT_TABLE_WIDTH: u16 = 100;

#[cfg(test)]
pub(crate) fn markdown_lines(s: &str) -> Vec<Line<'static>> {
    markdown_lines_at(s, DEFAULT_TABLE_WIDTH)
}

/// Strip the inline emphasis/code markers a table cell may carry: cells are
/// laid out as aligned plain text, so the markers would only add width. Only
/// a balanced pair that wraps text is a marker — a lone `**kwargs` or the
/// `__init__.py` identifier keeps its characters, and code spans keep theirs
/// verbatim.
fn plain_cell(cell: &str) -> String {
    let parts: Vec<&str> = cell.split('`').collect();
    // An odd number of backticks leaves the last one unpaired: keep it as text.
    if parts.len().is_multiple_of(2) {
        return strip_emphasis(cell);
    }
    parts
        .iter()
        .enumerate()
        .map(|(i, part)| {
            if i % 2 == 1 {
                (*part).to_string()
            } else {
                strip_emphasis(part)
            }
        })
        .collect()
}

fn strip_emphasis(text: &str) -> String {
    let text = unwrap_pairs(text, "**", false);
    let text = unwrap_pairs(&text, "~~", false);
    unwrap_pairs(&text, "__", true)
}

/// Remove `marker` pairs that tightly wrap text (`**bold**`). With `spaced`
/// the wrapped text must contain whitespace, which keeps `__init__` intact.
fn unwrap_pairs(text: &str, marker: &str, spaced: bool) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find(marker) {
        let after = &rest[open + marker.len()..];
        let Some(close) = after.find(marker) else {
            break;
        };
        let inner = &after[..close];
        let tight = !inner.is_empty()
            && !inner.starts_with(char::is_whitespace)
            && !inner.ends_with(char::is_whitespace);
        if tight && (!spaced || inner.contains(char::is_whitespace)) {
            out.push_str(&rest[..open]);
            out.push_str(inner);
            rest = &after[close + marker.len()..];
        } else {
            out.push_str(&rest[..open + marker.len()]);
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

fn truncate_cell(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// A table as aligned columns on the terminal background: bold header, a thin
/// rule under it, two-space gutters, no box glyphs (copy-safe, like code
/// blocks). Column widths fit their content; when the table is wider than
/// `avail` the widest columns give way first and cells end in `…`, so a row
/// never wraps into the next.
fn table_lines(headers: &[String], rows: &[Vec<String>], avail: usize) -> Vec<Line<'static>> {
    const GUTTER: usize = 2;
    const MIN_COL: usize = 4;
    let cols = headers
        .len()
        .max(rows.iter().map(Vec::len).max().unwrap_or(0));
    if cols == 0 {
        return Vec::new();
    }
    let cell = |row: &[String], c: usize| plain_cell(row.get(c).map_or("", String::as_str));
    let mut widths: Vec<usize> = (0..cols)
        .map(|c| {
            std::iter::once(cell(headers, c))
                .chain(rows.iter().map(|r| cell(r, c)))
                .map(|t| UnicodeWidthStr::width(t.as_str()))
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect();
    let gutters = GUTTER * (cols - 1);
    // Shave the widest column one cell at a time until the table fits (or
    // every column is at its floor).
    while widths.iter().sum::<usize>() + gutters > avail {
        let (idx, widest) = widths
            .iter()
            .enumerate()
            .max_by_key(|(_, w)| **w)
            .map(|(i, w)| (i, *w))
            .unwrap_or((0, 0));
        if widest <= MIN_COL {
            break;
        }
        widths[idx] -= 1;
    }
    let render_row = |row: &[String], style: ratatui::style::Style| {
        let mut spans = Vec::new();
        for (c, width) in widths.iter().enumerate() {
            let text = truncate_cell(&cell(row, c), *width);
            let pad = width.saturating_sub(UnicodeWidthStr::width(text.as_str()));
            let last = c + 1 == cols;
            spans.push(Span::styled(
                if last {
                    text
                } else {
                    format!("{text}{}", " ".repeat(pad))
                },
                style,
            ));
            if !last {
                spans.push(Span::raw(" ".repeat(GUTTER)));
            }
        }
        Line::from(spans)
    };
    let mut out = vec![render_row(
        headers,
        fg(theme::surface_fg()).add_modifier(ratatui::style::Modifier::BOLD),
    )];
    let rule: Vec<Span<'static>> = widths
        .iter()
        .enumerate()
        .flat_map(|(c, w)| {
            let mut v = vec![Span::styled("─".repeat(*w), fg(theme::muted_fg()))];
            if c + 1 < cols {
                v.push(Span::raw(" ".repeat(GUTTER)));
            }
            v
        })
        .collect();
    out.push(Line::from(rule));
    out.extend(rows.iter().map(|r| render_row(r, fg(theme::surface_fg()))));
    out
}

/// Tidy the renderer's output: bullets read `• text` (one space), and a space
/// the renderer leaves between an inline-code span and the punctuation that
/// follows it (`` `x` . ``) is dropped.
fn tidy_inline(lines: &mut [Line<'static>]) {
    for line in lines {
        if let Some(first) = line.spans.first_mut() {
            if let Some(rest) = first.content.strip_prefix("•  ") {
                first.content = format!("• {rest}").into();
            }
        }
        for i in 1..line.spans.len() {
            let (head, tail) = line.spans.split_at_mut(i);
            let (prev, cur) = (&head[i - 1], &mut tail[0]);
            if prev.style != cur.style {
                if let Some(rest) = cur.content.strip_prefix(' ') {
                    if rest.starts_with(['.', ',', ';', ':', '!', '?', ')']) {
                        cur.content = rest.to_string().into();
                    }
                }
            }
        }
    }
}

/// Markdown to transcript rows; `width` is the transcript's text width, which
/// tables fit themselves to (the rows are stored pre-rendered, so a later
/// resize does not re-fit them).
pub(crate) fn markdown_lines_at(s: &str, width: u16) -> Vec<Line<'static>> {
    // Borderless code blocks (see `CopySafeCodeHooks`): the `│ ` gutter and
    // `╭─`/`╰─` rules copy as text in native terminal selections and force
    // edits before pasted commands run. Indent + highlight distinguishes
    // code without any glyph that pollutes copies (the dim language label
    // is metadata, not code — copy the body rows).
    let blocks = split_markdown(s);
    let renderer = MarkdownRenderer::new(0)
        .with_render_hooks(Box::new(CopySafeCodeHooks) as Box<dyn RenderHooks>);
    let theme = markdown_theme();
    // Tables lay themselves out (the renderer's box ignores the width);
    // everything between them renders in runs.
    let avail = (width as usize)
        .saturating_sub(super::super::TRANSCRIPT_INDENT)
        .max(20);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut run_start = 0;
    for (i, block) in blocks.iter().enumerate() {
        if let MarkdownBlock::Table { headers, rows } = block {
            out.extend(renderer.render(&blocks[run_start..i], &theme));
            out.extend(table_lines(headers, rows, avail));
            run_start = i + 1;
        }
    }
    out.extend(renderer.render(&blocks[run_start..], &theme));
    tidy_inline(&mut out);
    out
}

/// Theme-aware markdown palette: prose/borders use the terminal's real
/// foreground (readable on light + tinted themes, where the default
/// `White`/`DarkGray` slots vanish or clash); semantic hues stay ANSI so the
/// terminal remaps them. Code neutrals (variable/comment/punctuation) follow
/// the foreground instead of fixed `White`/`DarkGray`. The palette itself
/// lives in `core::highlight` so headless output uses identical colors.
fn markdown_theme() -> ThemeConfig {
    ThemeConfig::default()
        .with_text_color(theme::surface_fg())
        .with_muted_text_color(theme::muted_fg())
        .with_border_color(theme::muted_fg())
        .with_focused_border_color(theme::surface_fg())
        .with_code_colors(highlight::code_colors())
}

/// Highlight a multi-line snippet in ONE tree-sitter pass and split the
/// result back into per-line spans, so multi-line constructs (block
/// comments, triple-quoted strings) keep their style across rows. Tree-sitter
/// misses (sql, dockerfile, kotlin/groovy) fall back to the generic lexer; `None`
/// means nothing colorable — callers keep the dim fallback.
/// ponytail: byte-safe slicing throughout; a bad split falls back to dim
/// rather than panicking mid-frame.
pub(crate) fn highlight_code_block(lang: &str, code: &str) -> Option<Vec<Vec<Span<'static>>>> {
    if lang.is_empty() || code.is_empty() {
        return None;
    }
    let segs = highlight::highlight_segments(lang, code);
    if !segs.is_empty() {
        return highlight::code_block_spans(code, &segs);
    }
    highlight::fallback_code_block(lang, code)
}
