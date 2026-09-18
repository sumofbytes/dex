use super::super::push_info;
use super::super::slash::handle_slash;
use super::super::slash::reset_session_state;
use super::boot::push_skills_listing;
use super::input::find_local_session_file;
use super::input::no_paint;
use super::input::rebuild_remote_from_messages;
use super::input::replay_remote_events;
use super::state::RemoteApp;
use crate::core::types::PermissionMode;
use crate::session::Session;

/// Slash commands for remote mode. Locally-answered commands are handled
/// here; everything else defers to the shared `slash` module's `handle_slash`
/// (with the client-side config deltas the daemon needs re-synced). Returns
/// true when the app should quit.
pub(crate) fn handle_remote_slash(remote: &mut RemoteApp, line: &str) -> bool {
    use super::super::slash::{cmd_help, parse, SlashCommand};

    match parse(line) {
        SlashCommand::Quit => return true,
        SlashCommand::Clear => remote_reset(remote, false),
        SlashCommand::New => remote_reset(remote, true),
        SlashCommand::Session => {
            let id = remote.session_id.clone();
            push_info(&mut remote.app, format!("session: {id} (on daemon)"));
        }
        SlashCommand::Undo => remote_undo(remote),
        SlashCommand::Mcp(arg) => remote_mcp(remote, arg),
        SlashCommand::Help => cmd_help(&mut remote.app, true),
        SlashCommand::Skill(name) => remote_skill(remote, name),
        SlashCommand::Waive(reason) => remote_waive(remote, reason),
        SlashCommand::Name(name) => remote_name(remote, name),
        SlashCommand::Thinking(arg) => remote_thinking(remote, arg),
        SlashCommand::Extension(full) => {
            let owned = full.to_string();
            remote_extension_line(remote, &owned);
        }
        SlashCommand::Provider(Some(name)) if !name.trim().is_empty() => push_info(
            &mut remote.app,
            "provider is configured on the daemon host; not switchable from a remote client"
                .to_string(),
        ),
        SlashCommand::Resume(selector) => remote_resume(remote, selector),
        SlashCommand::Unknown => {
            if !remote_unknown_or_extension(remote, line) {
                push_info(&mut remote.app, format!("unknown command: {line}"));
            }
        }
        _ => {
            let had_model = remote.app.config.model.clone();
            let had_permission = remote.app.config.permission;
            let had_plan = remote.app.plan.clone();
            // Raw `/model <selection>` argument: the daemon routes endpoint
            // prefixes (`go/…`) against its own endpoint table, so it must
            // see the un-stripped selection — the client's `config.model` is
            // already resolved to the bare id.
            let raw_model = line.strip_prefix("/model ").map(|s| s.trim().to_string());
            let quit = handle_slash(&mut remote.app, line);
            if remote.app.config.model != had_model {
                remote.options.model = Some(match raw_model {
                    Some(raw) if !raw.is_empty() => raw,
                    _ => remote.app.config.model.clone(),
                });
                // A new endpoint+model has its own effort: drop the old
                // override so the next turn uses the daemon default instead
                // of pinning the previous model's level.
                remote.options.thinking_effort = None;
            }
            if remote.app.config.permission != had_permission {
                remote.options.permission = Some(match remote.app.config.permission {
                    PermissionMode::ReadOnly => "read-only".to_string(),
                    PermissionMode::AskWrites => "ask-writes".to_string(),
                    PermissionMode::AskShell => "ask-shell".to_string(),
                    PermissionMode::Trusted => "trusted".to_string(),
                });
            }
            if remote.app.plan != had_plan {
                if remote.app.plan.is_empty() {
                    remote.options.plan = Some(String::new());
                } else {
                    remote.options.plan = Some(remote.app.plan.to_json());
                }
            }
            return quit;
        }
    }
    false
}

/// `/clear` and `/new` both start a fresh daemon session (the daemon owns the
/// conversation); `new_session` only picks the wording.
fn remote_reset(remote: &mut RemoteApp, new_session: bool) {
    if remote.app.busy {
        let what = if new_session {
            "start a new session"
        } else {
            "clear history"
        };
        push_info(
            &mut remote.app,
            format!("cannot {what} while a turn is running."),
        );
        return;
    }
    let label = if new_session {
        "new session started."
    } else {
        "history cleared."
    };
    let cwd = remote.app.cwd.clone();
    let name = Session::default_session_name(&cwd);
    match remote.client.create_session(&cwd, Some(&name)) {
        Ok(session) => {
            remote.session_id = session.session_id;
            reset_session_state(&mut remote.app);
            remote.options.plan = None;
            let mut fresh = Session::in_memory(cwd);
            fresh.set_name(name).ok();
            remote.app.session = fresh;
            push_info(&mut remote.app, label.to_string());
            push_skills_listing(&mut remote.app);
        }
        Err(e) => push_info(&mut remote.app, format!("could not start new session: {e}")),
    }
}

fn remote_undo(remote: &mut RemoteApp) {
    let sid = remote.session_id.clone();
    match remote.client.undo(&sid) {
        Ok(true) => push_info(&mut remote.app, "last change undone.".to_string()),
        Ok(false) | Err(_) => push_info(
            &mut remote.app,
            "nothing to undo (or undo refused).".to_string(),
        ),
    }
}

fn remote_mcp(remote: &mut RemoteApp, arg: Option<&str>) {
    match arg {
        Some("help") => {
            // Same grammar as local `/mcp`: only `help` is valid; anything
            // else is usage (login itself runs in the CLI on the daemon host).
            push_info(
                &mut remote.app,
                "/mcp shows server status including auth.".to_string(),
            );
            push_info(
                &mut remote.app,
                "MCP OAuth runs in the CLI: `dex mcp login <server>` (on the daemon host when remote).".to_string(),
            );
        }
        Some(_) => push_info(&mut remote.app, "usage: /mcp [help]".to_string()),
        None => {
            // Explicit arm (not the `handle_slash` fallthrough below): the
            // daemon owns the MCP connections, so status must come from
            // `/api/mcp` — the client process's own manager was never
            // bootstrapped and would render an empty list.
            match remote.client.mcp_status() {
                Ok(body) => {
                    let statuses: Vec<crate::mcp::ServerStatus> = body["servers"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .map(|v| crate::mcp::ServerStatus {
                                    name: v["name"].as_str().unwrap_or("?").to_string(),
                                    state: v["state"].as_str().unwrap_or("down").to_string(),
                                    tools: v["tools"].as_u64().unwrap_or(0) as usize,
                                    error: v["error"].as_str().map(|s| s.to_string()),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    // No per-tool detail over the wire yet: headers + errors.
                    let truncated = body["truncated"].as_u64().unwrap_or(0) as usize;
                    for line in crate::core::format::render_mcp_panel(&statuses, &[], truncated) {
                        push_info(&mut remote.app, line);
                    }
                    // Auth rides the same body (`auth`, null for stdio) so a
                    // remote TUI never needs the daemon host's token files.
                    for line in crate::client::http::mcp_auth_lines(&body) {
                        push_info(&mut remote.app, line);
                    }
                }
                Err(e) => push_info(&mut remote.app, format!("could not fetch MCP status: {e}")),
            }
        }
    }
}

fn remote_skill(remote: &mut RemoteApp, name: Option<&str>) {
    let name = name.unwrap_or("").trim().to_string();
    if name.is_empty() {
        push_info(&mut remote.app, "usage: /skill:<name>".to_string());
        return;
    }
    let sid = remote.session_id.clone();
    let dirs = remote.options.skill_dirs.clone();
    match remote.client.load_skill(&sid, &name, &dirs) {
        Ok(resp) => {
            push_info(&mut remote.app, format!("loaded skill: {}", resp.name));
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("404") {
                push_info(&mut remote.app, format!("skill not found: {name}"));
                let needs_refresh = remote.app.skills.is_empty();
                if needs_refresh {
                    if let Ok(fresh) = remote.client.list_skills() {
                        remote.app.skills = fresh
                            .into_iter()
                            .map(|info| crate::core::types::Skill {
                                name: info.name,
                                description: info.description,
                                path: std::path::PathBuf::from(""),
                            })
                            .collect();
                    }
                }
                let names: Vec<String> = remote.app.skills.iter().map(|s| s.name.clone()).collect();
                if !names.is_empty() {
                    push_info(&mut remote.app, "available skills:".to_string());
                    for n in names {
                        push_info(&mut remote.app, format!("  - {n}"));
                    }
                }
            } else {
                push_info(
                    &mut remote.app,
                    format!("could not load skill '{name}': {e}"),
                );
            }
        }
    }
}

fn remote_waive(remote: &mut RemoteApp, reason: Option<&str>) {
    let reason = reason.unwrap_or("").trim().to_string();
    if reason.is_empty() {
        push_info(&mut remote.app, "usage: /waive <reason>".to_string());
        return;
    }
    let sid = remote.session_id.clone();
    match remote.client.waive(&sid, &reason) {
        Ok(()) => push_info(
            &mut remote.app,
            "verification waived (recorded for this session).".to_string(),
        ),
        Err(e) => push_info(
            &mut remote.app,
            format!("could not waive verification: {e}"),
        ),
    }
}

fn remote_name(remote: &mut RemoteApp, name: Option<&str>) {
    let name = name.unwrap_or("").trim().to_string();
    if name.is_empty() {
        push_info(&mut remote.app, "usage: /name <name>".to_string());
        return;
    }
    let sid = remote.session_id.clone();
    match remote.client.rename_session(&sid, &name) {
        Ok(()) => push_info(&mut remote.app, format!("session name: {name}")),
        Err(e) => push_info(&mut remote.app, format!("could not rename: {e}")),
    }
}

/// Remote `/thinking`: same grammar as local, but the choice lives on the
/// daemon (it runs the turns). Never touches the client's
/// `thinking-effort.json` — that file is keyed by `base_url|model` and the
/// display copy's `base_url` is empty, so a local write would key wrong and
/// the daemon would silently ignore it. Instead the display copy and the
/// per-turn override move together, so what `/thinking` shows is what the
/// next turn uses. `Some("")` is the explicit-clear override (unset).
fn remote_thinking(remote: &mut RemoteApp, arg: Option<&str>) {
    let model = remote.app.config.model.clone();
    let Some(arg) = arg else {
        let advertised = crate::llm::config::reasoning_options_for(&model);
        let effort = remote.app.config.thinking_effort.clone();
        let text = match (&effort, advertised) {
            (Some(effort), Some(options)) => {
                format!(
                    "thinking effort: {effort} (options: {})",
                    options.join(", ")
                )
            }
            (Some(effort), None) => format!("thinking effort: {effort}"),
            (None, Some(options)) => {
                format!("thinking effort: unset (options: {})", options.join(", "))
            }
            (None, None) => "thinking effort: unset".to_string(),
        };
        push_info(&mut remote.app, text);
        return;
    };
    let arg = arg.trim();
    if arg.is_empty() {
        push_info(
            &mut remote.app,
            "usage: /thinking <level>|clear".to_string(),
        );
    } else if ["clear", "auto", "off"]
        .iter()
        .any(|w| w.eq_ignore_ascii_case(arg))
    {
        remote.app.config.thinking_effort = None;
        remote.options.thinking_effort = Some(String::new());
        push_info(&mut remote.app, "thinking effort cleared".to_string());
    } else {
        match crate::llm::config::validate_thinking_effort(&model, arg) {
            Ok(level) => {
                remote.app.config.thinking_effort = Some(level.clone());
                remote.options.thinking_effort = Some(level.clone());
                push_info(
                    &mut remote.app,
                    format!("thinking effort: {level} for {model}"),
                );
            }
            Err(options) => push_info(
                &mut remote.app,
                format!(
                    "unknown thinking effort '{arg}' for {model} (options: {})",
                    options.join(", ")
                ),
            ),
        }
    }
}

/// Split `/<name> [args]` without consulting the client's extension list
/// (empty when remote — the daemon owns the manager). `None` when the line
/// is not a plausible command word, so built-in usage errors keep their
/// local messages instead of paying a daemon round trip.
pub(crate) fn split_remote_extension(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix('/')?;
    if rest.contains('\n') {
        return None;
    }
    let mut parts = rest.splitn(2, ' ');
    let name = parts.next()?.trim().to_string();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let arg = parts.next().unwrap_or("").trim().to_string();
    Some((name, arg))
}

/// Built-in command words: an `Unknown` line naming one is a usage error,
/// never a daemon extension (avoids a round trip for typos like `/quit x`).
pub(crate) fn is_builtin_command(name: &str) -> bool {
    matches!(
        name,
        "quit"
            | "clear"
            | "new"
            | "session"
            | "permissions"
            | "resume"
            | "name"
            | "model"
            | "provider"
            | "thinking"
            | "waive"
            | "undo"
            | "mcp"
            | "extensions"
            | "help"
            | "skill"
    ) || name.starts_with("skill:")
}

/// Run one extension command on the daemon. Returns true when the daemon
/// knew the command (dispatched or handler-failed — both are answered, not
/// unknown); false on 404 so the caller falls back to "unknown command".
fn remote_extension_run(remote: &mut RemoteApp, name: &str, arg: &str) -> bool {
    match remote.client.extensions_run(name, arg) {
        Ok(body) => {
            if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
                push_info(&mut remote.app, format!("command '/{name}' failed: {err}"));
            } else {
                let ext = body
                    .get("extension")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                push_info(
                    &mut remote.app,
                    format!("command '{name}@{ext}' dispatched (output → log)"),
                );
                if let Some(out) = body.get("output").and_then(|v| v.as_str()) {
                    eprintln!("dex: [extensions] /{name}: {out}");
                }
            }
            true
        }
        Err(e) => {
            if e.to_string().contains("404") {
                false
            } else {
                push_info(
                    &mut remote.app,
                    format!("could not run command '/{name}': {e}"),
                );
                true
            }
        }
    }
}

/// An `Extension` line from the shared parser (client list non-empty when
/// co-located): still dispatch on the daemon, which owns the workspace.
fn remote_extension_line(remote: &mut RemoteApp, line: &str) {
    match split_remote_extension(line) {
        Some((name, arg)) => {
            if !remote_extension_run(remote, &name, &arg) {
                push_info(&mut remote.app, format!("unknown command: {line}"));
            }
        }
        None => push_info(&mut remote.app, format!("unknown command: {line}")),
    }
}

/// An `Unknown` line may be a daemon extension the client's empty list could
/// not classify. Returns true when handled (dispatched or answered); false
/// when still unknown so the caller prints "unknown command".
pub(crate) fn remote_unknown_or_extension(remote: &mut RemoteApp, line: &str) -> bool {
    let Some((name, arg)) = split_remote_extension(line) else {
        return false;
    };
    if is_builtin_command(&name) {
        return false;
    }
    remote_extension_run(remote, &name, &arg)
}

/// `/resume` lists daemon sessions (local files as an offline fallback);
/// `/resume <index|id>` reattaches. `selector` is `None` for the bare form.
fn remote_resume(remote: &mut RemoteApp, selector: Option<&str>) {
    let Some(selector) = selector.filter(|s| !s.trim().is_empty()) else {
        // Prefer daemon listing (works over network); fall back to local files for offline.
        let daemon_sessions = remote.client.list_sessions().ok();
        if let Some(mut sessions) = daemon_sessions {
            sessions.retain(|s| {
                s.cwd == remote.app.cwd && s.session_id != remote.session_id && s.message_count > 0
            });
            if sessions.is_empty() {
                push_info(&mut remote.app, "no sessions found.".to_string());
            } else {
                push_info(&mut remote.app, "sessions:".to_string());
                for (i, s) in sessions.iter().enumerate() {
                    let name = s.name.as_deref().unwrap_or("(unnamed)");
                    let mut row = format!("  {}: {} ({})", i, name, s.session_id);
                    if s.child_agents > 0 {
                        row.push_str(&format!(" · {} agent run(s)", s.child_agents));
                    }
                    if s.interrupted_children > 0 {
                        row.push_str(&format!(" · {} interrupted", s.interrupted_children));
                    }
                    push_info(&mut remote.app, row);
                }
                push_info(
                    &mut remote.app,
                    "use /resume <index|id> to resume (reattaches on daemon)".to_string(),
                );
            }
        } else {
            let sessions = Session::list(&remote.app.cwd).unwrap_or_default();
            let filtered: Vec<_> = sessions
                .into_iter()
                .filter(|(path, header)| {
                    if header.id() == remote.app.session.id() {
                        return false;
                    }
                    matches!(
                        crate::session::load_messages_from_session(path),
                        Ok(msgs) if !msgs.is_empty()
                    )
                })
                .collect();
            if filtered.is_empty() {
                push_info(&mut remote.app, "no sessions found.".to_string());
            } else {
                push_info(&mut remote.app, "sessions:".to_string());
                for (i, (path, header)) in filtered.iter().enumerate() {
                    let name = header.name().unwrap_or("(unnamed)");
                    push_info(
                        &mut remote.app,
                        format!("  {}: {} ({})", i, name, path.display()),
                    );
                }
                push_info(
                    &mut remote.app,
                    "use /resume <index|id> to resume (reattaches on daemon)".to_string(),
                );
            }
        }
        return;
    };
    if remote.app.busy {
        push_info(
            &mut remote.app,
            "cannot resume a session while a turn is running.".to_string(),
        );
        return;
    }
    let selector = selector.trim().to_string();
    // Resolve selector to a daemon session_id + server-side path: index
    // or id prefix. Filter the same way as the listing so indices line
    // up. The daemon path doubles as the local file when co-located.
    let resolved: Option<(String, Option<String>)> = remote
        .client
        .list_sessions()
        .ok()
        .and_then(|mut sessions| {
            sessions.retain(|s| {
                s.cwd == remote.app.cwd && s.session_id != remote.session_id && s.message_count > 0
            });
            if let Ok(idx) = selector.parse::<usize>() {
                return sessions
                    .get(idx)
                    .map(|s| (s.session_id.clone(), Some(s.path.clone())));
            }
            // prefix or exact id/name match
            let q = selector.to_ascii_lowercase();
            sessions
                .iter()
                .find(|s| {
                    s.session_id.to_ascii_lowercase().starts_with(&q)
                        || s.name
                            .as_deref()
                            .unwrap_or("")
                            .to_ascii_lowercase()
                            .contains(&q)
                })
                .map(|s| (s.session_id.clone(), Some(s.path.clone())))
        })
        .or_else(|| {
            let sessions = Session::list(&remote.app.cwd).unwrap_or_default();
            let filtered: Vec<_> = sessions
                .into_iter()
                .filter(|(path, header)| {
                    if header.id() == remote.app.session.id() {
                        return false;
                    }
                    matches!(
                        crate::session::load_messages_from_session(path),
                        Ok(msgs) if !msgs.is_empty()
                    )
                })
                .collect();
            if let Ok(idx) = selector.parse::<usize>() {
                return filtered.get(idx).and_then(|(p, _)| {
                    // Resolve id from file header for local fallback.
                    crate::session::Session::from_path(p)
                        .ok()
                        .map(|s| (s.id().to_string(), Some(p.display().to_string())))
                });
            }
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
                .and_then(|(p, _)| crate::session::Session::from_path(p).ok())
                .map(|s| {
                    let path = s.path().map(|p| p.display().to_string());
                    (s.id().to_string(), path)
                })
        });
    let Some((sid, daemon_path)) = resolved else {
        push_info(
            &mut remote.app,
            format!("could not resume session: {selector} not found"),
        );
        return;
    };
    // Local JSONL when files are shared (default co-located daemon):
    // daemon path first, then an id lookup (Session::resume only
    // handles index/path, so an id through it silently misses).
    let local_path: Option<std::path::PathBuf> = daemon_path
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_file())
        .or_else(|| find_local_session_file(&sid));
    // Reattach before touching the transcript: a failed reattach must
    // leave the current view intact instead of blanking it.
    match remote.client.reattach(&sid) {
        Ok(resp) => {
            remote.session_id = resp.session_id.clone();
            // Full per-session reset (transcript, usage, plan, scroll,
            // pending steering/approvals) so the old conversation
            // doesn't leak into the resumed one.
            reset_session_state(&mut remote.app);
            remote.options.plan = None;
            if let Some(p) = local_path.as_deref() {
                if let Ok(s) = Session::from_path(p) {
                    remote.app.session = s;
                }
            }
            // Prefer JSONL messages (complete, includes user prompts
            // the events journal never records); replay events only
            // when no local file is available (true remote).
            let mut rebuilt = false;
            if let Some(p) = local_path.as_deref() {
                rebuilt = rebuild_remote_from_messages(remote, p, no_paint);
            }
            if !rebuilt {
                let sid = remote.session_id.clone();
                replay_remote_events(remote, &sid, no_paint);
                if remote.app.transcript.is_empty() && remote.app.assistant_pending.is_empty() {
                    push_info(
                        &mut remote.app,
                        "resumed session has no replayable history.".to_string(),
                    );
                }
            }
            push_info(
                &mut remote.app,
                format!("resumed session: {}", remote.session_id),
            );
        }
        Err(e) => push_info(&mut remote.app, format!("could not reattach session: {e}")),
    }
}
