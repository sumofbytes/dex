#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

#[allow(unused_imports)]
use crate::core::types::Plan;
use crate::core::types::{ChatMessage, Provider};
use crate::session::Session;

use super::{push_info, rebuild_transcript, App, InputField};

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/quit", "Exit the REPL"),
    ("/clear", "Clear conversation history"),
    ("/new", "Start a new session"),
    ("/session", "Show current session details"),
    ("/resume", "List or resume a session"),
    ("/permissions", "Show permission mode and workspace"),
    ("/mcp", "Show MCP server status and auth"),
    ("/mcp help", "MCP OAuth help (login/logout run in the CLI)"),
    ("/name", "Rename the current session"),
    ("/model", "Show or switch the model"),
    ("/provider", "Show or switch the provider"),
    ("/thinking", "Show or set reasoning effort"),
    ("/waive <reason>", "Waive verification with a reason"),
    ("/undo", "Undo the last recorded file change"),
    ("/help", "Show available commands"),
];

fn save_plan(app: &mut App) -> std::io::Result<()> {
    let json = app.plan.to_json();
    app.session.set_state("plan", &json)
}

/// Every session for this workspace, newest first, excluding the current one.
/// No emptiness filtering: users pick by index/time, and resuming a session
/// without messages just shows an empty transcript. Listing is header-only
/// (`Session::list` reads one line per file), so no cache is needed.
fn resume_candidates(app: &App) -> Vec<(PathBuf, crate::session::SessionHeader)> {
    let current = app.session.id().to_string();
    Session::list(&app.cwd)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, header)| header.id() != current)
        .collect()
}

pub(super) fn slash_suggestions(app: &App) -> Vec<(String, String)> {
    let input = app.input.text();
    if app.busy || !input.starts_with('/') || input.contains('\n') {
        return Vec::new();
    }

    if let Some(query) = input.strip_prefix("/model ") {
        let query = query.to_ascii_lowercase();
        return app
            .config
            .available_models
            .iter()
            .filter(|model| {
                let lower = model.to_ascii_lowercase();
                lower.starts_with(&query)
                    || lower
                        .split('/')
                        .next_back()
                        .is_some_and(|tail| tail.starts_with(&query))
            })
            .map(|model| {
                let is_current = model == &app.config.model
                    || model.ends_with(&format!("/{}", app.config.model));
                (
                    format!("/model {model}"),
                    if is_current {
                        "Current model".to_string()
                    } else if let Some((prov, _)) = model.split_once('/') {
                        prov.to_string()
                    } else {
                        String::new()
                    },
                )
            })
            .collect();
    }
    if let Some(query) = input.strip_prefix("/provider ") {
        let query = query.to_ascii_lowercase();
        return ["opencode", "openai-codex"]
            .into_iter()
            .filter(|provider| provider.starts_with(&query))
            .map(|provider| {
                (
                    format!("/provider {provider}"),
                    if *provider == *app.config.provider.name() {
                        "Current provider".to_string()
                    } else {
                        String::new()
                    },
                )
            })
            .collect();
    }
    if input == "/resume" || input.starts_with("/resume ") {
        let query = input
            .strip_prefix("/resume")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        // Hide the current session; everything else is listed by index/time.
        let filtered = resume_candidates(app);
        return filtered
            .iter()
            .enumerate()
            .filter(|(i, (_, header))| {
                if query.is_empty() {
                    return true;
                }
                let name = header.name().unwrap_or("").to_ascii_lowercase();
                let id = header.id().to_ascii_lowercase();
                i.to_string().starts_with(&query)
                    || name.contains(&query)
                    || id.starts_with(&query)
                    || header.timestamp().to_ascii_lowercase().contains(&query)
            })
            .map(|(i, (_, header))| {
                let name = header.name().unwrap_or("(unnamed)");
                (
                    format!("/resume {i}"),
                    format!("{} · {}", name, header.timestamp()),
                )
            })
            .collect();
    }
    if input.contains(' ') {
        return Vec::new();
    }
    let query = input.to_ascii_lowercase();
    let mut suggestions: Vec<(String, String)> = SLASH_COMMANDS
        .iter()
        .filter(|(command, _)| command.starts_with(&query))
        .map(|(command, description)| ((*command).to_string(), (*description).to_string()))
        .collect();
    suggestions.extend(
        app.skills
            .iter()
            .map(|skill| {
                (
                    format!("/skill:{}", skill.name),
                    "Load this skill".to_string(),
                )
            })
            .filter(|(command, _)| command.to_ascii_lowercase().starts_with(&query)),
    );
    suggestions
}

pub(super) fn complete_slash(app: &mut App) -> bool {
    let suggestions = slash_suggestions(app);
    let Some((command, _)) =
        suggestions.get(app.slash_selected.min(suggestions.len().saturating_sub(1)))
    else {
        return false;
    };
    // Choices that take an argument already end in a space; don't stack a
    // second one ("/plan add  " trims to a bare "/plan add" that matches no
    // handler arm and reports "unknown command").
    let completed = if command.ends_with(' ') {
        command.clone()
    } else {
        format!("{command} ")
    };
    if app.input.text() == completed {
        return false;
    }
    app.input = InputField::from_text(&completed);
    app.slash_selected = 0;
    true
}

/// Dismiss the slash popup without completing anything: clears the drafted
/// command line and resets the highlight. Returns false when no popup is
/// open (suggestions empty) so callers can fall through to other handling.
/// The popup is derived from the input text, so clearing the draft is what
/// actually closes it.
pub(super) fn dismiss_slash(app: &mut App) -> bool {
    if slash_suggestions(app).is_empty() {
        return false;
    }
    app.input = InputField::from_text("");
    app.slash_selected = 0;
    true
}

/// Display label for a popup row. Inside an argument picker (`/model `,
/// `/provider `, `/resume …`) the completion text repeats the command
/// (`/model gpt-5`), so rows show just the item (`gpt-5`) — the popup
/// header already names the picker. Outside a picker the command itself
/// is the label.
pub(super) fn suggestion_label<'a>(input: &str, command: &'a str) -> &'a str {
    let prefix = if input.starts_with("/model ") {
        Some("/model ")
    } else if input.starts_with("/provider ") {
        Some("/provider ")
    } else if input == "/resume" || input.starts_with("/resume ") {
        Some("/resume ")
    } else {
        None
    };
    match prefix {
        Some(prefix) => command.strip_prefix(prefix).unwrap_or(command),
        None => command,
    }
}

/// Bare slash commands that open a picker (`/model` + `/provider` complete
/// from the catalog, `/resume` from the session list). Enter on the bare
/// form expands to `"<cmd> "` and keeps the popup open instead of
/// submitting — the bare form would only print info into the transcript
/// ("current model: …", "sessions: …"), which is never what Enter means
/// when the popup is offering a choice.
pub(super) const EXPAND_ON_ENTER: &[&str] = &["/model", "/provider", "/resume"];

/// Rewrite a bare picker command (`/model`, `/provider`, `/resume`) to
/// `"<cmd> "` so the popup shows the full choice list. Returns true when
/// it expanded — the caller must not submit. Anything with an argument
/// already (`/model foo`, `/resume 0`), a newline, or a non-picker command
/// (`/clear`) is left untouched.
///
/// `/resume` needs the direct rewrite: its suggestions are session args, so
/// `complete_slash` would jump straight to `/resume 0` and resume the first
/// session instead of showing the list.
pub(super) fn expand_bare_command(app: &mut App) -> bool {
    let text = app.input.text();
    if text.contains(' ') || text.contains('\n') {
        return false;
    }
    let trimmed = text.trim();
    if EXPAND_ON_ENTER.contains(&trimmed) {
        app.input = InputField::from_text(&format!("{trimmed} "));
        app.slash_selected = 0;
        return true;
    }
    false
}

/// Clear per-session TUI state for `/clear` (same session) and `/new`
/// (fresh session). Keeps connection/config/skills/history — everything else
/// (transcript, token spend, plan, queued steering/follow-ups, turn markers)
/// is scoped to the conversation being discarded.
pub(super) fn reset_session_state(app: &mut App) {
    app.messages.truncate(1);
    app.turn_start = 0;
    app.pending_steering.clear();
    app.pending_followups.clear();
    app.cancel_requested = false;
    app.cancel_presses = 0;
    for approval in app.pending_approvals.drain(..) {
        let _ = approval
            .response
            .try_send(crate::core::types::ApprovalDecision::Deny);
    }
    app.approval_rx = None;
    app.steering_rx = None;
    app.followup_rx = None;
    app.tool_state.last_usage = None;
    app.tool_state.last_cached = None;
    app.tool_state.total_usage = 0;
    app.tool_state.total_output = 0;
    app.tool_state.total_cost = 0.0;
    app.tool_state.last_tok_s = None;
    app.tool_state.verify_dirty = false;
    app.plan = crate::core::types::Plan::default();
    app.transcript.clear();
    app.assistant_pending.clear();
    app.assistant_gap.reset();
    app.assistant_open = false;
    app.thinking_open = false;
    app.autoscroll = true;
    app.scroll = 0;
}

pub(super) fn handle_slash(app: &mut App, line: &str) -> bool {
    match line {
        "/quit" => return true,
        "/clear" => {
            if app.busy {
                push_info(app, "cannot clear while a turn is running.".to_string());
                return false;
            }
            reset_session_state(app);
            let _ = app.session.clear_messages();
            let _ = app
                .session
                .set_state("plan", &crate::core::types::Plan::default().to_json());
            push_info(app, "history cleared.".to_string());
        }
        "/new" => {
            if app.busy {
                push_info(
                    app,
                    "cannot start a new session while a turn is running.".to_string(),
                );
                return false;
            }
            let system = app.messages.first().cloned();
            reset_session_state(app);
            let cwd = app.cwd.clone();
            match Session::new(cwd, None) {
                Ok(mut s) => {
                    if let Some(system) = system {
                        s.append_message(&system).ok();
                    }
                    app.session = s;
                    push_info(app, "new session started.".to_string());
                }
                Err(e) => push_info(app, format!("could not start new session: {}", e)),
            }
        }
        "/session" => {
            push_info(app, format!("session: {}", app.session.display_name()));
            if let Some(path) = app.session.path() {
                push_info(app, format!("path: {}", path.display()));
            }
            push_info(app, format!("turns: {}", app.session.count()));
        }
        "/permissions" => {
            push_info(app, format!("permission mode: {:?}", app.config.permission));
            push_info(app, format!("workspace: {}", app.cwd));
        }
        _ if line.starts_with("/mcp ") => match line["/mcp ".len()..].trim() {
            "help" => {
                push_info(app, "/mcp shows server status including auth.".to_string());
                push_info(
                    app,
                    "MCP OAuth runs in the CLI: `dex mcp login <server>`, `dex mcp logout <server>`, `dex mcp status`.".to_string(),
                );
            }
            _ => push_info(app, "usage: /mcp [help]".to_string()),
        },
        "/mcp" => {
            // Sync snapshot only: a slash handler must not initialize the
            // manager (that would spawn background connects from the TUI
            // process) — remote clients fetch daemon state via /api/mcp.
            match crate::mcp::cached_statuses() {
                None => push_info(
                    app,
                    "MCP status unavailable (manager not initialized).".to_string(),
                ),
                Some(statuses) => {
                    let tools = crate::mcp::cached_tools();
                    let truncated = crate::mcp::cached_truncated();
                    for line in crate::core::format::render_mcp_panel(&statuses, &tools, truncated)
                    {
                        push_info(app, line);
                    }
                    for line in crate::mcp::oauth::auth_lines() {
                        push_info(app, line);
                    }
                }
            }
        }
        "/resume" => {
            let sessions = Session::list(&app.cwd).unwrap_or_default();
            let filtered: Vec<(std::path::PathBuf, crate::session::SessionHeader)> = sessions
                .into_iter()
                .filter(|(_, header)| header.id() != app.session.id())
                .collect();
            if filtered.is_empty() {
                push_info(app, "no sessions found.".to_string());
            } else {
                push_info(app, "sessions:".to_string());
                for (i, (path, header)) in filtered.iter().enumerate() {
                    let name = header.name().unwrap_or("(unnamed)");
                    push_info(app, format!("  {}: {} ({})", i, name, path.display()));
                }
            }
        }
        _ if line.starts_with("/resume ") => {
            let selector = line["/resume ".len()..].trim();
            let sessions = Session::list(&app.cwd).unwrap_or_default();
            let filtered: Vec<(std::path::PathBuf, crate::session::SessionHeader)> = sessions
                .into_iter()
                .filter(|(_, header)| header.id() != app.session.id())
                .collect();
            let path_opt = if let Ok(idx) = selector.parse::<usize>() {
                filtered.get(idx).map(|(p, _)| p.clone())
            } else {
                let q = selector.to_ascii_lowercase();
                filtered
                    .iter()
                    .find(|(p, h)| {
                        h.id().to_ascii_lowercase().starts_with(&q)
                            || h.name().unwrap_or("").to_ascii_lowercase().contains(&q)
                            || p.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("")
                                .to_ascii_lowercase()
                                .starts_with(&q)
                    })
                    .map(|(p, _)| p.clone())
                    .or_else(|| {
                        // Fallback to legacy path-based resume for compatibility.
                        let cand = std::path::PathBuf::from(selector);
                        filtered
                            .iter()
                            .find(|(p, _)| p == &cand)
                            .map(|(p, _)| p.clone())
                    })
            };
            let Some(path) = path_opt else {
                push_info(
                    app,
                    format!("could not resume session: {selector} not found"),
                );
                return false;
            };
            match Session::from_path(&path) {
                Ok(session) => {
                    let loaded = session
                        .path()
                        .and_then(|p| crate::session::load_messages_from_session(p).ok())
                        .unwrap_or_default();
                    let system = app.messages.first().cloned();
                    app.messages = loaded;
                    if let Some(system) = system {
                        app.messages.insert(0, system);
                    }
                    let session_path = session.path().map(|p| p.to_path_buf());
                    app.session = session;
                    rebuild_transcript(app);
                    apply_session_state(app, session_path.as_deref());
                    push_info(
                        app,
                        format!("resumed session: {}", app.session.display_name()),
                    );
                }
                Err(e) => push_info(app, format!("could not resume session: {}", e)),
            }
        }
        _ if line.starts_with("/name ") => {
            let name = line["/name ".len()..].trim().to_string();
            if !name.is_empty() {
                app.session.set_name(name.clone()).ok();
                push_info(app, format!("session name: {}", name));
            }
        }
        _ if line.starts_with("/skill:") => {
            let name = line["/skill:".len()..].trim();
            if let Some(skill) = app.skills.iter().find(|s| s.name == name) {
                let content = fs::read_to_string(&skill.path).unwrap_or_default();
                app.messages.push(ChatMessage::user_named(
                    format!("--- Skill: {} ---\n{}", skill.name, content),
                    "skill",
                ));
                let _ = app.session.append_message(app.messages.last().unwrap());
                push_info(app, format!("loaded skill: {}", skill.name));
            } else {
                push_info(app, format!("skill not found: {}", name));
                push_info(app, "available skills:".to_string());
                let names: Vec<String> = app.skills.iter().map(|s| s.name.clone()).collect();
                for n in names {
                    push_info(app, format!("  - {}", n));
                }
            }
        }
        "/model" => {
            push_info(
                app,
                format!(
                    "current model: {} ({})",
                    app.config.model,
                    app.config.api.name()
                ),
            );
        }
        "/provider" => {
            push_info(
                app,
                format!("current provider: {}", app.config.provider.name()),
            );
            push_info(
                app,
                "available providers: opencode, openai-codex, anthropic".to_string(),
            );
        }
        "/thinking" => {
            let advertised = crate::llm::config::reasoning_options_for(&app.config.model);
            push_info(
                app,
                match (&app.config.thinking_effort, advertised) {
                    (Some(effort), Some(options)) => format!(
                        "thinking effort: {effort} (options: {})",
                        options.join(", ")
                    ),
                    (Some(effort), None) => format!("thinking effort: {effort}"),
                    (None, Some(options)) => {
                        format!("thinking effort: unset (options: {})", options.join(", "))
                    }
                    (None, None) => "thinking effort: unset".to_string(),
                },
            );
        }
        _ if line.starts_with("/thinking ") => {
            let arg = line["/thinking ".len()..].trim();
            if arg.is_empty() {
                push_info(app, "usage: /thinking <level>|clear".to_string());
            } else if ["clear", "auto", "off"]
                .iter()
                .any(|w| w.eq_ignore_ascii_case(arg))
            {
                crate::llm::config::remember_thinking_effort(
                    &app.config.base_url,
                    &app.config.model,
                    None,
                );
                app.config.refresh_thinking_effort();
                push_info(
                    app,
                    match &app.config.thinking_effort {
                        Some(effort) => {
                            format!("thinking effort cleared (env default: {effort})")
                        }
                        None => "thinking effort cleared".to_string(),
                    },
                );
            } else {
                let model = app.config.model.clone();
                match crate::llm::config::validate_thinking_effort(&model, arg) {
                    Ok(level) => {
                        crate::llm::config::remember_thinking_effort(
                            &app.config.base_url,
                            &model,
                            Some(&level),
                        );
                        app.config.refresh_thinking_effort();
                        push_info(app, format!("thinking effort: {level} for {model}"));
                    }
                    Err(options) => push_info(
                        app,
                        format!(
                            "unknown thinking effort '{arg}' for {model} (options: {})",
                            options.join(", ")
                        ),
                    ),
                }
            }
        }
        _ if line.starts_with("/waive ") => {
            let reason = line["/waive ".len()..].trim().to_string();
            if reason.is_empty() {
                push_info(app, "usage: /waive <reason>".to_string());
            } else {
                app.messages.push(ChatMessage::user_named(
                    format!("[verify waived] {reason}"),
                    "waive",
                ));
                let _ = app.session.append_message(app.messages.last().unwrap());
                let _ = app.session.set_state(
                    "verify",
                    &serde_json::json!({
                        "disposition": "waived",
                        "reason": reason,
                        "timestamp": chrono::Utc::now().to_rfc3339(),
                    })
                    .to_string(),
                );
                push_info(app, "verification waived (recorded)".to_string());
            }
        }
        "/undo" => match crate::session::undo_last_change(&mut app.session) {
            Ok(message) => push_info(app, message),
            Err(e) => push_info(app, format!("undo: {e}")),
        },
        "/help" => {
            push_info(
                app,
                "commands: /quit /clear /new /session /resume [index|path] /permissions /mcp /name <n> /skill:<name> /model [<m>] /provider [<name>] /thinking [<level>|clear]"
                    .to_string(),
            );
            push_info(
                app,
                "prefix: !<command> runs shell directly, output feeds the next turn.".to_string(),
            );
            push_info(
                app,
                "prefix: !!<command> keeps the output out of model context.".to_string(),
            );
            push_info(
                app,
                "keys: Enter send · Shift+Enter / Ctrl+J newline · ↑↓ history · PgUp/PgDn/wheel scroll · Ctrl+T thinking"
                    .to_string(),
            );
            push_info(
                app,
                "mouse: drag selects + copies · wheel scrolls transcript".to_string(),
            );
            push_info(app, "while working: Enter queues steer · Alt+Enter queues follow-up · Alt+Up recalls the newest queued message for editing · Esc/Ctrl+C cancels and restores queued input".to_string());
        }
        _ if line.starts_with("/model ") => {
            let m = line["/model ".len()..].trim().to_string();
            if !m.is_empty() {
                let old_provider = app.config.provider.clone();
                let endpoint = match app.config.apply_model(&m, true) {
                    Ok(endpoint) => endpoint,
                    Err(error) => {
                        push_info(app, format!("could not switch model: {error}"));
                        return false;
                    }
                };
                if !app
                    .config
                    .available_models
                    .iter()
                    .any(|candidate| candidate == &m)
                {
                    app.config.available_models.push(m.clone());
                }
                let _ = app.session.set_state("model", &m);
                if app.config.provider != old_provider {
                    let _ = app
                        .session
                        .set_state("provider", app.config.provider.name());
                }
                let provider_suffix = if app.config.provider != old_provider {
                    format!(" via {}", app.config.provider.name())
                } else {
                    String::new()
                };
                // Surface the thinking knob when the picked model advertises
                // reasoning options (models.dev reasoning_options).
                let thinking_suffix = crate::llm::config::reasoning_options_for(&app.config.model)
                    .map(|options| format!(" (thinking: {})", options.join(", ")))
                    .unwrap_or_default();
                match endpoint {
                    Some(name) => push_info(
                        app,
                        format!(
                            "switched to model: {} @ {} ({}, {}){}{}",
                            app.config.model,
                            name,
                            app.config.base_url,
                            app.config.api.name(),
                            provider_suffix,
                            thinking_suffix
                        ),
                    ),
                    None => push_info(
                        app,
                        format!(
                            "switched to model: {} ({}, {}){}{}",
                            app.config.model,
                            app.config.api.name(),
                            app.config.base_url,
                            provider_suffix,
                            thinking_suffix
                        ),
                    ),
                }
            }
        }
        _ if line.starts_with("/provider ") => {
            let name = line["/provider ".len()..].trim();
            let known: std::collections::BTreeSet<String> =
                app.config.provider_entries.keys().cloned().collect();
            match Provider::parse_known(name, &known) {
                Some(provider) if provider == app.config.provider => {
                    push_info(
                        app,
                        format!("provider already selected: {}", provider.name()),
                    );
                }
                Some(provider) => match app.config.switch_provider(&provider, true) {
                    Ok(()) => {
                        let _ = app.session.set_state("provider", provider.name());
                        push_info(app, format!("switched to provider: {}", provider.name()));
                    }
                    Err(error) => push_info(app, format!("could not switch provider: {}", error)),
                },
                None => push_info(
                    app,
                    format!(
                        "unknown provider: {name}; use opencode, openai-codex or a providers: entry"
                    ),
                ),
            }
        }
        _ => push_info(app, format!("unknown command: {}", line)),
    }
    false
}

/// Re-apply provider/model overrides that were persisted with the session
/// (`/model` and `/provider` write `session_state` entries; later entries in
/// the JSONL win, matching the append-order semantics used for messages).
/// Each applied switch is reported to the transcript so the user can see why
/// their model changed on resume. Silently keeps the current config when the
/// session predates state entries, or when a persisted provider is no longer
/// resolvable in this environment.
pub(super) fn apply_session_state(app: &mut App, session_path: Option<&Path>) {
    let Some(path) = session_path else {
        return;
    };
    let state = match crate::session::load_session_state(path) {
        Ok(state) => state,
        Err(_) => return,
    };
    if let Some(name) = state.get("provider") {
        let known: std::collections::BTreeSet<String> =
            app.config.provider_entries.keys().cloned().collect();
        match Provider::parse_known(name, &known) {
            Some(provider) if provider != app.config.provider => {
                match app.config.switch_provider(&provider, false) {
                    Ok(()) => push_info(app, format!("restored provider: {}", provider.name())),
                    Err(error) => push_info(
                        app,
                        format!("could not restore provider '{}': {}", name, error),
                    ),
                }
            }
            _ => {}
        }
    }
    if let Some(model) = state.get("model") {
        if app.config.model != *model {
            let endpoint = match app.config.apply_model(model, false) {
                Ok(endpoint) => endpoint,
                Err(error) => {
                    push_info(app, format!("could not restore model '{model}': {error}"));
                    return;
                }
            };
            if !app
                .config
                .available_models
                .iter()
                .any(|candidate| candidate == model)
            {
                app.config.available_models.push(model.clone());
            }
            match endpoint {
                Some(name) => push_info(
                    app,
                    format!(
                        "restored model: {} @ {} ({}, {})",
                        app.config.model,
                        name,
                        app.config.base_url,
                        app.config.api.name()
                    ),
                ),
                None => push_info(
                    app,
                    format!("restored model: {} ({})", model, app.config.api.name()),
                ),
            }
        }
    }
    if let Some(plan_json) = state.get("plan") {
        let plan = crate::core::types::Plan::from_json(plan_json);
        if !plan.is_empty() {
            app.plan = plan;
            let done = app.plan.steps.iter().filter(|(_, d)| *d).count();
            push_info(
                app,
                format!("restored plan: {} / {} steps", done, app.plan.steps.len()),
            );
            if let Some(g) = &app.plan.goal {
                push_info(app, format!("restored goal: {g}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::Skill;

    fn new_app() -> App {
        App::test_app()
    }

    /// Info lines pushed by slash handlers (`push_info` appends `Info`
    /// transcript blocks; grab their plain text for assertions).
    fn info_texts(app: &App) -> Vec<String> {
        app.transcript
            .iter()
            .filter_map(|b| match b {
                crate::ui::TranscriptBlock::Info { line, .. } => Some(
                    line.spans
                        .iter()
                        .map(|s| s.content.to_string())
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect()
    }

    fn has_info(app: &App, needle: &str) -> bool {
        info_texts(app).iter().any(|t| t.contains(needle))
    }

    fn type_input(app: &mut App, text: &str) {
        app.input = InputField::from_text(text);
        app.slash_selected = 0;
    }

    #[test]
    fn suggestions_only_for_bare_slash_prefixes() {
        let mut app = new_app();
        type_input(&mut app, "");
        assert!(slash_suggestions(&app).is_empty(), "no popup without input");
        type_input(&mut app, "hello");
        assert!(
            slash_suggestions(&app).is_empty(),
            "no popup for plain text"
        );

        type_input(&mut app, "a\nb");
        assert!(
            slash_suggestions(&app).is_empty(),
            "no popup across a newline"
        );

        app.busy = true;
        type_input(&mut app, "/cle");
        assert!(
            slash_suggestions(&app).is_empty(),
            "no popup while a turn runs"
        );
        app.busy = false;

        type_input(&mut app, "/cle");
        let got = slash_suggestions(&app);
        assert_eq!(
            got,
            vec![(
                "/clear".to_string(),
                "Clear conversation history".to_string()
            )]
        );

        type_input(&mut app, "/mod");
        let got = slash_suggestions(&app);
        assert_eq!(got[0].0, "/model");
        // Skills are suggested as /skill:<name> entries.
        app.skills.push(Skill {
            name: "demo".into(),
            description: "d".into(),
            path: "/tmp/demo/SKILL.md".into(),
        });
        type_input(&mut app, "/sk");
        let got = slash_suggestions(&app);
        assert_eq!(got[0].0, "/skill:demo");

        // Argument form without a known picker -> nothing.
        type_input(&mut app, "/nope arg");
        assert!(slash_suggestions(&app).is_empty());
    }

    #[test]
    fn model_and_provider_pickers_filter_and_mark_current() {
        let mut app = new_app();
        app.config.available_models = vec!["test".into(), "prov/other".into()];
        type_input(&mut app, "/model te");
        let got = slash_suggestions(&app);
        // "test" matches by prefix and is the current model; "prov/other"
        // matches by its tail ("other" does not start with "te" -> filtered).
        assert_eq!(
            got,
            vec![("/model test".to_string(), "Current model".to_string())]
        );

        type_input(&mut app, "/model other");
        let got = slash_suggestions(&app);
        assert_eq!(
            got,
            vec![("/model prov/other".to_string(), "prov".to_string())],
            "tail match shows the provider as the row hint"
        );

        type_input(&mut app, "/provider op");
        let got = slash_suggestions(&app);
        assert_eq!(
            got,
            vec![
                (
                    "/provider opencode".to_string(),
                    "Current provider".to_string()
                ),
                ("/provider openai-codex".to_string(), String::new()),
            ]
        );

        type_input(&mut app, "/provider zzz");
        assert!(slash_suggestions(&app).is_empty());
    }

    #[test]
    fn resume_picker_lists_other_sessions() {
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let data_dir =
            std::env::temp_dir().join(format!("dex-slash-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            [("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]
                .into_iter()
                .collect();
        let _env = crate::session::EnvGuard(saved);
        std::env::set_var("XDG_DATA_HOME", &data_dir);
        let other = Session::new("/tmp".to_string(), Some("other".to_string())).unwrap();
        std::mem::forget(other); // keep the file; no drop side effects expected

        let mut app = new_app();
        type_input(&mut app, "/resume");
        let got = slash_suggestions(&app);
        assert_eq!(got.len(), 1, "the current session must be hidden");
        assert_eq!(got[0].0, "/resume 0");
        assert!(got[0].1.contains("other"), "row shows the session name");

        // Query filters by name.
        type_input(&mut app, "/resume oth");
        assert_eq!(slash_suggestions(&app).len(), 1);
        type_input(&mut app, "/resume zzz");
        assert!(slash_suggestions(&app).is_empty());
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn picker_rows_strip_the_command_prefix() {
        assert_eq!(suggestion_label("/model te", "/model test"), "test");
        assert_eq!(
            suggestion_label("/provider o", "/provider opencode"),
            "opencode"
        );
        assert_eq!(suggestion_label("/resume", "/resume 0"), "0");
        assert_eq!(
            suggestion_label("/cle", "/clear"),
            "/clear",
            "outside a picker the command itself is the label"
        );
    }

    #[test]
    fn complete_slash_expands_the_selected_suggestion() {
        let mut app = new_app();
        type_input(&mut app, "/cle");
        assert!(complete_slash(&mut app));
        assert_eq!(app.input.text(), "/clear ");
        assert_eq!(app.slash_selected, 0);

        // Already-complete text is left alone (no stacked spaces).
        type_input(&mut app, "/clear ");
        app.slash_selected = 0;
        assert!(!complete_slash(&mut app));
        assert_eq!(app.input.text(), "/clear ");

        // Nothing to complete (no suggestions).
        type_input(&mut app, "plain text");
        assert!(!complete_slash(&mut app));
    }

    #[test]
    fn dismiss_slash_clears_the_draft_only_when_open() {
        let mut app = new_app();
        type_input(&mut app, "plain");
        assert!(!dismiss_slash(&mut app));
        assert_eq!(app.input.text(), "plain", "nothing cleared when closed");

        type_input(&mut app, "/cle");
        assert!(dismiss_slash(&mut app));
        assert_eq!(app.input.text(), "", "the draft is what closes the popup");
        assert_eq!(app.slash_selected, 0);
    }

    #[test]
    fn bare_picker_commands_expand_on_enter() {
        let mut app = new_app();
        for cmd in EXPAND_ON_ENTER {
            type_input(&mut app, cmd);
            assert!(expand_bare_command(&mut app), "{cmd} expands");
            assert_eq!(app.input.text(), format!("{cmd} "));
            assert_eq!(app.slash_selected, 0);
        }
        // Already has an argument / newline / non-picker -> untouched.
        type_input(&mut app, "/model gpt");
        assert!(!expand_bare_command(&mut app));
        assert_eq!(app.input.text(), "/model gpt");
        type_input(&mut app, "/resume 0");
        assert!(!expand_bare_command(&mut app));
        type_input(&mut app, "/clear");
        assert!(!expand_bare_command(&mut app));
        type_input(&mut app, "a\nb");
        assert!(!expand_bare_command(&mut app));
    }

    #[test]
    fn reset_session_state_clears_conversation_scope_only() {
        let mut app = new_app();
        app.messages.push(ChatMessage::user("m1"));
        app.messages.push(ChatMessage::user("m2"));
        app.pending_steering.push("s".into());
        app.pending_followups.push("f".into());
        app.cancel_requested = true;
        app.cancel_presses = 1;
        app.tool_state.total_usage = 99;
        app.tool_state.total_cost = 1.5;
        app.tool_state.verify_dirty = true;
        app.plan = crate::core::types::Plan {
            goal: Some("g".into()),
            steps: vec![("step".into(), false)],
            ..Default::default()
        };
        app.messages.clear();
        app.messages.push(ChatMessage::system("sys"));
        app.messages.push(ChatMessage::user("u"));
        // A pending approval is denied on reset so the agent loop unblocks.
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        app.pending_approvals.push(crate::ui::PendingApproval {
            name: "bash".into(),
            input: "{}".into(),
            agent: None,
            selected: 0,
            request_id: "r".into(),
            response: tx,
        });

        reset_session_state(&mut app);

        // The system prompt (first message) survives; the rest is dropped.
        assert_eq!(app.messages.len(), 1);
        assert!(app.messages[0].content_str().contains("sys"));
        assert!(app.pending_steering.is_empty() && app.pending_followups.is_empty());
        assert!(!app.cancel_requested && app.cancel_presses == 0);
        assert_eq!(app.tool_state.total_usage, 0);
        assert_eq!(app.tool_state.total_cost, 0.0);
        assert!(!app.tool_state.verify_dirty);
        assert!(app.plan.is_empty());
        assert!(app.transcript.is_empty());
        assert!(app.pending_approvals.is_empty());
        assert_eq!(
            rx.try_recv().ok(),
            Some(crate::core::types::ApprovalDecision::Deny),
            "pending approvals are denied, not dropped silently"
        );
    }

    #[test]
    fn handle_slash_basic_commands() {
        // /quit returns true (quit request), everything else false.
        let mut app = new_app();
        assert!(handle_slash(&mut app, "/quit"));

        let mut app = new_app();
        app.messages.push(ChatMessage::system("sys"));
        assert!(!handle_slash(&mut app, "/session"));
        assert!(
            has_info(&app, "session: "),
            "{}",
            info_texts(&app).join(" | ")
        );
        assert!(has_info(&app, "turns: 0"));

        let mut app = new_app();
        assert!(!handle_slash(&mut app, "/permissions"));
        assert!(has_info(&app, "permission mode: Trusted"));
        assert!(has_info(&app, "workspace: /tmp"));

        let mut app = new_app();
        assert!(!handle_slash(&mut app, "/no-such-command"));
        assert!(has_info(&app, "unknown command: /no-such-command"));

        let mut app = new_app();
        assert!(!handle_slash(&mut app, "/help"));
        assert!(has_info(&app, "commands: /quit"));
        assert!(has_info(&app, "prefix: !<command>"));
    }

    #[test]
    fn handle_slash_clear_refuses_while_busy_and_clears_when_idle() {
        let mut app = new_app();
        app.messages.push(ChatMessage::system("sys"));
        app.messages.push(ChatMessage::user("keep-me-out"));
        app.busy = true;
        assert!(!handle_slash(&mut app, "/clear"));
        assert!(has_info(&app, "cannot clear while a turn is running"));
        assert_eq!(app.messages.len(), 2, "busy /clear changes nothing");

        app.busy = false;
        assert!(!handle_slash(&mut app, "/clear"));
        assert!(has_info(&app, "history cleared"));
        assert_eq!(app.messages.len(), 1, "the system prompt survives");
    }

    #[test]
    fn handle_slash_mcp_variants() {
        // The `/mcp` snapshot depends on whether some other test has already
        // initialized the global manager (process-wide, parallel tests), so
        // both outcomes are valid: unavailable, or a rendered panel.
        let mut app = new_app();
        assert!(!handle_slash(&mut app, "/mcp"));
        let texts = info_texts(&app).join(" | ");
        let initialized = crate::mcp::cached_statuses().is_some();
        if initialized {
            assert!(!texts.contains("MCP status unavailable"), "{texts}");
        } else {
            assert!(texts.contains("MCP status unavailable"), "{texts}");
        }

        let mut app = new_app();
        assert!(!handle_slash(&mut app, "/mcp help"));
        assert!(has_info(&app, "dex mcp login"));

        let mut app = new_app();
        assert!(!handle_slash(&mut app, "/mcp bogus"));
        assert!(has_info(&app, "usage: /mcp [help]"));
    }

    #[test]
    fn handle_slash_name_sets_session_name() {
        let mut app = new_app();
        assert!(!handle_slash(&mut app, "/name  Fancy  "));
        assert!(has_info(&app, "session name: Fancy"));
        assert_eq!(app.session.name(), Some("Fancy"));

        // Blank name is ignored (no info, no rename).
        let mut app = new_app();
        let before = info_texts(&app).len();
        assert!(!handle_slash(&mut app, "/name   "));
        assert_eq!(info_texts(&app).len(), before);
    }

    #[test]
    fn handle_slash_skill_load_and_miss() {
        let root = std::env::temp_dir().join(format!("dex-slash-skill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("demo")).unwrap();
        std::fs::write(root.join("demo/SKILL.md"), "body").unwrap();

        let mut app = new_app();
        app.skills.push(Skill {
            name: "demo".into(),
            description: "d".into(),
            path: root.join("demo/SKILL.md"),
        });
        assert!(!handle_slash(&mut app, "/skill:demo"));
        assert!(has_info(&app, "loaded skill: demo"));
        assert_eq!(
            app.messages.last().unwrap().name.as_deref(),
            Some("skill"),
            "the skill lands as a named user message"
        );
        assert!(app.messages.last().unwrap().content_str().contains("body"));

        let mut app = new_app();
        app.skills.push(Skill {
            name: "demo".into(),
            description: "d".into(),
            path: root.join("demo/SKILL.md"),
        });
        assert!(!handle_slash(&mut app, "/skill:missing"));
        assert!(has_info(&app, "skill not found: missing"));
        assert!(has_info(&app, "available skills:"));
        assert!(has_info(&app, "  - demo"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn handle_slash_thinking_show_set_and_clear() {
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cache = std::env::temp_dir().join(format!("dex-slash-think-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = [
            ("XDG_CACHE_HOME", std::env::var_os("XDG_CACHE_HOME")),
            (
                "DEX_THINKING_EFFORT",
                std::env::var_os("DEX_THINKING_EFFORT"),
            ),
            ("DEX_CONFIG", std::env::var_os("DEX_CONFIG")),
        ]
        .into_iter()
        .collect();
        let _env = crate::session::EnvGuard(saved);
        std::env::set_var("XDG_CACHE_HOME", &cache);
        std::env::remove_var("DEX_THINKING_EFFORT");
        // No user config: a file `thinking_effort:` must not leak into the
        // refresh after `/thinking clear`.
        std::env::set_var("DEX_CONFIG", cache.join("no-such-config.yaml"));
        // A minimal models.dev cache entry advertising effort options, so the
        // validation branches are reachable without the network.
        std::fs::create_dir_all(cache.join("dex")).unwrap();
        std::fs::write(
            cache.join("dex/models.dev.json"),
            r#"{"openai":{"models":{"fake-reasoner":{"reasoning_options":[{"values":["low","high"]}]}}}}"#,
        )
        .unwrap();

        let mut app = new_app();
        app.config.model = "fake-reasoner".into();
        assert!(!handle_slash(&mut app, "/thinking"));
        assert!(has_info(
            &app,
            "thinking effort: unset (options: low, high)"
        ));

        // A level the model does not advertise is rejected with the options.
        let mut app = new_app();
        app.config.model = "fake-reasoner".into();
        assert!(!handle_slash(&mut app, "/thinking bogus"));
        assert!(
            has_info(
                &app,
                "unknown thinking effort 'bogus' for fake-reasoner (options: low, high)"
            ),
            "{}",
            info_texts(&app).join(" | ")
        );
        assert!(app.config.thinking_effort.is_none());

        // A valid level is remembered and shown for the model.
        let mut app = new_app();
        app.config.model = "fake-reasoner".into();
        assert!(!handle_slash(&mut app, "/thinking high"));
        assert!(
            has_info(&app, "thinking effort: high for fake-reasoner"),
            "{}",
            info_texts(&app).join(" | ")
        );
        assert_eq!(app.config.thinking_effort.as_deref(), Some("high"));

        // A blank argument prints usage.
        let mut app = new_app();
        assert!(!handle_slash(&mut app, "/thinking  "));
        assert!(has_info(&app, "usage: /thinking <level>|clear"));

        // "clear" resets to unset even when an effort was set.
        let mut app = new_app();
        app.config.model = "fake-reasoner".into();
        app.config.thinking_effort = Some("high".into());
        assert!(!handle_slash(&mut app, "/thinking clear"));
        assert!(
            has_info(&app, "thinking effort cleared"),
            "{}",
            info_texts(&app).join(" | ")
        );
        assert!(app.config.thinking_effort.is_none());
        let _ = std::fs::remove_dir_all(&cache);
    }

    #[test]
    fn handle_slash_waive_records_reason() {
        let mut app = new_app();
        assert!(!handle_slash(&mut app, "/waive   "));
        assert!(has_info(&app, "usage: /waive <reason>"));

        let mut app = new_app();
        assert!(!handle_slash(&mut app, "/waive flaky env"));
        assert!(has_info(&app, "verification waived"));
        assert_eq!(
            app.messages.last().unwrap().name.as_deref(),
            Some("waive"),
            "the reason lands as a named user message"
        );
        assert!(app
            .messages
            .last()
            .unwrap()
            .content_str()
            .contains("[verify waived] flaky env"));
    }

    #[test]
    fn handle_slash_undo_reports_in_memory_sessions() {
        // in-memory sessions have no change ledger: the error surfaces.
        let mut app = new_app();
        assert!(!handle_slash(&mut app, "/undo"));
        assert!(has_info(&app, "undo: "));
    }

    #[test]
    fn handle_slash_provider_known_and_unknown() {
        let mut app = new_app();
        assert!(!handle_slash(&mut app, "/provider nope"));
        assert!(has_info(&app, "unknown provider: nope"));

        let mut app = new_app();
        assert!(!handle_slash(&mut app, "/provider opencode"));
        assert!(has_info(&app, "provider already selected: opencode"));
    }

    #[test]
    fn handle_slash_model_switch_and_persist_is_hermetic() {
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dex-slash-model-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let saved: Vec<(&str, Option<std::ffi::OsString>)> = [
            ("DEX_CONFIG", std::env::var_os("DEX_CONFIG")),
            ("XDG_CACHE_HOME", std::env::var_os("XDG_CACHE_HOME")),
            ("DEX_CONTEXT_WINDOW", std::env::var_os("DEX_CONTEXT_WINDOW")),
        ]
        .into_iter()
        .collect();
        let _env = crate::session::EnvGuard(saved);
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
        std::env::remove_var("DEX_CONTEXT_WINDOW");

        let mut app = new_app();
        // A persisted session file so `set_state("model", ...)` has a home.
        let session_path = dir.join("model-test.jsonl");
        std::fs::write(
            &session_path,
            "{\"type\":\"session\",\"version\":1,\"id\":\"model-test\",\"timestamp\":\"t\",\"cwd\":\"/tmp/x\"}\n",
        )
        .unwrap();
        app.session = Session::from_path(&session_path).unwrap();
        assert!(!handle_slash(&mut app, "/model beta-model"));
        assert!(
            has_info(&app, "switched to model: beta-model"),
            "{}",
            info_texts(&app).join(" | ")
        );
        assert_eq!(app.config.model, "beta-model");
        assert!(
            app.config
                .available_models
                .contains(&"beta-model".to_string()),
            "the picked model joins the catalog list"
        );
        assert_eq!(
            crate::session::load_session_state(&session_path)
                .unwrap()
                .get("model"),
            Some(&"beta-model".to_string()),
            "/model persists the choice into session state"
        );

        // A blank argument changes nothing.
        let mut app = new_app();
        assert!(!handle_slash(&mut app, "/model  "));
        assert_eq!(app.config.model, "test");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_session_state_restores_plan_and_reports_model_errors() {
        let mut app = new_app();
        // No session path -> nothing happens.
        apply_session_state(&mut app, None);

        let dir = std::env::temp_dir().join(format!("dex-slash-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state-test.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"session\",\"version\":1,\"id\":\"state-test\",\"timestamp\":\"t\",\"cwd\":\"/tmp/x\"}\n",
        )
        .unwrap();
        {
            let mut session = Session::from_path(&path).unwrap();
            session
                .set_state(
                    "plan",
                    &crate::core::types::Plan {
                        goal: Some("build it".into()),
                        steps: vec![("one".into(), true), ("two".into(), false)],
                        ..Default::default()
                    }
                    .to_json(),
                )
                .unwrap();
            session.set_state("model", "not-a-model").unwrap();
        }

        apply_session_state(&mut app, Some(&path));
        let texts = info_texts(&app).join(" | ");
        assert!(texts.contains("restored plan: 1 / 2 steps"), "{texts}");
        assert!(texts.contains("restored goal: build it"), "{texts}");
        assert_eq!(app.plan.steps.len(), 2);
        // apply_model does not validate against the catalog: the stored id is
        // adopted as-is and reported.
        assert!(texts.contains("restored model: not-a-model"), "{texts}");
        assert_eq!(app.config.model, "not-a-model");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
