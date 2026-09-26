//! Extension discovery: directory scopes, consent markers, enable/disable.

use std::path::PathBuf;
use std::sync::OnceLock;

use super::global::loaded_summaries;
use super::manifest::{self, Manifest};

/// Walk the discovery dirs and collect consent-passing (dir, manifest)
/// pairs. Pure disk read — the load/reload paths share it.
pub(crate) fn discover_scoped() -> Vec<(PathBuf, Manifest)> {
    let mut found: Vec<(PathBuf, Manifest)> = Vec::new();
    for (dir, scope) in scoped_extension_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let ext_dir = entry.path();
            if !ext_dir.is_dir() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(ext_dir.join("manifest.yaml")) else {
                continue;
            };
            let Ok(m) = manifest::parse_manifest(&text) else {
                continue;
            };
            match scope {
                Scope::Project if !is_enabled(&m.id) => {
                    eprintln!(
                        "dex: [extensions] skip {}: project extension not enabled (run `dex extensions enable {}`)",
                        ext_dir.display(),
                        m.id
                    );
                    continue;
                }
                Scope::User if is_disabled(&m.id) => {
                    eprintln!("dex: [extensions] skip {}: disabled", ext_dir.display());
                    continue;
                }
                _ => {}
            }
            found.push((ext_dir, m));
        }
    }
    found
}

/// Extra extension dirs from `--extensions-dir` (mirrors `--skill-dir`).
/// Set once from CLI args before the manager initializes; the daemon reads
/// it at bootstrap, one-shot runs set it before their in-process turn.
static EXTRA_DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();

pub fn set_extra_dirs(dirs: Vec<PathBuf>) {
    let _ = EXTRA_DIRS.set(dirs);
}

/// Extra extension dirs from config/env, in precedence order: env
/// `DEX_EXTENSIONS_PATHS` (`:`-separated) wins over the file's
/// `extensions.paths:` (same layering as every other knob).
pub fn config_extension_paths() -> Vec<PathBuf> {
    if let Ok(raw) = std::env::var("DEX_EXTENSIONS_PATHS") {
        return raw
            .split(':')
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .collect();
    }
    crate::llm::config::config_file_value()
        .as_ref()
        .map(parse_config_paths)
        .unwrap_or_default()
}

/// Parse `extensions.paths:` (a list of dirs) out of a config file value.
/// Non-string entries are skipped; the key existing with no paths is fine.
pub(crate) fn parse_config_paths(root: &serde_yaml::Value) -> Vec<PathBuf> {
    root.get("extensions")
        .and_then(|e| e.get("paths"))
        .and_then(|p| p.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str())
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Discovery with scope: `Project` dirs (cwd-relative) carry third-party
/// code the workspace ships, so they load only with an explicit user
/// consent marker (`dex extensions enable <id>`); `User` dirs (XDG config,
/// CLI extras) load unless explicitly disabled. Same contract as skills
/// otherwise: sorted + deduped, hook order deterministic.
pub(crate) fn scoped_extension_dirs() -> Vec<(PathBuf, Scope)> {
    let mut dirs: Vec<(PathBuf, Scope)> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push((cwd.join(".dex/extensions"), Scope::Project));
        dirs.push((cwd.join(".agents/extensions"), Scope::Project));
    }
    dirs.push((user_extensions_dir(), Scope::User));
    for path in config_extension_paths() {
        dirs.push((path, Scope::User));
    }
    if let Some(extra) = EXTRA_DIRS.get() {
        dirs.extend(extra.iter().cloned().map(|d| (d, Scope::User)));
    }
    // Project scope first, then path: a duplicate id across scopes resolves
    // project-first (the consent-gated one wins), not by path accident.
    dirs.sort_by_key(|(path, scope)| (scope_rank(*scope), path.clone()));
    dirs.dedup_by(|a, b| a.0 == b.0);
    dirs
}

fn scope_rank(scope: Scope) -> u8 {
    match scope {
        Scope::Project => 0,
        Scope::User => 1,
    }
}

/// The user-scope install target: `$XDG_CONFIG_HOME/dex/extensions`.
pub(crate) fn user_extensions_dir() -> PathBuf {
    if let Some(cfg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(cfg).join("dex/extensions");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config/dex/extensions");
    }
    PathBuf::from(".config/dex/extensions")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Scope {
    /// cwd-relative: loads only with an `enabled/<id>` consent marker.
    Project,
    /// XDG config / CLI extras: loads unless `disabled/<id>` exists.
    User,
}

/// `$XDG_DATA_HOME/dex/extensions` (marker-file home; no new persistence
/// design — plan §9).
pub(crate) fn data_extensions_dir() -> PathBuf {
    crate::runtime::logging::data_home()
        .map(|base| base.join("dex/extensions"))
        .unwrap_or_else(|| PathBuf::from(".dex/extensions"))
}

fn marker_path(kind: &str, id: &str) -> PathBuf {
    data_extensions_dir().join(kind).join(id)
}

pub(crate) fn is_disabled(id: &str) -> bool {
    marker_path("disabled", id).is_file()
}

pub(crate) fn is_enabled(id: &str) -> bool {
    marker_path("enabled", id).is_file()
}

/// `dex extensions enable|disable <id>`: write/remove the marker files.
/// Enabling a project-scope extension IS the trust consent.
pub fn set_enabled(id: &str, enabled: bool) -> std::io::Result<()> {
    // CLI-supplied id: reject traversal/shapes that would escape the marker
    // or data dirs (the manifest validator guarantees stored ids are safe).
    if !manifest::valid_segment(id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid extension id '{id}': use [a-z0-9_-]+, max 64 chars, no `__`"),
        ));
    }
    let enabled_path = marker_path("enabled", id);
    let disabled_path = marker_path("disabled", id);
    if enabled {
        std::fs::create_dir_all(enabled_path.parent().expect("parent"))?;
        std::fs::write(&enabled_path, b"")?;
        std::fs::remove_file(&disabled_path).ok();
    } else {
        std::fs::create_dir_all(disabled_path.parent().expect("parent"))?;
        std::fs::write(&disabled_path, b"")?;
        std::fs::remove_file(&enabled_path).ok();
    }
    Ok(())
}

/// Disk discovery for `dex doctor`: (id, version, scope, consent state)
/// per found manifest. Reads no manager state, so it is deterministic
/// regardless of what parallel test runs loaded.
pub fn discovered_extensions() -> Vec<(String, String, &'static str, String)> {
    let mut found = Vec::new();
    for (dir, scope) in scoped_extension_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let ext_dir = entry.path();
            if !ext_dir.is_dir() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(ext_dir.join("manifest.yaml")) else {
                continue;
            };
            let Ok(m) = manifest::parse_manifest(&text) else {
                continue;
            };
            let scope = match scope {
                Scope::Project => "project",
                Scope::User => "user",
            };
            let state = if is_disabled(&m.id) {
                "disabled"
            } else if scope == "project" && !is_enabled(&m.id) {
                "not enabled (trust gate)"
            } else {
                "enabled"
            };
            found.push((m.id, m.version, scope, state.to_string()));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
    found
}

/// Extensions that may arbitrate approvals: loaded (user scope not
/// disabled / project scope explicitly enabled) and declaring the
/// `harness` capability — the enforced gate for `permission.request` and
/// `supervisor.route` subscriptions. Subscriptions live in the Lua, not
/// the manifest, so the capability is the proxy `dex doctor` can read
/// without booting a worker — a Lua permission hook must never be
/// invisible next to the configured permission mode.
pub fn permission_hook_ids() -> Vec<String> {
    let mut ids = Vec::new();
    for (dir, scope) in scoped_extension_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let ext_dir = entry.path();
            if !ext_dir.is_dir() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(ext_dir.join("manifest.yaml")) else {
                continue;
            };
            let Ok(m) = manifest::parse_manifest(&text) else {
                continue;
            };
            if !m.has_capability("harness") {
                continue;
            }
            let live = match scope {
                Scope::User => !is_disabled(&m.id),
                Scope::Project => is_enabled(&m.id),
            };
            if live {
                ids.push(m.id);
            }
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

/// One-line summary for `dex extensions list` / `/extensions` (shared so
/// the two surfaces never drift).
pub fn summary_line(id: &str, version: &str, tools: &[String], events: &[String]) -> String {
    format!(
        "{id} {version} — {} tool(s), events: {}",
        tools.len(),
        if events.is_empty() {
            "-".to_string()
        } else {
            events.join(",")
        }
    )
}

/// `dex extensions list`: discovery walk + consent state + loaded summary.
/// The caller refreshes the manager first (one-shot blocks on it).
pub fn list_command() {
    for (dir, scope) in scoped_extension_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let ext_dir = entry.path();
            let text = match std::fs::read_to_string(ext_dir.join("manifest.yaml")) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let m = match manifest::parse_manifest(&text) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let scope = match scope {
                Scope::Project => "project",
                Scope::User => "user",
            };
            let state = if is_disabled(&m.id) {
                "disabled"
            } else if scope == "project" && !is_enabled(&m.id) {
                "not enabled (trust gate)"
            } else {
                "enabled"
            };
            let loaded = loaded_summaries()
                .into_iter()
                .find(|(id, _, _, _)| *id == m.id)
                .map(|(_, _, tools, events)| {
                    format!(
                        "loaded, {}",
                        summary_line(&m.id, &m.version, &tools, &events)
                    )
                })
                .unwrap_or_else(|| "not loaded".to_string());
            println!(
                "{:<14} {:<8} {:<6} {:<28} {}",
                m.id, m.version, scope, state, loaded
            );
            println!("  {}", ext_dir.display());
        }
    }
}
