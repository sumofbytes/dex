use super::super::theme;
use crate::core::highlight;
use crate::core::markdown as md;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui_markdown::markdown::MarkdownBlock;
use ratatui_markdown::markdown::MarkdownRenderer;
use ratatui_markdown::markdown::RenderHooks;
use ratatui_markdown::ThemeConfig;

pub(crate) fn split_markdown(s: &str) -> Vec<MarkdownBlock> {
    let lines: Vec<&str> = s.lines().collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim_start();
        if t.starts_with("```") {
            let lang = crate::core::highlight::normalize_code_lang(t.trim_start_matches('`'));
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
        let dim = Style::default().fg(theme::muted_fg());
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
        Some(lines)
    }
}

pub(crate) fn markdown_lines(s: &str) -> Vec<Line<'static>> {
    // Borderless code blocks (see `CopySafeCodeHooks`): the `│ ` gutter and
    // `╭─`/`╰─` rules copy as text in native terminal selections and force
    // edits before pasted commands run. Indent + highlight distinguishes
    // code without any glyph that pollutes copies (the dim language label
    // is metadata, not code — copy the body rows).
    let blocks = split_markdown(s);
    let renderer = MarkdownRenderer::new(0)
        .with_render_hooks(Box::new(CopySafeCodeHooks) as Box<dyn RenderHooks>);
    renderer.render(&blocks, &markdown_theme())
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
