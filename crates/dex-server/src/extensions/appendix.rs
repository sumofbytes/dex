//! System-prompt appendix and extension tool-name helpers.

// ---------------------------------------------------------------------------
// System-prompt appendix (load-time, sync read from the prompt builder)
// ---------------------------------------------------------------------------

pub(crate) static PROMPT_APPENDIX: std::sync::Mutex<Vec<(String, String)>> =
    std::sync::Mutex::new(Vec::new());

/// `dex.prompt.append(text)`: one entry per extension, keyed by id.
/// Composition sorts by id (`prompt_appendix`), so push order never leaks
/// into the system prefix.
pub fn push_prompt_appendix(ext_id: &str, text: String) {
    let mut guard = PROMPT_APPENDIX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match guard.iter_mut().find(|(id, _)| id == ext_id) {
        Some(entry) => entry.1.push_str(&text),
        None => guard.push((ext_id.to_string(), text)),
    }
}

/// Drop one extension's appendix entry (unload on reload).
pub(crate) fn remove_prompt_appendix(ext_id: &str) {
    PROMPT_APPENDIX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|(id, _)| id != ext_id);
}

/// The composed appendix for `system_prompt()`: each extension's text sorted
/// by extension id, separated by blank lines. Sorted (not load order) so the
/// system prefix is byte-identical across reload orders — prompt-cache
/// stability: any reorder would invalidate the cached system prefix.
pub fn prompt_appendix() -> String {
    let guard = PROMPT_APPENDIX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut entries: Vec<(&str, &str)> = guard
        .iter()
        .map(|(id, text)| (id.as_str(), text.as_str()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
        .into_iter()
        .map(|(_, text)| text.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(
            "

",
        )
}
pub(crate) fn full_tool_name(ext: &str, tool: &str) -> String {
    format!("ext__{ext}__{tool}")
}

/// True for extension tools under either the canonical `ext__` prefix or
/// the pre-rename `lua__` alias (single place that knows both, so dispatch,
/// gates, and preload stay in sync).
pub fn is_extension_tool(name: &str) -> bool {
    name.starts_with("ext__") || name.starts_with("lua__")
}

/// Map a legacy `lua__<ext>__<tool>` name to its canonical `ext__` form.
/// Canonical names (and non-extension names) pass through untouched.
pub fn normalize_tool_name(name: &str) -> String {
    match split_ext_name(name) {
        Some((ext, tool)) => full_tool_name(ext, tool),
        None => name.to_string(),
    }
}

/// Split `ext__<ext>__<tool>`; `None` for anything else (built-ins, MCP).
/// The pre-rename `lua__` prefix still splits (deprecated alias) with a
/// one-time pointer at the replacement, so old sessions and one-liners keep
/// dispatching instead of hitting `unknown tool`.
pub fn split_ext_name(name: &str) -> Option<(&str, &str)> {
    let rest = match name.strip_prefix("ext__") {
        Some(rest) => rest,
        None => {
            let rest = name.strip_prefix("lua__")?;
            crate::runtime::notice::warn_once(
                "ext.lua-prefix",
                "tool prefix `lua__` is deprecated, use `ext__` instead",
            );
            rest
        }
    };
    let (ext, tool) = rest.split_once("__")?;
    if ext.is_empty() || tool.is_empty() || tool.contains("__") {
        return None;
    }
    Some((ext, tool))
}

/// Schema cap: extension tools share the per-request schema budget with MCP
/// (plan §9). `DEX_MAX_EXTENSION_TOOLS` overrides for tests.
pub(crate) fn max_extension_tools() -> usize {
    std::env::var("DEX_MAX_EXTENSION_TOOLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64)
}

/// One-time diagnostic for the schema cap. Dropped extension tools are
/// silently absent from the model's view and answer "unknown tool" otherwise;
/// unlike MCP there is no `/mcp`-style panel count to surface them.
pub(crate) fn warn_hidden_tools(dropped: usize, max: usize) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "dex: [extensions] {dropped} tools hidden by the schema cap ({max} max — raise DEX_MAX_EXTENSION_TOOLS)"
        );
    }
}
