//! `/mcp` panel rendering for the slash surface. The one-line status lives
//! with its owner (`mcp::status_line`).

use super::clip_chars;

/// Ephemeral MCP status line for the compaction budget (`agent::loop`
/// counts it via the `ephemerals` preamble without storing it in the
/// transcript). `None` when no servers are configured so the budget is
/// unaffected. The schema already tells the model which tools exist; this
/// names what it is *not* seeing — down servers and schema-cap drops.
/// Multi-line `/mcp` panel: one header with totals,
/// then per server a `✓ name — N tools` line (with each tool + its one-line
/// description indented beneath) or a `✗ name — down: <reason>` line.
/// `tools` is the cached schema slice; only `mcp__<server>__*` entries
/// (plus the synthetic `_read_resource` reader) belong to a server.
/// `truncated` is the schema-cap drop count (`GET /api/mcp` carries it for
/// remote clients whose own process counter is always zero). Pure function
/// over snapshots so both TUIs share the render.
pub fn render_mcp_panel(
    statuses: &[crate::protocol::ServerStatus],
    tools: &[crate::protocol::ToolDefinition],
    truncated: usize,
) -> Vec<String> {
    if statuses.is_empty() {
        return vec!["no MCP servers configured.".to_string()];
    }
    let up = statuses.iter().filter(|s| s.state.as_str() == "up").count();
    let total_tools: usize = statuses.iter().map(|s| s.tools).sum();
    let mut lines = vec![format!(
        "MCP servers ({} connected, {} down · {} tools):",
        up,
        statuses.len() - up,
        total_tools
    )];
    for server in statuses {
        if server.state.as_str() == "up" {
            lines.push(format!(
                "✓ {} — {} tool{}",
                server.name,
                server.tools,
                if server.tools == 1 { "" } else { "s" }
            ));
            let mut names: Vec<(&str, &str)> = tools
                .iter()
                .filter_map(|t| {
                    short_mcp_tool(&server.name, &t.function.name)
                        .map(|short| (short, t.function.description.as_str()))
                })
                .collect();
            names.sort();
            for (short, desc) in names {
                let one_line = desc
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                lines.push(format!("  · {short} — {}", clip_chars(&one_line, 100)));
            }
        } else {
            let reason = server
                .error
                .as_deref()
                .map(|e| {
                    e.lines()
                        .next()
                        .unwrap_or("unknown error")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_else(|| "not connected".to_string());
            lines.push(format!(
                "✗ {} — down: {}",
                server.name,
                clip_chars(&reason, 160)
            ));
        }
    }
    if truncated > 0 {
        lines.push(format!(
            "… and {truncated} more tool{} hidden by the schema cap (DEX_MCP_MAX_TOOLS).",
            if truncated == 1 { "" } else { "s" }
        ));
    }
    lines
}

/// Strip the `mcp__<server>__` prefix (or the single-underscore synthetic
/// `_read_resource` reader) down to the bare tool name. `None` when the
/// tool belongs to a different server.
fn short_mcp_tool<'a>(server: &'a str, full: &'a str) -> Option<&'a str> {
    let prefix = format!("mcp__{server}__");
    if let Some(short) = full.strip_prefix(&prefix) {
        return Some(short);
    }
    full.strip_prefix(&format!("mcp__{server}_"))
}
