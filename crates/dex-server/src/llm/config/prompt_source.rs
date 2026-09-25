use super::load_config_file;
use super::load_config_str;
use super::warn_once;

use std::env;

/// Read a system-prompt file for the env/file layers: a miss warns once and
/// falls through to the next layer (an explicit `--system-prompt-file` miss
/// is instead a hard error at the CLI boundary, so remote daemons never
/// silently run the default when the user named a file).
fn read_prompt_file(path: &str, warn_id: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => Some(text),
        Ok(_) => None,
        Err(e) => {
            warn_once(
                warn_id,
                &format!("ignoring unreadable system prompt file '{path}': {e}"),
            );
            None
        }
    }
}

/// Custom base system prompt (DEX-13): replaces the built-in identity/rules;
/// project instructions, extensions and skills are still appended. Subagent
/// children keep their own persona/rules (`child_system_prompt`); `dex serve`
/// ignores CLI flags and falls back to its own env/file layers unless the
/// client forwards per-request text. Precedence mirrors every other knob —
/// explicit per-request/CLI text > env inline > env file > file inline >
/// file path > built-in default (`None`). Inline beats file within a layer;
/// any CLI beats any env beats any file. Empty/whitespace-only values count
/// as unset at every layer and fall through. Returns the text plus its
/// origin for `doctor`.
pub fn system_prompt_origin(explicit: Option<&str>) -> (Option<String>, &'static str) {
    system_prompt_origin_with_label(explicit, "--system-prompt")
}

/// Same as [`system_prompt_origin`], with the caller-provided label for the
/// explicit layer so `doctor` can report `--system-prompt-file` when the
/// text came from the file flag (runtime callers forward text only and keep
/// the default label — only the text matters there).
pub fn system_prompt_origin_with_label(
    explicit: Option<&str>,
    explicit_label: &'static str,
) -> (Option<String>, &'static str) {
    if let Some(text) = explicit.filter(|t| !t.trim().is_empty()) {
        return (Some(text.to_string()), explicit_label);
    }
    if let Ok(text) = env::var("DEX_SYSTEM_PROMPT") {
        if !text.trim().is_empty() {
            return (Some(text), "DEX_SYSTEM_PROMPT");
        }
    }
    if let Ok(path) = env::var("DEX_SYSTEM_PROMPT_FILE") {
        if !path.trim().is_empty() {
            if let Some(text) = read_prompt_file(path.trim(), "env:DEX_SYSTEM_PROMPT_FILE") {
                return (Some(text), "DEX_SYSTEM_PROMPT_FILE");
            }
        }
    }
    let file = load_config_file();
    if let Some(text) = load_config_str(&file, "system_prompt").filter(|t| !t.trim().is_empty()) {
        return (Some(text), "config system_prompt:");
    }
    if let Some(path) =
        load_config_str(&file, "system_prompt_file").filter(|p| !p.trim().is_empty())
    {
        if let Some(text) = read_prompt_file(path.trim(), "config:system_prompt_file") {
            return (Some(text), "config system_prompt_file:");
        }
    }
    (None, "built-in default")
}

/// Resolve the client-side CLI pair (`--system-prompt` > `--system-prompt-file`)
/// into the single text override forwarded per-request, plus the flag it came
/// from for `doctor`. The file is read here so a remote daemon never has to
/// see the client's local path; a miss is a hard error instead of a silent
/// default. Whitespace-only inline/file content counts as unset (`None`) and
/// falls through to the env/file layers.
pub fn resolve_cli_system_prompt(
    inline: Option<String>,
    file: Option<String>,
) -> Result<Option<(String, &'static str)>, String> {
    if let Some(text) = inline.filter(|t| !t.trim().is_empty()) {
        return Ok(Some((text, "--system-prompt")));
    }
    if let Some(path) = file.filter(|p| !p.trim().is_empty()) {
        let path = path.trim().to_string();
        return std::fs::read_to_string(&path)
            .map(|text| {
                if text.trim().is_empty() {
                    None
                } else {
                    Some((text, "--system-prompt-file"))
                }
            })
            .map_err(|e| format!("cannot read --system-prompt-file '{path}': {e}"));
    }
    Ok(None)
}
