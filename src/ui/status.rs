use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{format_tokens, theme, App};

pub(super) fn compact_path(path: &str) -> String {
    if let Ok(home) = std::env::var("HOME") {
        if let Some(rest) = path.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

/// Make text safe to put in buffer cells: backends print cell symbols raw,
/// but ratatui models every grapheme as one column. A tab advances the real
/// cursor to the next tab stop (8 columns) while the model still thinks
/// it moved one, desyncing every later cell of the frame — the transcript
/// then shows stale fragments mixed into fresh rows. Expand tabs to the
/// next tab stop and drop other C0 controls entirely.
pub(super) fn cell_safe(text: &str) -> String {
    if !text.chars().any(char::is_control) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut col: usize = 0;
    for c in text.chars() {
        match c {
            '\t' => {
                let spaces = super::TAB_WIDTH - (col % super::TAB_WIDTH);
                out.push_str(&" ".repeat(spaces));
                col += spaces;
            }
            c if c.is_control() => {}
            c => {
                let w = c.width().unwrap_or(0);
                out.push(c);
                col += w;
            }
        }
    }
    out
}

pub(super) fn truncate_display(text: &str, width: u16) -> String {
    let text = cell_safe(text);
    let width = width as usize;
    if UnicodeWidthStr::width(text.as_str()) <= width {
        return text;
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cw = ch.width().unwrap_or(0);
        if used + cw + 1 > width {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

/// One styled run of status-bar text.
pub(super) type Piece = (String, Style);

fn quiet_style() -> Style {
    Style::default().fg(theme::muted_fg())
}

fn sep() -> Piece {
    (" · ".to_string(), quiet_style())
}

fn quiet(text: impl Into<String>) -> Piece {
    (text.into(), quiet_style())
}

/// Style for the context-usage run, graduating with pressure: quiet while
/// there is headroom, the app's warning yellow at ≥75% of the compaction
/// trigger (`context_window - reserve_tokens`), red once past it.
fn context_style(app: &App, tokens: u64) -> Style {
    if app.config.context_window == 0 {
        return quiet_style();
    }
    let threshold = app.config.compaction_threshold();
    if tokens >= threshold {
        Style::default().fg(Color::LightRed)
    } else if tokens.saturating_mul(4) >= threshold.saturating_mul(3) {
        Style::default().fg(Color::Yellow)
    } else {
        quiet_style()
    }
}

/// Session cost like pi's footer: `$X.XXX`, catalog-priced when possible
/// else `DEX_COST_PER_1K` fallback. `Some` once any prompt has been billed.
/// Shared by the full and narrowed status lines so spend stays visible when
/// the full line no longer fits beside the connection badge. Styled in the
/// terminal's own foreground (`theme::surface_fg`, derived from the OSC 11
/// query): one step brighter than the quiet facts for a little attention,
/// but still on-theme for light/dark/tinted terminals instead of a fixed
/// ANSI slot.
fn cost_piece(app: &App) -> Option<Piece> {
    if app.tool_state.total_cost > 0.0005 {
        Some((
            format!("${:.3}", app.tool_state.total_cost),
            Style::default().fg(theme::surface_fg()),
        ))
    } else {
        None
    }
}

/// Model identity as one run: `provider/model`. Model ids that already
/// carry a provider prefix (OpenRouter-style `openai/gpt-5.2`) render
/// as-is instead of doubling it.
fn model_label(app: &App) -> String {
    let model = app.config.model.as_str();
    if model.contains('/') {
        model.to_string()
    } else {
        format!("{}/{}", app.config.provider.name(), model)
    }
}

fn push_cost(pieces: &mut Vec<Piece>, app: &App) {
    if let Some(cost) = cost_piece(app) {
        push_sep(pieces);
        pieces.push(cost);
    }
}

/// A separator joins items; it never opens a line (the no-cwd tier without
/// a branch starts from nothing).
fn push_sep(pieces: &mut Vec<Piece>) {
    if !pieces.is_empty() {
        pieces.push(sep());
    }
}

/// Branch badge shared by the full and compact lines (single source so the
/// two tiers cannot drift). Callers place the separating ` · ` themselves.
fn branch_pieces(app: &App) -> Vec<Piece> {
    let mut pieces = Vec::new();
    if let Some(branch) = app.git_branch.as_ref() {
        pieces.push((branch.clone(), Style::default().fg(Color::LightGreen)));
        if app.git_dirty {
            pieces.push(("*".to_string(), Style::default().fg(Color::Yellow)));
        }
    }
    pieces
}

/// The full left-side status as styled runs. Quiet facts use the theme's
/// muted foreground; spend uses the terminal's own foreground for a subtle
/// step up in prominence. Other accents reuse the app's semantic ANSI colors
/// (Cyan identity, LightGreen clean branch, Yellow warnings, LightRed past
/// the compaction trigger), which terminal themes remap to their own palette.
///
/// Width discipline: the full line must fit an ~110-column terminal or it
/// falls to the next tier and every live number vanishes, so each item is
/// written as tight as it stays readable — `ctx 12k/128k 9%`, `8k cached`,
/// `↑42k ↓1.2k` (cumulative in/out), `123 tok/s`. Redundant detail is
/// dropped, never information: the cached % is the adjacent pair divided by
/// eye, and a trailing `.0` on scaled tokens says nothing.
pub(super) fn status_pieces(app: &App, with_cwd: bool) -> Vec<Piece> {
    // Transient notice (copy confirmation) takes over the line until it
    // expires: unmissable feedback beats the quiet facts for two seconds.
    if let Some((text, at)) = &app.notice {
        if at.elapsed() < super::NOTICE_LIFETIME {
            return vec![(text.clone(), Style::default().fg(Color::Green))];
        }
    }
    let tokens = app
        .tool_state
        .last_usage
        .unwrap_or_else(|| crate::agent::compaction::estimate_tokens(&app.messages));
    let context_pct = if app.config.context_window == 0 {
        0
    } else {
        tokens
            .saturating_mul(100)
            .checked_div(app.config.context_window)
            .unwrap_or(0)
    };
    let mut pieces = Vec::new();
    if with_cwd {
        pieces.push((compact_path(&app.cwd), Style::default().fg(Color::Cyan)));
    }
    push_sep(&mut pieces);
    pieces.extend(branch_pieces(app));
    push_sep(&mut pieces);
    pieces.push(quiet(model_label(app)));
    pieces.push(sep());
    // Live context usage against the window. The % duplicates the pair, but
    // it is the glanceable readout behind the pressure color, so it stays.
    let ctx_text = if app.config.context_window > 0 {
        format!(
            "ctx {}/{} {}%",
            format_tokens(tokens),
            format_tokens(app.config.context_window),
            context_pct
        )
    } else {
        format!("ctx {}", format_tokens(tokens))
    };
    pieces.push((ctx_text, context_style(app, tokens)));
    // Provider-reported cache-hit subset of the last call's prompt (billed
    // at a fraction of full input price); omitted until a provider reports
    // it. The hit % is derivable from this and the ctx number, so only the
    // absolute is shown.
    if let Some(cached) = app.tool_state.last_cached {
        if cached > 0 {
            pieces.push(sep());
            pieces.push(quiet(format!("{} cached", format_tokens(cached))));
        }
    }
    // Cumulative prompt (↑) and completion (↓) tokens across all LLM calls
    // this TUI process has made — the spend-side figures that grow across
    // turns, vs the % above which is live context usage. Each half appears
    // only once billed.
    if app.tool_state.total_usage > 0 || app.tool_state.total_output > 0 {
        let mut flow = String::new();
        if app.tool_state.total_usage > 0 {
            flow.push_str(&format!("↑{}", format_tokens(app.tool_state.total_usage)));
        }
        if app.tool_state.total_output > 0 {
            if !flow.is_empty() {
                flow.push(' ');
            }
            flow.push_str(&format!("↓{}", format_tokens(app.tool_state.total_output)));
        }
        pieces.push(sep());
        pieces.push(quiet(flow));
    }
    // Output rate of the most recent LLM call (completion tokens over the
    // daemon-measured call duration). Sub-1 tok/s rounds to a lie, so it
    // stays hidden.
    if let Some(rate) = app.tool_state.last_tok_s {
        if rate >= 1.0 {
            pieces.push(sep());
            pieces.push(quiet(format!("{rate:.0} tok/s")));
        }
    }
    // Session cost like pi's footer: `$X.XXX`, catalog-priced when possible
    // else `DEX_COST_PER_1K` fallback. Shown once any prompt has been billed.
    push_cost(&mut pieces, app);
    pieces
}

pub(super) fn ui_status(app: &App) -> String {
    status_pieces(app, true)
        .into_iter()
        .map(|(text, _)| text)
        .collect()
}

fn compact_pieces(app: &App) -> Vec<Piece> {
    // Narrow tier: cwd + branch + model + spend. Branch and cost share
    // helpers with the full line so the tiers cannot drift.
    let mut pieces = vec![(compact_path(&app.cwd), Style::default().fg(Color::Cyan))];
    push_sep(&mut pieces);
    pieces.extend(branch_pieces(app));
    push_sep(&mut pieces);
    pieces.push(quiet(app.config.model.clone()));
    push_cost(&mut pieces, app);
    pieces
}

fn bare_pieces(app: &App) -> Vec<Piece> {
    let mut pieces = vec![quiet(app.config.model.clone())];
    push_cost(&mut pieces, app);
    pieces
}

/// How this TUI reached its engine. Remote is the exceptional state worth
/// noticing; a local daemon is a quiet fact.
fn conn_piece(app: &App) -> Piece {
    let conn = app
        .connection
        .clone()
        .unwrap_or_else(|| "[L] local".to_string());
    let style = if conn.starts_with("[R]") {
        Style::default().fg(Color::Cyan)
    } else {
        quiet_style()
    };
    (conn, style)
}

/// Scroll hint: an attention flag ("you're missing content above"), styled
/// like the other pending/attention items.
fn hint_pieces(app: &App) -> Vec<Piece> {
    if app.autoscroll {
        Vec::new()
    } else {
        vec![
            (
                "▲ more above".to_string(),
                Style::default().fg(Color::Yellow),
            ),
            sep(),
        ]
    }
}

fn pieces_width(pieces: &[Piece]) -> usize {
    pieces
        .iter()
        .map(|(text, _)| UnicodeWidthStr::width(text.as_str()))
        .sum()
}

fn to_line(pieces: Vec<Piece>) -> Line<'static> {
    Line::from(
        pieces
            .into_iter()
            .map(|(text, style)| Span::styled(text, style))
            .collect::<Vec<_>>(),
    )
}

/// Truncate a styled run sequence to `width` cells, ellipsizing the piece
/// that crosses the edge.
fn truncate_pieces(pieces: Vec<Piece>, width: usize) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for (text, style) in pieces {
        if used >= width {
            break;
        }
        let w = UnicodeWidthStr::width(text.as_str());
        if w <= width - used {
            used += w;
            out.push((text, style));
        } else {
            out.push((truncate_display(&text, (width - used) as u16), style));
            break;
        }
    }
    out
}

/// Connection badge pinned to the right edge. On a remote box knowing
/// that beats any left-side detail, so it survives narrowing at the
/// left's expense. The left candidates shed width in order of how static
/// they are: first the cwd (fixed for the whole session; every live number
/// stays), then branch/model, then only model + spend.
pub(super) fn footer_line(app: &App, width: u16) -> Line<'static> {
    let width = width as usize;
    let hint = hint_pieces(app);
    let conn = conn_piece(app);
    let conn_w = pieces_width(std::slice::from_ref(&conn));
    let candidates = [
        status_pieces(app, true),
        status_pieces(app, false),
        compact_pieces(app),
        bare_pieces(app),
    ];
    let mut left_only: Option<Vec<Piece>> = None;
    for candidate in candidates {
        let mut left = hint.clone();
        left.extend(candidate);
        let lw = pieces_width(&left);
        if lw + 1 + conn_w <= width {
            left.push((" ".repeat(width - lw - conn_w), Style::default()));
            left.push(conn);
            return to_line(left);
        }
        if left_only.is_none() && lw <= width {
            left_only = Some(left);
        }
    }
    if let Some(left) = left_only {
        return to_line(left);
    }
    let mut left = hint;
    left.push(quiet(app.config.model.clone()));
    to_line(truncate_pieces(left, width))
}

pub(super) fn footer_text(app: &App, width: u16) -> String {
    footer_line(app, width)
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}
