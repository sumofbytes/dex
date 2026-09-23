use super::style::fg;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::App;
use crate::agent::tokens::format_tokens;
use crate::render::theme;

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
    fg(theme::muted_fg())
}

fn sep() -> Piece {
    (" · ".to_string(), quiet_style())
}

fn quiet(text: impl Into<String>) -> Piece {
    (text.into(), quiet_style())
}

/// The agent-mode chip: a safety indicator (what the model is allowed to do),
/// so it survives every narrowing tier. Derived from the permission the
/// session will send, so it can never disagree with the gate.
fn mode_piece(app: &App) -> Piece {
    use crate::protocol::AgentMode;
    let mode = AgentMode::from_permission(app.config.permission);
    let color = match mode {
        AgentMode::Plan => theme::accent_fg(),
        AgentMode::Manual => theme::warn_fg(),
        AgentMode::Auto => theme::ok_fg(),
    };
    (mode.label().to_string(), fg(color))
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
        fg(theme::failure_fg())
    } else if tokens.saturating_mul(4) >= threshold.saturating_mul(3) {
        fg(theme::warn_fg())
    } else {
        quiet_style()
    }
}

/// Session cost: `$X.XXX`, catalog-priced when possible
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
            fg(theme::surface_fg()),
        ))
    } else {
        None
    }
}

/// Model identity as one run: `provider/model`. Model ids that already
/// carry a provider prefix (OpenRouter-style `openai/gpt-5.2`) render
/// as-is instead of doubling it. An empty provider (daemon resolved no
/// config yet) renders `unconfigured`, never `/unknown`.
fn model_label(app: &App) -> String {
    if app.config.provider.name().is_empty() {
        return "unconfigured".to_string();
    }
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
        pieces.push((branch.clone(), fg(theme::success_fg())));
        if app.git_dirty {
            pieces.push(("*".to_string(), fg(theme::warn_fg())));
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
/// written as tight as it stays readable — `ctx 12k/128k 9%`, `66% cached`,
/// `↑42k ↓1.2k` (cumulative in/out), `123 tok/s`. Redundant detail is
/// dropped, never information: the cached absolute is the ctx number times
/// its %, and a trailing `.0` on scaled tokens says nothing.
/// Context size for the status bar: provider-reported prompt tokens once
/// the first `Usage` event lands, else a cached transcript estimate (perf
/// doc §29). The estimate is served from `App::status_tokens_cache` while
/// the history fingerprint is unchanged, so per-frame callers (keystrokes,
/// SSE batches, busy ticks) pay one O(messages) sample per history change
/// instead of a full char walk per frame. Display-only — a stale read would
/// merely lag the ctx % by a frame.
pub(super) fn status_tokens(app: &App) -> u64 {
    if let Some(usage) = app.tool_state.last_usage {
        return usage;
    }
    let key = (app.messages.len(), history_fingerprint(&app.messages));
    let (n, f, tokens) = app.status_tokens_cache.get();
    if (n, f) == key {
        return tokens;
    }
    let tokens = crate::agent::compaction::estimate_tokens(&app.messages);
    app.status_tokens_cache.set((key.0, key.1, tokens));
    tokens
}

/// Cheap history identity: FNV-1a over per-message (role, byte lengths,
/// head/tail samples). A same-length middle edit or tail replace changes
/// sampled bytes and misses — the old (len, tail-len) key collided on
/// those. O(messages) pointer/len reads, never a full char walk, so a hit
/// still avoids the estimator's walk.
fn history_fingerprint(messages: &[crate::protocol::ChatMessage]) -> u64 {
    let mut h = 14695981039346656037u64;
    let mut mix = |b: u8| {
        h ^= u64::from(b);
        h = h.wrapping_mul(1099511628211);
    };
    for m in messages {
        for b in m.role.as_str().as_bytes() {
            mix(*b);
        }
        sample_text(&mut mix, m.content.as_deref().unwrap_or_default());
        sample_text(&mut mix, m.reasoning_content.as_deref().unwrap_or_default());
        mix((m.reasoning_items.as_ref().map_or(0, Vec::len) & 0xff) as u8);
    }
    h
}

/// Length + first/last 32 bytes: catches same-length swaps anywhere except
/// a mid-body change with identical endpoints — vanishingly rare, and the
/// miss only lags a display number by a frame.
fn sample_text(mix: &mut impl FnMut(u8), text: &str) {
    let bytes = text.as_bytes();
    for b in (bytes.len() as u64).to_le_bytes() {
        mix(b);
    }
    for b in bytes.iter().take(32).chain(bytes.iter().rev().take(32)) {
        mix(*b);
    }
}

pub(super) fn status_pieces(app: &App, with_cwd: bool) -> Vec<Piece> {
    // Transient notice (copy confirmation) takes over the line until it
    // expires: unmissable feedback beats the quiet facts for two seconds.
    if let Some((text, at)) = &app.notice {
        if at.elapsed() < super::NOTICE_LIFETIME {
            return vec![(text.clone(), fg(theme::ok_fg()))];
        }
    }
    let tokens = status_tokens(app);
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
        pieces.push((compact_path(&app.cwd), fg(theme::accent_fg())));
    }
    // The branch separator is part of the branch block: emitting it
    // unconditionally doubled up with the one after an empty branch
    // (`path ·  · model` in any non-repo directory).
    let branch = branch_pieces(app);
    if !branch.is_empty() {
        push_sep(&mut pieces);
        pieces.extend(branch);
    }
    push_sep(&mut pieces);
    pieces.push(quiet(model_label(app)));
    pieces.push(sep());
    pieces.push(mode_piece(app));
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
    // at a fraction of full input price), collapsed to a % of that prompt —
    // the glanceable "is caching working" readout; the absolute count is
    // the ctx number times this %. The subset is clamped to the prompt
    // (some third-party OpenAI-compatible endpoints report nonconforming
    // usage) and, once a provider reports it, omitted only at zero. The
    // absolute is the fallback when the prompt size is unknown or the hit
    // is below one percent.
    if let Some(cached) = app.tool_state.last_cached {
        if cached > 0 {
            pieces.push(sep());
            let cached_text = match app.tool_state.last_usage {
                Some(prompt) if prompt > 0 => {
                    let cached = cached.min(prompt);
                    match cached.saturating_mul(100) / prompt {
                        0 => format!("{} cached", format_tokens(cached)),
                        pct => format!("{pct}% cached"),
                    }
                }
                _ => format!("{} cached", format_tokens(cached)),
            };
            pieces.push(quiet(cached_text));
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
    // Session cost: `$X.XXX`, catalog-priced when possible
    // else `DEX_COST_PER_1K` fallback. Shown once any prompt has been billed.
    push_cost(&mut pieces, app);
    pieces
}

/// Live child agents (V1b typed events): `agents: explorer·bash, tester`.
/// The typed `AgentSpawned/Progress/Completed` events keep this current
/// without parsing the V1a transcript lines.
fn agents_pieces(app: &App) -> Vec<Piece> {
    if app.agents.is_empty() {
        return Vec::new();
    }
    let text = app
        .agents
        .iter()
        .map(|agent| match &agent.tool {
            Some(tool) => format!("{}·{}", agent.name, tool),
            None => agent.name.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    vec![sep(), (format!("agents: {text}"), fg(theme::ok_fg()))]
}

/// Test shim: the status row as plain text (string asserts in `render/tests`).
#[cfg(test)]
pub(super) fn ui_status(app: &App) -> String {
    status_pieces(app, true)
        .into_iter()
        .map(|(text, _)| text)
        .collect::<Vec<_>>()
        .concat()
}

fn compact_pieces(app: &App) -> Vec<Piece> {
    // Narrow tier: cwd + branch + model + spend. Branch and cost share
    // helpers with the full line so the tiers cannot drift. The model stays
    // canonical `provider/model` like the full line — width tiers shed the
    // cwd/branch first, never the provider.
    let mut pieces = vec![(compact_path(&app.cwd), fg(theme::accent_fg()))];
    let branch = branch_pieces(app);
    if !branch.is_empty() {
        push_sep(&mut pieces);
        pieces.extend(branch);
    }
    push_sep(&mut pieces);
    pieces.push(quiet(model_label(app)));
    pieces.push(sep());
    pieces.push(mode_piece(app));
    pieces.extend(agents_pieces(app));
    push_cost(&mut pieces, app);
    pieces
}

fn bare_pieces(app: &App) -> Vec<Piece> {
    // Narrowest tier: brevity beats precision — the bare id keeps the
    // connection badge on screen when even `provider/model` won't fit.
    let mut pieces = vec![mode_piece(app)];
    push_sep(&mut pieces);
    pieces.push(quiet(app.config.model.clone()));
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
        fg(theme::accent_fg())
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
        vec![("▲ more above".to_string(), fg(theme::warn_fg())), sep()]
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
    // Tier candidates build lazily, widest first: the full line almost
    // always fits, so on a wide terminal the narrower tiers (and their
    // transcript walks) never build at all.
    let mut left_only: Option<Vec<Piece>> = None;
    for tier in 0..4 {
        let candidate = match tier {
            0 => status_pieces(app, true),
            1 => status_pieces(app, false),
            2 => compact_pieces(app),
            _ => bare_pieces(app),
        };
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

/// Test shim: the footer row as plain text (string asserts in `render/tests`).
#[cfg(test)]
pub(super) fn footer_text(app: &App, width: u16) -> String {
    footer_line(app, width)
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}
