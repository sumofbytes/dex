//! `dex usage <session-id|path>` — token usage per model call, plotted from a
//! session's event journal (`<id>.events.jsonl`). The journal holds one
//! `usage` event per LLM call and `turn_complete`/`turn_failed` markers at
//! turn boundaries, so a turn is the usage events between two markers. Steps
//! run left→right on the x-axis; each call is a stacked bar — height is the
//! context (prompt tokens), solid bottom = cached, shaded top = fresh. On a
//! TTY the two segments are colored (green cached, yellow fresh); piped
//! output is plain.

use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, IsTerminal};
use std::path::{Path, PathBuf};

use crate::core::console::{AGENT_COLOR, RESET, TOOL_INPUT_COLOR};
use crate::session::Session;

/// Horizontal chart: 15 rows tall, one column per call (long sessions are
/// bucketed into ≤100 columns).
const CHART_ROWS: usize = 15;
const CHART_COLS: usize = 100;

/// Usage of one LLM call.
#[derive(Debug, PartialEq)]
struct CallUsage {
    /// 1-based call number across the whole session.
    n: usize,
    /// Prompt tokens (the context sent for this call).
    prompt: u64,
    /// Provider-reported cache-hit subset of `prompt`.
    cached: u64,
    /// Completion tokens.
    output: u64,
    /// Billed cost.
    cost: f64,
}

impl CallUsage {
    /// Fresh (uncached) input this call actually processed.
    fn fresh(&self) -> u64 {
        self.prompt.saturating_sub(self.cached)
    }
}

/// One turn: its calls plus how it ended.
#[derive(Debug, PartialEq)]
struct TurnUsage {
    /// 1-based turn number, in journal order.
    n: usize,
    /// How the turn ended: `complete`, `failed`, or `interrupted` (usage
    /// events with no closing marker — a crashed turn).
    end: &'static str,
    calls: Vec<CallUsage>,
}

impl TurnUsage {
    fn empty(n: usize) -> Self {
        Self {
            n,
            end: "complete",
            calls: Vec::new(),
        }
    }
}

/// Read `<id>.events.jsonl` and bucket its `usage` events into turns. The
/// journal carries no `turn_start` (only the closing marker), so the first
/// bucket opens implicitly at the first usage event; a trailing bucket with
/// no marker is an interrupted turn.
fn turns_from_events(path: &Path) -> std::io::Result<Vec<TurnUsage>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut turns = Vec::new();
    let mut current: Option<TurnUsage> = None;
    let mut call_n: usize = 0;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            // Never treat EINTR as EOF (matches `Session::load_events`).
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
            Ok(_) => {}
        }
        let Ok(value) = serde_json::from_str::<Value>(line.trim_end()) else {
            continue; // torn line, same tolerance as the replay path
        };
        let payload = value.get("payload");
        let kind = payload
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        match kind {
            "usage" => {
                let data = payload.and_then(|p| p.get("data")).unwrap_or(&Value::Null);
                call_n += 1;
                let call = CallUsage {
                    n: call_n,
                    prompt: u64_field(data, "tokens"),
                    cached: u64_field(data, "cached"),
                    output: u64_field(data, "output"),
                    cost: data.get("cost").and_then(Value::as_f64).unwrap_or_default(),
                };
                current
                    .get_or_insert_with(|| TurnUsage::empty(turns.len() + 1))
                    .calls
                    .push(call);
            }
            "turn_complete" | "turn_failed" => {
                if let Some(mut turn) = current.take() {
                    turn.end = if kind == "turn_complete" {
                        "complete"
                    } else {
                        "failed"
                    };
                    turns.push(turn);
                }
            }
            _ => {}
        }
    }
    // An open bucket at EOF ran out of journal mid-turn (daemon killed, no
    // marker ever written) — keep it, marked, so nothing is silently lost.
    if let Some(mut turn) = current.take() {
        turn.end = "interrupted";
        turns.push(turn);
    }
    Ok(turns)
}

fn u64_field(data: &Value, key: &str) -> u64 {
    data.get(key).and_then(Value::as_u64).unwrap_or_default()
}

/// Render the horizontal chart (x = model calls, y = context tokens with an
/// overlaid cache% line), then the Σ totals line.
fn render(turns: &[TurnUsage]) -> String {
    render_with_color(turns, std::io::stdout().is_terminal())
}

fn render_with_color(turns: &[TurnUsage], color: bool) -> String {
    let all: Vec<&CallUsage> = turns.iter().flat_map(|t| &t.calls).collect();
    let mut out = String::new();
    if !all.is_empty() {
        out.push_str(&chart(&all, turns, color));
    }
    out.push_str(&totals(turns, &all, color));
    out
}

/// One row's cells with the series colors applied (plain when `color` is
/// false): green for cached (`█`), yellow for fresh input (`▒`).
fn paint_row(cells: &[char], color: bool) -> String {
    let mut out = String::with_capacity(cells.len() + 16);
    let mut class = 0u8; // 0 plain, 1 cached, 2 fresh
    for &cell in cells {
        let next = match cell {
            '█' => 1,
            '▒' => 2,
            _ => 0,
        };
        if next != class {
            match (color, next) {
                (true, 1) => out.push_str(AGENT_COLOR),
                (true, 2) => out.push_str(TOOL_INPUT_COLOR),
                (true, _) => out.push_str(RESET),
                _ => {}
            }
            class = next;
        }
        out.push(cell);
    }
    if color && class != 0 {
        out.push_str(RESET);
    }
    out
}

/// Compact token counts: `229k`, `2.0k`, `1.2M`, plain below 1k.
fn fmt_k(v: u64) -> String {
    if v >= 1_000_000 {
        format!("{:.1}M", v as f64 / 1e6)
    } else if v >= 10_000 {
        format!("{}k", v.div_ceil(1000))
    } else if v >= 1_000 {
        format!("{:.1}k", v as f64 / 1e3)
    } else {
        v.to_string()
    }
}

/// The chart: stacked bars, one column per call. Bar height is the call's
/// context (prompt tokens) on a single token scale; the solid bottom share is
/// cached, the shaded top is fresh — cache coverage is the fill fraction, no
/// second axis needed. An x-axis of call numbers and a `turn ends` line mark
/// where each turn's last call sits.
fn chart(all: &[&CallUsage], turns: &[TurnUsage], color: bool) -> String {
    let max_ctx = all.iter().map(|c| c.prompt).max().unwrap_or(0).max(1);
    // One column per call; long sessions collapse into ≤ CHART_COLS buckets,
    // each showing its peak values.
    let per = all.len().div_ceil(CHART_COLS).max(1);
    let cols = all.len().div_ceil(per);
    let buckets: Vec<(u64, u64, usize)> = (0..cols)
        .map(|col| {
            let i = col * per;
            let slice = &all[i..(i + per).min(all.len())];
            (
                slice.iter().map(|c| c.prompt).max().unwrap_or(0),
                slice.iter().map(|c| c.cached).max().unwrap_or(0),
                slice[0].n,
            )
        })
        .collect();

    // grid[row][col], row 0 printed first (top). A bar rises from the bottom
    // axis row: solid █ up to the cached share, shaded ▒ up to the full
    // context height.
    let mut grid = vec![vec![' '; cols]; CHART_ROWS];
    for (col, (tok, cached, _)) in buckets.iter().enumerate() {
        if *tok == 0 {
            continue;
        }
        let total = (((*tok as f64 / max_ctx as f64) * CHART_ROWS as f64).round() as usize)
            .clamp(1, CHART_ROWS);
        let solid = (((*cached as f64 / *tok as f64) * total as f64).round() as usize).min(total);
        for (r, row) in grid.iter_mut().enumerate() {
            let from_bottom = CHART_ROWS - 1 - r;
            if from_bottom < solid {
                row[col] = '█';
            } else if from_bottom < total {
                row[col] = '▒';
            }
        }
    }

    // y-axis labels at 0, ¼, ½, ¾, and max of the token scale.
    let label_rows = [
        0,
        CHART_ROWS / 4,
        CHART_ROWS / 2,
        CHART_ROWS * 3 / 4,
        CHART_ROWS - 1,
    ];
    let labels: Vec<String> = label_rows
        .iter()
        .map(|&r| {
            fmt_k(
                (max_ctx as f64 * (CHART_ROWS - 1 - r) as f64 / (CHART_ROWS - 1) as f64).round()
                    as u64,
            )
        })
        .collect();
    let y_w = labels.iter().map(String::len).max().unwrap_or(0);
    let mut out = String::new();
    let mut li = 0;
    for (r, row_cells) in grid.iter().enumerate() {
        let (label, axis) = if r == label_rows[li] {
            let label = labels[li].clone();
            li = (li + 1).min(labels.len() - 1);
            (label, '┤')
        } else {
            (String::new(), '│')
        };
        out.push_str(&format!(
            "{:>y_w$} {axis}{}\n",
            label,
            paint_row(row_cells, color),
            y_w = y_w
        ));
    }

    // x-axis with ┼ at ~6 evenly spaced ticks, call numbers under them.
    let ticks: Vec<usize> = if cols <= 6 {
        (0..cols).collect()
    } else {
        (0..6).map(|k| k * (cols - 1) / 5).collect()
    };
    let mut axis = format!("{:>y_w$} └", "", y_w = y_w);
    for col in 0..cols {
        axis.push(if ticks.contains(&col) { '┼' } else { '─' });
    }
    out.push_str(&axis);
    out.push('\n');
    let mut x_labels = String::new();
    let mut cursor = 0;
    for &t in &ticks {
        let text = buckets[t].2.to_string();
        for _ in cursor..t {
            x_labels.push(' ');
        }
        x_labels.push_str(&text);
        cursor = t + text.len();
    }
    out.push_str(x_labels.trim_end());
    out.push('\n');

    // Where each turn ends, with `*` for failed and `^` for interrupted.
    let ends: Vec<String> = turns
        .iter()
        .map(|turn| {
            let last = turn.calls.last().map(|c| c.n).unwrap_or(0);
            let marker = match turn.end {
                "failed" => "*",
                "interrupted" => "^",
                _ => "",
            };
            format!("{}→{}{}", turn.n, last, marker)
        })
        .collect();
    out.push_str(&format!("turn ends: {}\n", ends.join(" ")));
    out
}

/// Legend + Σ totals line.
fn totals(turns: &[TurnUsage], all: &[&CallUsage], color: bool) -> String {
    let fresh: u64 = all.iter().map(|c| c.fresh()).sum();
    let output: u64 = all.iter().map(|c| c.output).sum();
    let cost: f64 = all.iter().map(|c| c.cost).sum();
    let cached: u64 = all.iter().map(|c| c.cached).sum();
    let prompt: u64 = all.iter().map(|c| c.prompt).sum();
    let pct = cached.saturating_mul(100).checked_div(prompt).unwrap_or(0);
    let cache_pct = if pct > 0 {
        format!(" ({pct}%)")
    } else {
        String::new()
    };
    let legend = if color {
        format!("{AGENT_COLOR}█{RESET} cached · {TOOL_INPUT_COLOR}▒{RESET} fresh — bar height = prompt tokens")
    } else {
        "█ cached · ▒ fresh — bar height = prompt tokens".to_string()
    };
    format!(
        "{legend}\nΣ {} turns · {} calls: in {} · out {} · {} · cached {}/{}{}\n",
        turns.len(),
        all.len(),
        with_commas(fresh),
        with_commas(output),
        fmt_cost(cost),
        with_commas(cached),
        with_commas(prompt),
        cache_pct
    )
}

/// Thousands separators, e.g. `14480` → `14,480`.
fn with_commas(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn fmt_cost(cost: f64) -> String {
    if cost == 0.0 {
        "$0".to_string()
    } else {
        format!("${cost:.4}")
    }
}

/// Resolve the `dex usage <arg>` target to a session's event journal. `arg`
/// may be a path (session JSONL or events journal) or a session id, searched
/// across every workspace directory via `Session::list_all`.
fn events_path(arg: &str) -> Result<PathBuf, String> {
    let invalid = arg == "__invalid__";
    if invalid {
        return Err("usage: dex usage <session-id|path>".to_string());
    }
    let path = Path::new(arg);
    if path.exists() {
        if path.is_dir() {
            return Err(format!("{arg} is a directory"));
        }
        // `dex usage <id>.jsonl` → the journal next to it; an explicit
        // `.events.jsonl` path is used as-is (`with_extension` would double it).
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && !path.to_string_lossy().ends_with(".events.jsonl")
        {
            return Ok(path.with_extension("events.jsonl"));
        }
        return Ok(path.to_path_buf());
    }
    let sessions = Session::list_all().map_err(|e| format!("cannot list sessions: {e}"))?;
    sessions
        .iter()
        .find(|(_, header)| header.id() == arg)
        .map(|(path, _)| path.with_extension("events.jsonl"))
        .ok_or_else(|| format!("no session '{arg}' found (not a path or session id)"))
}

/// CLI entry: resolve, parse, print.
pub(crate) fn run(arg: &str) -> Result<(), String> {
    let path = events_path(arg)?;
    if !path.exists() {
        return Err(format!("no event journal at {}", path.display()));
    }
    let turns = turns_from_events(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let calls: usize = turns.iter().map(|t| t.calls.len()).sum();
    if calls == 0 {
        println!("{} — no model-call usage recorded", path.display());
        return Ok(());
    }
    println!(
        "{} — token usage per model call ({} turns, {} calls)",
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session"),
        turns.len(),
        calls
    );
    print!("{}", render(&turns));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write fixture journal lines to a unique temp file and return its path.
    fn fixture(lines: &[&str]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dex-usage-test-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &path,
            lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
        .unwrap();
        path
    }

    #[test]
    fn parses_calls_and_buckets_them_into_turns() {
        let path = fixture(&[
            r#"{"seq":1,"ts":"2026-09-12T06:21:59+00:00","payload":{"type":"usage","data":{"tokens":1000,"cached":400,"cost":0.001,"output":50,"gen_ms":10}}}"#,
            r#"{"seq":2,"ts":"2026-09-12T06:22:10+00:00","payload":{"type":"usage","data":{"tokens":1200,"cached":600,"cost":0.002,"output":30,"gen_ms":5}}}"#,
            r#"{"seq":3,"ts":"2026-09-12T06:22:11+00:00","payload":{"type":"turn_complete","data":{"usage":1200,"cached":600}}}"#,
            r#"{"seq":4,"ts":"2026-09-12T06:23:00+00:00","payload":{"type":"usage","data":{"tokens":1500,"cached":0,"cost":0.003,"output":10,"gen_ms":5}}}"#,
            r#"{"seq":5,"ts":"2026-09-12T06:23:01+00:00","payload":{"type":"turn_failed","data":{}}}"#,
        ]);
        let turns = turns_from_events(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(turns.len(), 2);
        // Turn 1: two calls with their per-call usage; the TurnComplete
        // payload is not double-counted as a call.
        assert_eq!(turns[0].end, "complete");
        assert_eq!(turns[0].calls.len(), 2);
        assert_eq!(turns[0].calls[0].prompt, 1000);
        assert_eq!(turns[0].calls[0].cached, 400);
        assert_eq!(turns[0].calls[0].fresh(), 600); // (1000-400)
        assert_eq!(turns[0].calls[1].n, 2); // global call numbering
                                            // Turn 2 failed after its first call.
        assert_eq!(turns[1].end, "failed");
        assert_eq!(turns[1].calls.len(), 1);
        assert_eq!(turns[1].calls[0].n, 3);
        assert_eq!(turns[1].calls[0].prompt, 1500);
    }

    #[test]
    fn trailing_usage_without_marker_is_interrupted() {
        let path = fixture(&[
            r#"{"seq":1,"ts":"2026-09-12T06:21:59+00:00","payload":{"type":"usage","data":{"tokens":900,"cached":0,"cost":0.001,"output":5,"gen_ms":1}}}"#,
        ]);
        let turns = turns_from_events(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(turns[0].end, "interrupted");
        assert_eq!(turns[0].calls.len(), 1);
    }

    #[test]
    fn render_draws_horizontal_chart_with_cache_line() {
        let turns = vec![TurnUsage {
            n: 1,
            end: "complete",
            calls: vec![
                CallUsage {
                    n: 1,
                    prompt: 1000,
                    cached: 800,
                    output: 50,
                    cost: 0.001,
                },
                CallUsage {
                    n: 2,
                    prompt: 2000,
                    cached: 1000,
                    output: 30,
                    cost: 0.002,
                },
            ],
        }];
        let text = render(&turns);
        let rows: Vec<&str> = text.lines().collect();
        // 15 chart rows, x-axis, x labels, turn-ends line, legend, Σ line.
        assert_eq!(rows.len(), 20);
        // y-axis: top label is the max context, bottom is 0.
        assert!(rows[0].contains("2.0k"));
        assert!(rows[14].trim_start().starts_with('0'));
        // Stacked bars: solid cached bottom + shaded fresh top. Call 2
        // (2,000 prompt, 50% cached) fills the full height: its bottom half
        // is solid, top half shaded; call 1 is half as tall.
        assert!(rows[14].contains("██"));
        assert!(rows[2].contains("▒"));
        assert!(rows[10].contains("█"));
        // The old dual-series markers are gone (the axis spine `│` remains).
        assert!(!text.contains('●') && !text.contains('○'));
        // x labels start at call 1.
        assert!(rows[16].trim_start().starts_with('1'));
        assert!(text.contains("turn ends: 1\u{2192}2"));
        assert!(text.contains("█ cached · ▒ fresh — bar height = prompt tokens"));
        assert!(text.contains(
            "\u{03a3} 1 turns \u{00b7} 2 calls: in 1,200 \u{00b7} out 80 \u{00b7} $0.0030 \u{00b7} cached 1,800/3,000 (60%)"
        ));
    }

    #[test]
    fn paint_row_colors_series_and_keeps_glyphs() {
        let cells: Vec<char> = "█▒".chars().collect();
        // Without color: exactly the raw glyphs.
        assert_eq!(paint_row(&cells, false), "█▒");
        // With color: green run, then yellow run; the glyphs themselves are
        // unchanged (coloring is decoration only).
        let colored = paint_row(&cells, true);
        assert!(colored.starts_with(AGENT_COLOR));
        assert!(colored.contains("▒"));
        assert!(colored.ends_with(RESET));
        // Glyphs preserved (the remaining non-escape text is exactly the
        // series markers).
        assert_eq!(
            colored.matches('█').count() + colored.matches('▒').count(),
            2
        );
    }

    #[test]
    fn long_sessions_bucket_into_chart_cols() {
        // 250 calls of growing context must collapse into <=100 columns.
        let calls: Vec<CallUsage> = (1..=250)
            .map(|n| CallUsage {
                n,
                prompt: n as u64 * 100,
                cached: n as u64 * 50,
                output: 1,
                cost: 0.0,
            })
            .collect();
        let turns = vec![TurnUsage {
            n: 1,
            end: "complete",
            calls,
        }];
        let text = render(&turns);
        let width = text.lines().map(|l| l.chars().count()).max().unwrap();
        assert!(width < 120, "chart too wide: {width}");
        assert!(text.contains("turn ends: 1\u{2192}250"));
    }

    #[test]
    fn empty_journal_renders_totals_only() {
        let path = fixture(&[
            r#"{"seq":1,"ts":"2026-09-12T06:21:59+00:00","payload":{"type":"turn_complete","data":{}}}"#,
        ]);
        let turns = turns_from_events(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(turns.is_empty());
        // No chart, just the legend and \u{03a3} line.
        let text = render(&[TurnUsage::empty(1)]);
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("\u{03a3} 1 turns \u{00b7} 0 calls: in 0 \u{00b7} out 0 \u{00b7} $0 \u{00b7} cached 0/0"));
    }
}
