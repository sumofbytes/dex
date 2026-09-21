use super::super::style::fg;
use super::markdown::highlight_code_block;
use crate::render::theme;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

/// Per-tool glyph heading the transcript input row — `$ bash git status` —
/// standing in for the generic `▸`. Each reads like the tool's own notation:
/// shell `$`, vim-style search `/`, diff `±` for edit, branch for git.
/// Bold so it reads as an affordance rather than content. Unknown tools (and
/// the replay fallback row) keep the generic `▸`.
fn tool_glyph(name: &str) -> &'static str {
    match name {
        "bash" => "$",
        "read" => "¶",
        "write" => "✎",
        "edit" => "±",
        "grep" | "ffgrep" | "find" | "fffind" => "/",
        "ls" => "☰",
        "git" => "⎇",
        "chain" => "→",
        name if name.starts_with("mcp__") => "⇄",
        _ => "▸",
    }
}

/// Transcript `▸ tool arg` row, headed by the tool's own glyph (`$ bash …`).
/// Bash commands highlight via the compiled bash grammar (keywords/strings/
/// flags read apart instead of one dim blob); every other tool keeps the dim
/// arg. Unknown/unhighlightable bash falls back to dim, so this never
/// regresses.
pub(crate) fn render_tool_input(name: &str, arg: &str) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            format!("{} ", tool_glyph(name)),
            fg(theme::warn_fg()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(name.to_string(), fg(theme::warn_fg())),
    ];
    // Single-line commands only: highlight_code_block splits per row and
    // only the first row is appended — multi-line would drop lines 2+.
    if name == "bash" && !arg.is_empty() && !arg.contains('\n') {
        if let Some(mut rows) = highlight_code_block("bash", arg) {
            if let Some(first) = rows.first_mut() {
                if !first.is_empty() {
                    spans.push(Span::styled(" ".to_string(), fg(theme::tool_input_fg())));
                    spans.append(first);
                    return super::super::indent_transcript_line(Line::from(spans));
                }
            }
        }
    }
    spans.push(Span::styled(format!(" {arg}"), fg(theme::tool_input_fg())));
    super::super::indent_transcript_line(Line::from(spans))
}

/// Approval-overlay detail row. Bash commands highlight the code after the
/// `$ `/indent prefix (same grammar as the transcript); paths and diff
/// markers keep their existing colors.
pub(crate) fn render_approval_detail(name: &str, detail: &str) -> Line<'static> {
    if name == "bash" {
        let (prefix, code) = if let Some(rest) = detail.strip_prefix("$ ") {
            ("$ ", rest)
        } else if detail.starts_with("$") && detail.trim() == "$" {
            return Line::from(Span::styled(detail.to_string(), fg(theme::accent_fg())));
        } else if let Some(rest) = detail.strip_prefix("  ") {
            ("  ", rest)
        } else {
            ("", detail)
        };
        // Single-line only (same first-row truncation as render_tool_input).
        if !code.is_empty() && !code.contains('\n') {
            if let Some(mut rows) = highlight_code_block("bash", code) {
                if let Some(first) = rows.first_mut() {
                    if !first.is_empty() {
                        let mut spans =
                            vec![Span::styled(prefix.to_string(), fg(theme::accent_fg()))];
                        spans.append(first);
                        return Line::from(spans);
                    }
                }
            }
        }
    }
    let style = if detail.starts_with('$') || detail.starts_with("path:") {
        fg(theme::accent_fg())
    } else if detail.starts_with("  −") {
        fg(theme::failure_fg())
    } else if detail.starts_with("  +") {
        fg(theme::success_fg())
    } else {
        fg(theme::tool_input_fg())
    };
    Line::from(Span::styled(detail.to_string(), style))
}

/// Split read's `{:>4}  content` gutter: the leading number plus its
/// two-space gap is structural, so code starting with digits (e.g.
/// `123abc`) is never mistaken for a gutter.
fn split_read_gutter(line: &str) -> Option<(String, &str)> {
    let trimmed_start = line.len() - line.trim_start().len();
    let rest = &line[trimmed_start..];
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    if !rest[digits..].starts_with("  ") {
        return None;
    }
    let code_start = trimmed_start + digits + 2;
    // The `  ` prefix below mirrors the generic preview's two-space indent.
    Some((format!("  {}", &line[..code_start]), &line[code_start..]))
}

/// Whole `read` preview rows: `==> file <==` headers stay dim landmarks
/// (and re-target the highlight language for their section, so fan-out
/// snippets highlight per file); `… +N more` tails stay dim; numbered rows
/// keep the gutter dim and highlight the code via ONE tree-sitter pass per
/// section, falling back to dim per row when unhighlightable.
pub(crate) fn render_read_preview(preview: &[String], base_lang: &str) -> Vec<Line<'static>> {
    let dim = fg(theme::tool_preview_fg());
    // Section = header + its rows; headers re-target the language.
    struct Section<'a> {
        header: Option<&'a str>,
        lang: String,
        rows: Vec<Row<'a>>,
    }
    enum Row<'a> {
        Meta(&'a str),
        Code { gutter: String, code: &'a str },
    }
    let mut sections: Vec<Section> = Vec::new();
    let mut cur = Section {
        header: None,
        lang: base_lang.to_string(),
        rows: Vec::new(),
    };
    for line in preview {
        let trimmed = line.trim_start();
        // Success headers (`==> path <==`) re-target the language per
        // section. Fan-out errors (`==> path: error: …`, no `<==`) are dim
        // landmarks, not sections.
        if trimmed.starts_with("==>") && trimmed.contains("<==") {
            if !cur.rows.is_empty() || cur.header.is_some() {
                sections.push(cur);
            }
            let inner = trimmed
                .strip_prefix("==>")
                .unwrap_or("")
                .trim_end_matches("<==")
                .trim();
            let path = inner.split_whitespace().next().unwrap_or(inner);
            let lang = crate::render::theme::lang::lang_from_path(path).to_string();
            cur = Section {
                header: Some(line.as_str()),
                lang,
                rows: Vec::new(),
            };
        } else if trimmed.starts_with('…')
            || trimmed.starts_with("[...")
            || trimmed.starts_with("==>")
        {
            cur.rows.push(Row::Meta(line.as_str()));
        } else if let Some((gutter, code)) = split_read_gutter(line) {
            cur.rows.push(Row::Code { gutter, code });
        } else if line.trim().is_empty() {
            cur.rows.push(Row::Meta(line.as_str()));
        } else {
            // Unnumbered (glob hits, plain text): highlight whole-line.
            cur.rows.push(Row::Code {
                gutter: "  ".to_string(),
                code: line.as_str(),
            });
        }
    }
    sections.push(cur);

    let mut out = Vec::with_capacity(preview.len());
    for section in &sections {
        if let Some(header) = section.header {
            out.push(super::super::indent_transcript_line(Line::from(
                Span::styled(format!("  {header}"), dim),
            )));
        }
        // One pass per section: collect code rows, highlight joined, split.
        let code_rows: Vec<usize> = section
            .rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| matches!(r, Row::Code { .. }).then_some(i))
            .collect();
        let joined = code_rows
            .iter()
            .map(|&i| match &section.rows[i] {
                Row::Code { code, .. } => *code,
                Row::Meta(_) => unreachable!(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        let highlighted = highlight_code_block(&section.lang, &joined);
        for (idx, row) in section.rows.iter().enumerate() {
            match row {
                Row::Meta(text) => {
                    out.push(super::super::indent_transcript_line(Line::from(
                        Span::styled(format!("  {text}"), dim),
                    )));
                }
                Row::Code { gutter, .. } => {
                    let pos = code_rows.iter().position(|&i| i == idx);
                    let spans = pos
                        .and_then(|p| highlighted.as_ref().and_then(|h| h.get(p)))
                        .filter(|spans| !spans.is_empty());
                    match spans {
                        Some(spans) => {
                            let mut all = vec![Span::styled(gutter.clone(), dim)];
                            all.extend(spans.iter().cloned());
                            out.push(super::super::indent_transcript_line(Line::from(all)));
                        }
                        None => {
                            let text = match row {
                                Row::Code { gutter, code } => format!("{gutter}{code}"),
                                Row::Meta(_) => unreachable!(),
                            };
                            out.push(super::super::indent_transcript_line(Line::from(
                                Span::styled(text, dim),
                            )));
                        }
                    }
                }
            }
        }
    }
    out
}

/// Split a search hit's structural gutter: `path:line:` (match) or
/// `path:line-` (context row); the first `:` + digits + `:`/`-` run wins.
/// Byte-safe: `:` and ASCII digits never occur inside a multi-byte UTF-8
/// sequence, so slicing at these offsets lands on char boundaries.
fn split_search_gutter(line: &str) -> Option<(&str, &str, char, &str)> {
    let bytes = line.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b != b':' {
            continue;
        }
        let ds = i + 1;
        let mut de = ds;
        while de < bytes.len() && bytes[de].is_ascii_digit() {
            de += 1;
        }
        if de == ds || de >= bytes.len() {
            continue;
        }
        let sep = bytes[de];
        if sep == b':' || sep == b'-' {
            return Some((&line[..i], &line[ds..de], sep as char, &line[de + 1..]));
        }
    }
    None
}

/// Flush one same-language run of search rows: ONE highlight pass over the
/// joined code (same scheme as `render_read_preview`), gutter dim, per-row
/// dim fallback when the language has no highlighter.
fn push_search_run(out: &mut Vec<Line<'static>>, run: &[(String, &str)], lang: &str, dim: Style) {
    let joined = run
        .iter()
        .map(|(_, code)| *code)
        .collect::<Vec<_>>()
        .join("\n");
    let highlighted = highlight_code_block(lang, &joined);
    for (i, (gutter, code)) in run.iter().enumerate() {
        let spans = highlighted
            .as_ref()
            .and_then(|h| h.get(i))
            .filter(|spans| !spans.is_empty());
        match spans {
            Some(spans) => {
                let mut all = vec![Span::styled(gutter.clone(), dim)];
                all.extend(spans.iter().cloned());
                out.push(super::super::indent_transcript_line(Line::from(all)));
            }
            None => out.push(super::super::indent_transcript_line(Line::from(
                Span::styled(format!("{gutter}{code}"), dim),
            ))),
        }
    }
}

/// Whole `grep`/`ffgrep` content-mode preview rows: hits are
/// `path:line:code` (context rows `path:line-code`), so the gutter is
/// structural — keep it dim and highlight the code by path extension.
/// Contiguous same-language hits share one highlight pass; bare path
/// headers (files mode, fff's fuzzy-fallback grouping with `  N: code`
/// rows) and prose stay dim.
pub(crate) fn render_search_preview(preview: &[String]) -> Vec<Line<'static>> {
    let dim = fg(theme::tool_preview_fg());
    enum Row<'a> {
        Meta(&'a str),
        Code {
            gutter: String,
            code: &'a str,
            lang: String,
        },
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut header_lang = String::new();
    for line in preview {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("[...") {
            rows.push(Row::Meta(line.as_str()));
            continue;
        }
        if let Some((path, num, sep, code)) = split_search_gutter(line) {
            rows.push(Row::Code {
                gutter: format!("  {path}:{num}{sep}"),
                code,
                lang: crate::render::theme::lang::lang_from_path(path).to_string(),
            });
            continue;
        }
        let lang = crate::render::theme::lang::lang_from_path(trimmed);
        if !trimmed.chars().any(char::is_whitespace) && !lang.is_empty() {
            // Bare path (files-mode list, fuzzy grouping): dim landmark
            // re-targeting the language for following grouped rows.
            header_lang = lang.to_string();
        }
        if !header_lang.is_empty() {
            let digits = trimmed.len()
                - trimmed
                    .trim_start_matches(|c: char| c.is_ascii_digit())
                    .len();
            if digits > 0 && trimmed[digits..].starts_with(": ") {
                rows.push(Row::Code {
                    gutter: format!("  {}: ", &trimmed[..digits]),
                    code: &trimmed[digits + 2..],
                    lang: header_lang.clone(),
                });
                continue;
            }
        }
        rows.push(Row::Meta(line.as_str()));
    }

    let mut out = Vec::with_capacity(preview.len());
    let mut runs: Vec<(String, Vec<(String, &str)>)> = Vec::new();
    for row in rows {
        match row {
            Row::Meta(text) => {
                for (lang, run) in runs.drain(..) {
                    push_search_run(&mut out, &run, &lang, dim);
                }
                out.push(super::super::indent_transcript_line(Line::from(
                    Span::styled(format!("  {text}"), dim),
                )));
            }
            Row::Code { gutter, code, lang } => match runs.iter().position(|(l, _)| *l == lang) {
                Some(i) => runs[i].1.push((gutter, code)),
                None => runs.push((lang, vec![(gutter, code)])),
            },
        }
    }
    for (lang, run) in runs {
        push_search_run(&mut out, &run, &lang, dim);
    }
    out
}
