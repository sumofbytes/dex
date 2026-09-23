//! Slash-command parsing and execution: `SlashCommand` vocabulary, the
//! `/...` dispatcher, and each `cmd_*` handler (model, provider, resume,
//! extensions, ...). Suggestion/completion lives in `completion.rs`.

use super::super::push_info;
use super::super::rebuild_transcript;
use super::super::App;
use super::parser::is_extension_command;
use super::parser::split_extension_command;
use super::parser::COMMANDS;
use crate::protocol::AgentMode;
use crate::protocol::ChatMessage;
use crate::protocol::Provider;
use crate::session::Session;
use std::fs;
use std::path::Path;

/// Clear per-session TUI state for `/clear` (same session) and `/new`
/// (fresh session). Keeps connection/config/skills/history — everything else
/// (transcript, token spend, plan, queued steering/follow-ups, turn markers)
/// is scoped to the conversation being discarded.
pub(crate) fn reset_session_state(app: &mut App) {
    app.messages.truncate(1);
    app.turn_start = 0;
    app.pending_steering.clear();
    app.pending_followups.clear();
    app.cancel_requested = false;
    app.cancel_presses = 0;
    for approval in app.pending_approvals.drain(..) {
        let _ = approval
            .response
            .try_send(crate::protocol::ApprovalDecision::Deny);
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
    app.plan = crate::protocol::Plan::default();
    app.transcript.clear();
    // Wrapped/display caches are keyed by block stamps: a cleared transcript
    // reuses stamp 0, so stale rows would hit. Drop both (and any selection
    // into them) alongside the transcript.
    app.wrapped_cache.clear();
    app.display_cache.clear();
    app.selection = None;
    app.assistant_pending.clear();
    app.assistant_gap.reset();
    app.assistant_open = false;
    app.thinking_open = false;
    app.autoscroll = true;
    app.scroll = 0;
}

/// One parsed slash line: the leading command plus its trimmed argument
/// (`None` when nothing followed the command name). Parsing once here keeps
/// the local and remote entry points on the same grammar instead of each
/// re-deriving it from its own `starts_with` chain.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SlashCommand<'a> {
    Quit,
    Clear,
    New,
    Session,
    Permissions,
    Mode(Option<&'a str>),
    Resume(Option<&'a str>),
    Name(Option<&'a str>),
    Skill(Option<&'a str>),
    Model(Option<&'a str>),
    Provider(Option<&'a str>),
    Thinking(Option<&'a str>),
    Waive(Option<&'a str>),
    Undo,
    Mcp(Option<&'a str>),
    Extensions(Option<&'a str>),
    /// A registered extension slash command; carries the full line.
    Extension(&'a str),
    Help,
    Unknown,
}

/// Split a slash line into the command plus its trimmed argument.
pub(crate) fn parse(line: &str) -> SlashCommand<'_> {
    let Some(rest) = line.strip_prefix('/') else {
        return SlashCommand::Unknown;
    };
    // `/skill:<name>` is colon-attached and the name may contain spaces, so it
    // is matched before the first-token split.
    if let Some(name) = rest.strip_prefix("skill:") {
        return SlashCommand::Skill(Some(name.trim()));
    }
    let (word, arg) = match rest.split_once(char::is_whitespace) {
        Some((word, arg)) => (word, Some(arg.trim())),
        None => (rest, None),
    };
    match word {
        "quit" if arg.is_none_or(|a| a.is_empty()) => SlashCommand::Quit,
        "clear" if arg.is_none_or(|a| a.is_empty()) => SlashCommand::Clear,
        "new" if arg.is_none_or(|a| a.is_empty()) => SlashCommand::New,
        "session" if arg.is_none_or(|a| a.is_empty()) => SlashCommand::Session,
        "mode" => SlashCommand::Mode(arg),
        // Deprecated alias: kept so an old muscle-memory line still works and
        // points at `/mode`; no longer advertised in `COMMANDS`.
        "permissions" if arg.is_none_or(|a| a.is_empty()) => SlashCommand::Permissions,
        "resume" => SlashCommand::Resume(arg),
        "name" => SlashCommand::Name(arg),
        "model" => SlashCommand::Model(arg),
        "provider" => SlashCommand::Provider(arg),
        "thinking" => SlashCommand::Thinking(arg),
        "waive" => SlashCommand::Waive(arg),
        "undo" if arg.is_none_or(|a| a.is_empty()) => SlashCommand::Undo,
        "mcp" => SlashCommand::Mcp(arg),
        "extensions" => SlashCommand::Extensions(arg),
        "help" if arg.is_none_or(|a| a.is_empty()) => SlashCommand::Help,
        _ if is_extension_command(line) => SlashCommand::Extension(line),
        _ => SlashCommand::Unknown,
    }
}

pub(crate) fn handle_slash(app: &mut App, line: &str) -> bool {
    match parse(line) {
        SlashCommand::Quit => return true,
        SlashCommand::Clear => cmd_clear(app),
        SlashCommand::New => cmd_new(app),
        SlashCommand::Session => cmd_session(app),
        SlashCommand::Permissions => cmd_permissions(app),
        SlashCommand::Mode(arg) => cmd_mode(app, arg),
        SlashCommand::Extension(line) => cmd_extension(app, line),
        SlashCommand::Mcp(arg) => cmd_mcp(app, arg),
        SlashCommand::Extensions(arg) => cmd_extensions(app, arg),
        SlashCommand::Resume(selector) => cmd_resume(app, selector),
        SlashCommand::Name(name) => cmd_name(app, name),
        SlashCommand::Skill(name) => cmd_skill(app, name),
        SlashCommand::Model(arg) => cmd_model(app, arg),
        SlashCommand::Provider(arg) => cmd_provider(app, arg),
        SlashCommand::Thinking(arg) => cmd_thinking(app, arg),
        SlashCommand::Waive(reason) => cmd_waive(app, reason),
        SlashCommand::Undo => cmd_undo(app),
        SlashCommand::Help => cmd_help(app, false),
        SlashCommand::Unknown => push_info(app, format!("unknown command: {line}")),
    }
    false
}

/// The `/help` "commands:" line, generated from the shared [`COMMANDS`] table
/// so the local and remote help text cannot drift.
pub(crate) fn commands_help_line() -> String {
    let mut line = String::from("commands:");
    for spec in COMMANDS {
        if spec.usage.is_empty() {
            continue;
        }
        line.push(' ');
        line.push_str(spec.usage);
    }
    line
}

/// The `/help` block. `remote` swaps the mouse line for the remote TUI's
/// selection behavior; every other line is shared.
pub(crate) fn cmd_help(app: &mut App, remote: bool) {
    push_info(app, commands_help_line());
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
        if remote {
            "mouse: drag, double/triple-click to select and copy · wheel scrolls".to_string()
        } else {
            "mouse: drag selects + copies · wheel scrolls transcript".to_string()
        },
    );
    push_info(app, "while working: Enter queues steer · Alt+Enter queues follow-up · Alt+Up recalls the newest queued message for editing · Esc/Ctrl+C cancels and restores queued input".to_string());
}

fn cmd_clear(app: &mut App) {
    if app.busy {
        push_info(app, "cannot clear while a turn is running.".to_string());
        return;
    }
    reset_session_state(app);
    let _ = app.session.clear_messages();
    let _ = app
        .session
        .set_state("plan", &crate::protocol::Plan::default().to_json());
    push_info(app, "history cleared.".to_string());
}

fn cmd_new(app: &mut App) {
    if app.busy {
        push_info(
            app,
            "cannot start a new session while a turn is running.".to_string(),
        );
        return;
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

fn cmd_session(app: &mut App) {
    push_info(app, format!("session: {}", app.session.display_name()));
    if let Some(path) = app.session.path() {
        push_info(app, format!("path: {}", path.display()));
    }
    push_info(app, format!("turns: {}", app.session.count_turns()));
}

fn cmd_permissions(app: &mut App) {
    push_info(app, "permissions is now /mode".to_string());
    cmd_mode(app, None);
}

/// `/mode [plan|manual|auto]`: report the current mode and its derived
/// permission, or switch. The workspace fact the old `/permissions` printed
/// is gone — `status_pieces` already shows the compact cwd.
fn cmd_mode(app: &mut App, arg: Option<&str>) {
    match arg {
        None | Some("") => {
            let mode = AgentMode::from_permission(app.config.permission);
            push_info(app, format!("mode: {}", mode.label()));
            push_info(
                app,
                format!(
                    "permission: {} (derived from mode)",
                    app.config.permission.as_str()
                ),
            );
        }
        Some(raw) => match AgentMode::parse(raw) {
            Ok(mode) => {
                app.config.permission = mode.permission();
                push_info(
                    app,
                    format!(
                        "mode: {} (permission: {})",
                        mode.label(),
                        mode.permission().as_str()
                    ),
                );
            }
            Err(e) => push_info(app, e),
        },
    }
}

fn cmd_extension(app: &mut App, line: &str) {
    // Extension command (plan §7 P3): dispatch fire-and-forget with a
    // read-only policy — a slash command is not a permission gate.
    let (ext_id, name, arg) = split_extension_command(line);
    let label = format!("{name}@{ext_id}");
    let cancel = crate::agent::state::GlobalCancellation;
    crate::runtime::http::spawn_task(async move {
        match crate::extensions::run_command_global(&ext_id, &name, &arg, &cancel).await {
            Ok(out) => eprintln!("dex: [extensions] /{name}: {out}"),
            Err(e) => eprintln!("dex: [extensions] /{name} failed: {e}"),
        }
    });
    push_info(app, format!("command '{label}' dispatched (output → log)"));
}

fn cmd_mcp(app: &mut App, arg: Option<&str>) {
    match arg {
        Some("help") => {
            push_info(app, "/mcp shows server status including auth.".to_string());
            push_info(
                app,
                "MCP OAuth runs in the CLI: `dex mcp login <server>`, `dex mcp logout <server>`, `dex mcp status`.".to_string(),
            );
        }
        Some(_) => push_info(app, "usage: /mcp [help]".to_string()),
        None => {
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
                    for line in
                        crate::render::format::render_mcp_panel(&statuses, &tools, truncated)
                    {
                        push_info(app, line);
                    }
                    for line in crate::mcp::oauth::auth_lines() {
                        push_info(app, line);
                    }
                }
            }
        }
    }
}

fn cmd_extensions(app: &mut App, arg: Option<&str>) {
    let arg = arg.unwrap_or("");
    // A remote TUI's daemon is the process that dispatches `ext__*` tools and
    // hooks, so its manager is the one that matters: both subcommands go to the
    // daemon API (§9 — reload/status reach the dispatcher, never just the
    // client's local copy).
    if let Some(url) = app.daemon_url.clone() {
        let reload = arg == "reload";
        crate::runtime::http::spawn_task(async move {
            let client = match crate::client::http::DaemonClient::new(&url) {
                Ok(client) => client,
                Err(e) => {
                    eprintln!("dex: [extensions] daemon client: {e}");
                    return;
                }
            };
            let result = if reload {
                client.extensions_reload_async().await
            } else {
                client.extensions_status_async().await
            };
            match result {
                Ok(body) => {
                    let exts = body["extensions"].as_array().cloned().unwrap_or_default();
                    for ext in &exts {
                        eprintln!(
                            "dex: [extensions] {} {} — {} tool(s), events: {}",
                            ext["id"].as_str().unwrap_or("?"),
                            ext["version"].as_str().unwrap_or("?"),
                            ext["tools"].as_u64().unwrap_or(0),
                            ext["events"]
                                .as_array()
                                .map(|a| a
                                    .iter()
                                    .filter_map(|v| v.as_str())
                                    .collect::<Vec<_>>()
                                    .join(","))
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| "-".to_string()),
                        );
                    }
                    if exts.is_empty() {
                        eprintln!("dex: [extensions] none loaded on the daemon");
                    }
                    if reload {
                        eprintln!("dex: [extensions] daemon reload complete");
                    }
                }
                Err(e) => eprintln!("dex: [extensions] daemon request failed: {e}"),
            }
        });
        push_info(
            app,
            if arg == "reload" {
                "daemon extension reload started (output → log)".to_string()
            } else {
                "daemon extension status requested (output → log)".to_string()
            },
        );
    } else if arg == "reload" {
        // Fire-and-forget rescan in this process (local/loopback turn: it owns
        // the manager that dispatches).
        crate::runtime::http::spawn_task(async move {
            crate::extensions::global_manager().reload().await;
            eprintln!("dex: [extensions] reload complete");
        });
        push_info(app, "extension reload started".to_string());
    } else if !arg.is_empty() {
        push_info(app, "usage: /extensions [reload]".to_string());
    } else {
        let summaries = crate::extensions::loaded_summaries();
        if summaries.is_empty() {
            push_info(app, "no extensions loaded".to_string());
        }
        for (id, version, tools, events) in summaries {
            push_info(
                app,
                crate::extensions::summary_line(&id, &version, &tools, &events),
            );
        }
    }
}

fn cmd_resume(app: &mut App, selector: Option<&str>) {
    let Some(selector) = selector.filter(|s| !s.trim().is_empty()) else {
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
        return;
    };
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
        return;
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

fn cmd_name(app: &mut App, name: Option<&str>) {
    let name = name.unwrap_or("").trim();
    if name.is_empty() {
        return;
    }
    app.session.set_name(name.to_string()).ok();
    push_info(app, format!("session name: {}", name));
}

fn cmd_skill(app: &mut App, name: Option<&str>) {
    let name = name.unwrap_or("").trim();
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

/// All selectable provider names: builtins (incl. the `codex` alias) plus
/// every configured `providers:` entry. Shared by the `/provider` picker
/// (`completion.rs`) and the bare `/model` + `/provider` listings below so
/// discovery never drifts.
fn known_provider_names(app: &App) -> Vec<String> {
    let mut names: std::collections::BTreeSet<String> = crate::protocol::Provider::BUILTINS
        .iter()
        .map(|s| s.to_string())
        .collect();
    names.extend(app.config.provider_entries.keys().cloned());
    names.into_iter().collect()
}

/// Canonical `provider/model` identity for every user surface (status bar,
/// `/model` output, switch confirmations). The stored model id is already
/// routing-stripped; the provider prefix is what selects it.
fn canonical_model(app: &App) -> String {
    let model = app.config.model.as_str();
    if model.contains('/') {
        model.to_string()
    } else {
        format!("{}/{}", app.config.provider.name(), model)
    }
}

fn cmd_model(app: &mut App, arg: Option<&str>) {
    let m = arg.unwrap_or("").trim().to_string();
    if m.is_empty() {
        let current = canonical_model(app);
        let cost = crate::llm::config::cost_hint_for(&current)
            .or_else(|| crate::llm::config::cost_hint_for(&app.config.model));
        push_info(
            app,
            format!(
                "current model: {current} ({}{})",
                app.config.api.name(),
                cost.map(|c| format!(", {c}")).unwrap_or_default(),
            ),
        );
        push_info(
            app,
            format!("providers: {}", known_provider_names(app).join(", ")),
        );
        push_info(app, "usage: /model <provider>/<id>".to_string());
        return;
    }
    let old_provider = app.config.provider.clone();
    // Remote TUI: the daemon owns endpoint routing and credentials — the
    // client may not even have the target provider's key, so resolve
    // nothing locally. Update the display; `handle_remote_slash` forwards
    // the raw selection and the daemon persists it in the session state,
    // never in the shared config file.
    if app.remote_mode {
        if !app.config.available_models.iter().any(|c| c == &m) {
            app.config.available_models.push(m.clone());
        }
        app.config.model = m;
        push_info(
            app,
            format!("model selection sent to the daemon: {}", app.config.model),
        );
        return;
    }
    let endpoint = match app.config.apply_model(&m, true) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            push_info(app, format!("could not switch model: {error}"));
            return;
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
    // Surface the thinking knob when the picked model advertises reasoning
    // options (models.dev reasoning_options).
    let thinking_suffix = crate::llm::config::reasoning_options_for(&app.config.model)
        .map(|options| format!(", thinking: {}", options.join(", ")))
        .unwrap_or_default();
    let cost_suffix = crate::llm::config::cost_hint_for(&canonical_model(app))
        .or_else(|| crate::llm::config::cost_hint_for(&app.config.model))
        .map(|c| format!(", {c}"))
        .unwrap_or_default();
    let endpoint_suffix = endpoint
        .map(|name| format!(" via endpoint {name}"))
        .unwrap_or_default();
    push_info(
        app,
        format!(
            "switched to {} ({}{}{}){endpoint_suffix}",
            canonical_model(app),
            app.config.api.name(),
            cost_suffix,
            thinking_suffix,
        ),
    );
}

fn cmd_provider(app: &mut App, arg: Option<&str>) {
    let Some(name) = arg.filter(|s| !s.trim().is_empty()) else {
        let names = known_provider_names(app);
        let current = app.config.provider.name();
        let listed = names
            .iter()
            .map(|n| {
                if n == current {
                    format!("{n} (current)")
                } else {
                    n.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        push_info(app, format!("current provider: {current}"));
        push_info(app, format!("providers: {listed}"));
        push_info(app, "usage: /provider <name>".to_string());
        return;
    };
    let known: std::collections::BTreeSet<String> =
        app.config.provider_entries.keys().cloned().collect();
    match Provider::parse_known(name, &known) {
        Some(provider) if provider == app.config.provider => {
            push_info(
                app,
                format!("provider already selected: {}", provider.name()),
            );
        }
        // Defensive: `handle_remote_slash` intercepts `/provider <name>`
        // before this runs, so remote clients never reach it — the gate
        // mirrors `cmd_model` in case that routing ever changes.
        Some(provider) => match app.config.switch_provider(&provider, !app.remote_mode) {
            Ok(()) => {
                let _ = app.session.set_state("provider", provider.name());
                push_info(app, format!("switched to provider: {}", provider.name()));
            }
            Err(error) => push_info(app, format!("could not switch provider: {}", error)),
        },
        None => push_info(
            app,
            format!(
                "unknown provider: {name}; available: {}",
                known_provider_names(app).join(", ")
            ),
        ),
    }
}

fn cmd_thinking(app: &mut App, arg: Option<&str>) {
    let Some(arg) = arg else {
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
        return;
    };
    let arg = arg.trim();
    if arg.is_empty() {
        push_info(app, "usage: /thinking <level>|clear".to_string());
    } else if ["clear", "auto", "off"]
        .iter()
        .any(|w| w.eq_ignore_ascii_case(arg))
    {
        crate::llm::config::remember_thinking_effort(&app.config.base_url, &app.config.model, None);
        app.config.refresh_thinking_effort();
        push_info(
            app,
            match &app.config.thinking_effort {
                Some(effort) => format!("thinking effort cleared (env default: {effort})"),
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

fn cmd_waive(app: &mut App, reason: Option<&str>) {
    let reason = reason.unwrap_or("").trim().to_string();
    if reason.is_empty() {
        push_info(app, "usage: /waive <reason>".to_string());
        return;
    }
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

fn cmd_undo(app: &mut App) {
    match crate::session::undo_last_change(&mut app.session) {
        Ok(message) => push_info(app, message),
        Err(e) => push_info(app, format!("undo: {e}")),
    }
}

/// Re-apply provider/model overrides that were persisted with the session
/// (`/model` and `/provider` write `session_state` entries; later entries in
/// the JSONL win, matching the append-order semantics used for messages).
/// Each applied switch is reported to the transcript so the user can see why
/// their model changed on resume. Silently keeps the current config when the
/// session predates state entries, or when a persisted provider is no longer
/// resolvable in this environment.
pub(crate) fn apply_session_state(app: &mut App, session_path: Option<&Path>) {
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
            let via = endpoint
                .map(|name| format!(" via endpoint {name}"))
                .unwrap_or_default();
            push_info(
                app,
                format!("restored model: {}{via}", canonical_model(app)),
            );
        }
    }
    if let Some(plan_json) = state.get("plan") {
        let plan = crate::protocol::Plan::from_json(plan_json);
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
