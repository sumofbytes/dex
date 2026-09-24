//! Translate CLI flags into daemon-client request options.

use crate::cli::Args;

/// Per-request overrides built from CLI flags — shared by the TUI's remote
/// client and the `dex connect <url> "prompt"` one-shot, so daemon-backed
/// runs keep flag parity with in-process mode.
///
/// `--mode` wins; else the mode derives from `--permission` so headless
/// runs send the same mode/permission pairing the TUI does.
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
        // Explicit `--mode` wins; else derive from `--permission` (flag >
        // env > default) so headless runs keep the same mode/permission
        // pairing the TUI sends.
        mode: args
            .mode
            .or(args
                .permission
                .map(crate::protocol::AgentMode::from_permission))
            .map(|m| m.as_str().to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::AgentMode;
    use crate::protocol::PermissionMode;

    fn args() -> Args {
        Args {
            base_url: None,
            model: None,
            session_path: None,
            no_session: false,
            new_session: false,
            session_name: None,
            skill_dirs: Vec::new(),
            extension_dirs: Vec::new(),
            permission: None,
            mode: None,
            headers: Vec::new(),
            system_prompt: None,
            system_prompt_file: None,
            reattach: None,
            rest: Vec::new(),
        }
    }

    #[test]
    fn no_mode_flags_send_no_selector() {
        let options = chat_options_from_args(&args());
        assert_eq!(options.mode, None);
        assert_eq!(options.permission, None);
    }

    #[test]
    fn permission_derives_the_mode() {
        let mut a = args();
        a.permission = Some(PermissionMode::ReadOnly);
        let options = chat_options_from_args(&a);
        assert_eq!(options.mode.as_deref(), Some("plan"));
        assert_eq!(options.permission.as_deref(), Some("read-only"));
    }

    #[test]
    fn explicit_mode_wins_over_permission() {
        let mut a = args();
        a.permission = Some(PermissionMode::ReadOnly);
        a.mode = Some(AgentMode::Manual);
        let options = chat_options_from_args(&a);
        assert_eq!(options.mode.as_deref(), Some("manual"));
        // The permission still rides along unchanged: the daemon clamps
        // the mode to it.
        assert_eq!(options.permission.as_deref(), Some("read-only"));
    }
}
