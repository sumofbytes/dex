use super::super::push_info;
use super::super::rebuild_transcript;
use super::super::App;
use super::super::InputField;
use super::parser::is_extension_command;
use super::parser::split_extension_command;
use super::parser::COMMANDS;
use crate::protocol::ChatMessage;
use crate::protocol::Provider;
use crate::session::Session;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

fn save_plan(app: &mut App) -> std::io::Result<()> {
    let json = app.plan.to_json();
    app.session.set_state("plan", &json)
}

/// Every session for this workspace, newest first, excluding the current one.
/// No emptiness filtering: users pick by index/time, and resuming a session
/// without messages just shows an empty transcript. Listing is header-only
/// (`Session::list` reads one line per file); the popup still caches it per
/// input change (`SlashCache`, keyed on the sessions-dir mtime) so idle
/// frames never re-walk the dir.
fn resume_candidates(app: &App) -> Vec<(PathBuf, crate::session::SessionHeader)> {
    let current = app.session.id().to_string();
    Session::list(&app.cwd)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, header)| header.id() != current)
        .collect()
}

/// Whether the slash popup should intercept keys and render: composer is
/// free (not busy, not mid history walk — the walk guard lives in
/// slash_suggestions) and the input drafts a command. The remote key arm
/// keys off this so a recalled "/clear" never traps Up/Down in the popup;
/// while it is open, Esc only dismisses it.
pub(crate) fn popup_open(app: &App) -> bool {
    !slash_suggestions(app).is_empty()
}

/// Cached slash-popup listing (perf doc §29): `slash_suggestions` runs per
/// frame while the composer holds a `/` line — including the `/model` walk
/// over thousands of catalog ids (lowercasing each) and the `/resume` dir
/// walk per keystroke. Keyed on the input plus everything the arms read
/// (current model + model/skill counts + sessions-dir mtime), so a hit is
/// exact and recomputation happens per input change, not per frame.
#[derive(PartialEq)]
pub(crate) struct SlashKey {
    pub(crate) input: String,
    pub(crate) model: String,
    pub(crate) provider: String,
    pub(crate) models_hash: u64,
    pub(crate) skills_hash: u64,
    /// Registered extension slash commands: `/extensions reload` (or a
    /// daemon push) changes what the popup may offer without touching
    /// the input, model list, or sessions dir.
    pub(crate) ext_hash: u64,
    /// Workspace the `/resume` listing was read from: a cwd switch with an
    /// identical sessions-dir mtime must still miss.
    pub(crate) cwd: String,
    pub(crate) resume_mtime: Option<std::time::SystemTime>,
}

fn str_list_hash(items: &[String]) -> u64 {
    // FNV-1a over lengths + bytes: a same-length content swap still misses.
    let mut h = 14695981039346656037u64;
    for s in items {
        for b in s.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(1099511628211);
        }
        h ^= 0xff;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

pub(crate) struct SlashCache {
    pub(crate) key: SlashKey,
    pub(crate) suggestions: Vec<(String, String)>,
}

pub(crate) fn slash_suggestions(app: &App) -> Vec<(String, String)> {
    let input = app.input.text();
    // The popup is a typing affordance, not a history companion: while the
    // composer holds a line recalled by the history walk (history_index is
    // Some), Up/Down must keep walking history instead of getting trapped
    // over a recalled "/clear" — see popup_open().
    if app.busy || app.history_index.is_some() || !input.starts_with('/') || input.contains('\n') {
        return Vec::new();
    }
    let key = SlashKey {
        input: input.clone(),
        model: app.config.model.clone(),
        provider: app.config.provider.name().to_string(),
        models_hash: str_list_hash(&app.config.available_models),
        skills_hash: str_list_hash(
            &app.skills
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>(),
        ),
        ext_hash: str_list_hash(
            &crate::extensions::command_list()
                .into_iter()
                .map(|(_, name, _)| name)
                .collect::<Vec<_>>(),
        ),
        cwd: app.cwd.clone(),
        // One stat per input change; a sessions-dir rewrite (new/removed
        // session) invalidates the `/resume` listing.
        resume_mtime: Session::list_dir_mtime(&app.cwd),
    };
    if let Some(hit) = app.slash_cache.borrow().as_ref() {
        if hit.key == key {
            return hit.suggestions.clone();
        }
    }
    let suggestions = compute_suggestions(app, &input);
    *app.slash_cache.borrow_mut() = Some(SlashCache {
        key,
        suggestions: suggestions.clone(),
    });
    suggestions
}

pub(super) fn compute_suggestions(app: &App, input: &str) -> Vec<(String, String)> {
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
    let mut suggestions: Vec<(String, String)> = COMMANDS
        .iter()
        .filter(|spec| spec.command.starts_with(&query))
        .map(|spec| (spec.command.to_string(), spec.description.to_string()))
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

pub(crate) fn complete_slash(app: &mut App) -> bool {
    let suggestions = slash_suggestions(app);
    let Some((command, _)) =
        suggestions.get(app.slash_selected.min(suggestions.len().saturating_sub(1)))
    else {
        return false;
    };
    // Choices that take an argument may already end in a space; don't stack a
    // second one (a doubled space trims to a bare command that matches no
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
pub(crate) fn dismiss_slash(app: &mut App) -> bool {
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
pub(crate) fn suggestion_label<'a>(input: &str, command: &'a str) -> &'a str {
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
pub(crate) const EXPAND_ON_ENTER: &[&str] = &["/model", "/provider", "/resume"];

/// Rewrite a bare picker command (`/model`, `/provider`, `/resume`) to
/// `"<cmd> "` so the popup shows the full choice list. Returns true when
/// it expanded — the caller must not submit. Anything with an argument
/// already (`/model foo`, `/resume 0`), a newline, or a non-picker command
/// (`/clear`) is left untouched.
///
/// `/resume` needs the direct rewrite: its suggestions are session args, so
/// `complete_slash` would jump straight to `/resume 0` and resume the first
/// session instead of showing the list.
pub(crate) fn expand_bare_command(app: &mut App) -> bool {
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
    push_info(app, format!("permission mode: {:?}", app.config.permission));
    push_info(app, format!("workspace: {}", app.cwd));
}

fn cmd_extension(app: &mut App, line: &str) {
    // Extension command (plan §7 P3): dispatch fire-and-forget with a
    // read-only policy — a slash command is not a permission gate.
    let (ext_id, name, arg) = split_extension_command(line);
    let label = format!("{name}@{ext_id}");
    let cancel = crate::agent::state::GlobalCancellation;
    crate::client::http::spawn_task(async move {
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
        crate::client::http::spawn_task(async move {
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
        crate::client::http::spawn_task(async move {
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

fn cmd_model(app: &mut App, arg: Option<&str>) {
    let m = arg.unwrap_or("").trim().to_string();
    if m.is_empty() {
        push_info(
            app,
            format!(
                "current model: {} ({})",
                app.config.model,
                app.config.api.name()
            ),
        );
        return;
    }
    let old_provider = app.config.provider.clone();
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
    let provider_suffix = if app.config.provider != old_provider {
        format!(" via {}", app.config.provider.name())
    } else {
        String::new()
    };
    // Surface the thinking knob when the picked model advertises reasoning
    // options (models.dev reasoning_options).
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

fn cmd_provider(app: &mut App, arg: Option<&str>) {
    let Some(name) = arg.filter(|s| !s.trim().is_empty()) else {
        push_info(
            app,
            format!("current provider: {}", app.config.provider.name()),
        );
        push_info(
            app,
            "available providers: opencode, openai-codex, anthropic".to_string(),
        );
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
        Some(provider) => match app.config.switch_provider(&provider, true) {
            Ok(()) => {
                let _ = app.session.set_state("provider", provider.name());
                push_info(app, format!("switched to provider: {}", provider.name()));
            }
            Err(error) => push_info(app, format!("could not switch provider: {}", error)),
        },
        None => push_info(
            app,
            format!("unknown provider: {name}; use opencode, openai-codex or a providers: entry"),
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
