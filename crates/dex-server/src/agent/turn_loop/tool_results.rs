//! Dex adapter for tool-result freshness and host-owned result state.

use crate::agent::state::{cache_fingerprint, ToolState};
use crate::tools::ToolOutcome;

pub(super) use dex_coding_agent::NormalizedToolResult as ToolResult;

/// Add Dex's workspace freshness fingerprint, then delegate cache and
/// repeated-call policy to `dex-coding-agent` under the harness's
/// [`ResultPolicy`](dex_coding_agent::ResultPolicy).
pub(super) fn prepare_tool_result(
    state: &mut ToolState,
    last_tools: &mut Vec<String>,
    turn_cwd: &str,
    name: &str,
    input: &str,
    outcome: ToolOutcome,
    result_policy: &dex_coding_agent::ResultPolicy,
) -> ToolResult {
    let key = format!(
        "{}:{}:{}{}",
        turn_cwd,
        name,
        input,
        cache_fingerprint(name, input)
    );
    let result = dex_coding_agent::normalize_tool_result_with(
        result_policy,
        &mut state.cache,
        last_tools,
        key,
        name,
        outcome,
    );
    state.dirty |= result.cache_changed;
    result
}
