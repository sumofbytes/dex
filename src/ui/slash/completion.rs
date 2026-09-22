use super::super::App;
use super::super::InputField;
use super::parser::COMMANDS;
use crate::session::Session;
use std::path::PathBuf;

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
    /// Configured `providers:` keys feed the `/provider` picker, so a
    /// config edit without a model-list change must still miss.
    pub(crate) providers_hash: u64,
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
        providers_hash: str_list_hash(
            &app.config
                .provider_entries
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
        ),
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
        let filtered: Vec<String> = app
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
            .cloned()
            .collect();
        // One catalog pass for every visible row: provider attribution
        // (bare ids resolve to their cheapest server) plus `$in/$out` cost.
        let hints = crate::llm::config::model_hints_for(&filtered);
        return filtered
            .into_iter()
            .zip(hints)
            .map(|(model, hint)| {
                let is_current =
                    model == app.config.model || model.ends_with(&format!("/{}", app.config.model));
                let provider = match model.split_once('/') {
                    Some((prov, _)) => Some(prov.to_string()),
                    None => hint.provider.clone(),
                };
                let mut desc = if is_current {
                    "Current model".to_string()
                } else {
                    provider.clone().unwrap_or_default()
                };
                if let Some(prov) = provider {
                    if is_current && !prov.is_empty() {
                        desc.push_str(&format!(" · {prov}"));
                    }
                }
                if let Some(cost) = hint.cost {
                    if desc.is_empty() {
                        desc = cost;
                    } else {
                        desc.push_str(&format!(" · {cost}"));
                    }
                }
                (format!("/model {model}"), desc)
            })
            .collect();
    }
    if let Some(query) = input.strip_prefix("/provider ") {
        let query = query.to_ascii_lowercase();
        // Builtins plus every configured `providers:` entry plus the
        // current selection, so a configured generic name is completable.
        // The `codex` alias resolves to `openai-codex` downstream.
        let mut names: std::collections::BTreeSet<String> = crate::protocol::Provider::BUILTINS
            .iter()
            .map(|s| s.to_string())
            .collect();
        names.extend(app.config.provider_entries.keys().cloned());
        names.insert(app.config.provider.name().to_string());
        let current = app.config.provider.name();
        return names
            .into_iter()
            .filter(|provider| provider.starts_with(&query))
            .map(|provider| {
                let desc = if provider == current {
                    "Current provider".to_string()
                } else if app.config.provider_entries.contains_key(&provider) {
                    "configured".to_string()
                } else {
                    "built-in".to_string()
                };
                (format!("/provider {provider}"), desc)
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
