//! Per-request overrides from CLI flags — shared by TUI remote client and `dex connect` one-shot.

use crate::cli::Args;

/// Per-request overrides built from CLI flags — shared by the TUI's remote
/// client and the `dex connect <url> "prompt"` one-shot, so daemon-backed
/// runs keep flag parity with in-process mode.
pub(crate) fn chat_options_from_args(args: &Args) -> crate::client::http::ChatOptions {
    crate::client::http::ChatOptions {
        skill_dirs: args
            .skill_dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        base_url: args.base_url.clone(),
        model: args.model.clone(),
        permission: args.permission.map(|mode| mode.as_str().to_string()),
        headers: if args.headers.is_empty() {
            None
        } else {
            let mut merged = std::collections::BTreeMap::new();
            for raw in &args.headers {
                for (k, v) in crate::llm::config::parse_headers_str(raw) {
                    crate::llm::config::insert_extra_header(&mut merged, &k, &v);
                }
            }
            Some(merged)
        },
        plan: None,
        system_prompt: cli_system_prompt(args).map(|(text, _)| text),
        thinking_effort: None,
        idempotency_key: None,
    }
}

/// Resolve `--system-prompt` / `--system-prompt-file` for client-side
/// forwarding. The file is read here so remote daemons work; a miss fails
/// fast instead of silently running the default. Returns the text plus the
/// flag it came from so `doctor` names the right origin.
pub(crate) fn cli_system_prompt(args: &Args) -> Option<(String, &'static str)> {
    match crate::llm::config::resolve_cli_system_prompt(
        args.system_prompt.clone(),
        args.system_prompt_file.clone(),
    ) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
