#![allow(dead_code)]

use std::fs;
use std::path::Path;

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
        let Ok(sessions) = Session::list(&app.cwd) else {
            return Vec::new();
        };
        // Hide the current session and empty sessions — they are the
        // just-created placeholder and make `0` point at an empty transcript.
        let filtered: Vec<(std::path::PathBuf, crate::session::SessionHeader)> = sessions
            .into_iter()
            .filter(|(path, header)| {
                if header.id() == app.session.id() {
                    return false;
                }
                // Skip sessions with no persisted messages (only header).
                crate::session::has_messages(path)
            })
            .collect();
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
    app.turn_started = None;
    app.last_activity = None;
    app.pending_steering.clear();
    app.pending_followups.clear();
    app.cancel_requested = false;
    if let Some(approval) = app.pending_approval.take() {
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
                        s.append_message(system).ok();
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
                .filter(|(path, header)| {
                    if header.id() == app.session.id() {
                        return false;
                    }
                    crate::session::has_messages(path)
                })
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
                .filter(|(path, header)| {
                    if header.id() == app.session.id() {
                        return false;
                    }
                    crate::session::has_messages(path)
                })
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
                let _ = app
                    .session
                    .append_message(app.messages.last().cloned().unwrap());
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
                "available providers: opencode, openai-codex".to_string(),
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
                let _ = app
                    .session
                    .append_message(app.messages.last().cloned().unwrap());
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
                "keys: Enter send · Shift+Enter newline · ↑↓ history · PgUp/PgDn/wheel scroll · Ctrl+T thinking"
                    .to_string(),
            );
            push_info(
                app,
                "mouse: drag selects + copies · wheel scrolls transcript".to_string(),
            );
            push_info(app, "while working: Enter queues steer · Alt+Enter queues follow-up · Esc/Ctrl+C cancels and restores queued input".to_string());
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
