//! Lua harness extensions: directory discovery, process-global manager.
//!
//! Mirrors `mcp.rs`: loaded once at daemon bootstrap (not per turn), with
//! `cached_tools()` / `cached_schema_tokens()` / `call_global()` sync
//! surfaces for the schema + dispatch paths. A failed extension is skipped
//! whole — its tools never enter the schema. See
//! `docs/lua-extensions-plan.md` §§6/9/11 (P0).

mod engine;
pub(crate) mod hooks;
mod manifest;

pub(crate) use engine::{CallKind, ExtensionEngine, HostCtx, ShadowCtx, HOOK_TIMEOUT_SECS};

/// [`HOOK_TIMEOUT_SECS`] as a `Duration` for the event drive loop.
pub(crate) const HOOK_TIMEOUT_SECS_DURATION: std::time::Duration =
    std::time::Duration::from_secs(HOOK_TIMEOUT_SECS);
pub(crate) use hooks::{AfterOutcome, BeforeOutcome, CompactAction};
pub(crate) use manifest::Manifest;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::core::types::{FunctionDef, ToolDefinition};

/// Walk the discovery dirs and collect consent-passing (dir, manifest)
/// pairs. Pure disk read — the load/reload paths share it.
fn discover_scoped() -> Vec<(PathBuf, Manifest)> {
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

pub(crate) fn set_extra_dirs(dirs: Vec<PathBuf>) {
    let _ = EXTRA_DIRS.set(dirs);
}

/// Extra extension dirs from config/env, in precedence order: env
/// `DEX_EXTENSIONS_PATHS` (`:`-separated) wins over the file's
/// `extensions.paths:` (same layering as every other knob).
pub(crate) fn config_extension_paths() -> Vec<PathBuf> {
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
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(dir).join("dex/extensions");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".local/share/dex/extensions");
    }
    PathBuf::from(".dex/extensions")
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
pub(crate) fn set_enabled(id: &str, enabled: bool) -> std::io::Result<()> {
    // CLI-supplied id: reject traversal/shapes that would escape the marker
    // or data dirs (the manifest validator guarantees stored ids are safe).
    if !manifest::valid_segment(id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid extension id '{id}': use [a-z0-9_-]+, max 64 chars"),
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

struct LoadedExtension {
    manifest: Manifest,
    engine: ExtensionEngine,
    /// Full `lua__<ext>__<tool>` names, sorted (shadows excluded: they ride
    /// the built-in name, never a `lua__` one).
    tools: Vec<String>,
    /// Subscribed event names, sorted.
    events: Vec<String>,
    /// Shadowed built-in names.
    shadows: Vec<String>,
    /// Registered slash commands: (name, description), sorted by name.
    commands: Vec<(String, String)>,
}

pub(crate) struct ExtensionManager {
    cached: tokio::sync::RwLock<Vec<ToolDefinition>>,
    /// Sorted by id: iteration order is hook order (same contract as the
    /// skills dedup — deterministic, first-wins on collision).
    engines: tokio::sync::RwLock<BTreeMap<String, LoadedExtension>>,
    /// Built-in names currently shadowed (sync read for `metadata()`).
    shadowed: tokio::sync::RwLock<HashSet<String>>,
    /// `dex.tools.set_active` slice: `None` = all extension tools, `Some`
    /// = exactly these full names. Sync read on the schema path.
    active: std::sync::RwLock<Option<Vec<String>>>,
    /// Serializes refreshes: without it the background load and an
    /// explicit one (one-shot / `run`) race, boot two workers per
    /// extension, and log twice. The second refresh then no-ops on the
    /// already-present ids.
    refresh_lock: tokio::sync::Mutex<()>,
}

impl ExtensionManager {
    fn fresh() -> Self {
        Self {
            cached: tokio::sync::RwLock::new(Vec::new()),
            engines: tokio::sync::RwLock::new(BTreeMap::new()),
            shadowed: tokio::sync::RwLock::new(HashSet::new()),
            active: std::sync::RwLock::new(None),
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Load every extension found in `dirs`. Sequential: load order is
    /// sorted by id, failures skip one extension and continue.
    #[cfg(test)]
    pub(crate) async fn refresh_with(&self, dirs: &[PathBuf]) {
        let _guard = self.refresh_lock.lock().await;
        let mut found: Vec<(PathBuf, Manifest)> = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let ext_dir = entry.path();
                if !ext_dir.is_dir() {
                    continue;
                }
                let manifest_path = ext_dir.join("manifest.yaml");
                if !manifest_path.is_file() {
                    continue;
                }
                let text = match std::fs::read_to_string(&manifest_path) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("dex: [extensions] skip {}: {e}", ext_dir.display());
                        continue;
                    }
                };
                match manifest::parse_manifest(&text) {
                    Ok(m) => found.push((ext_dir, m)),
                    Err(e) => eprintln!("dex: [extensions] skip {}: {e}", ext_dir.display()),
                }
            }
        }
        found.sort_by(|a, b| a.1.id.cmp(&b.1.id));
        for (dir, m) in found {
            if self.engines.read().await.contains_key(&m.id) {
                continue;
            }
            if let Err(e) = self.load_one(&dir, m).await {
                eprintln!("dex: [extensions] skip {}: {e}", dir.display());
            }
        }
        self.rebuild_cache().await;
    }

    /// Load one extension: spawn the worker, run its chunk, validate the
    /// exports. Any failure skips the whole extension — nothing
    /// half-registered ever reaches the schema (§9).
    async fn load_one(&self, dir: &std::path::Path, m: Manifest) -> Result<(), String> {
        let source = std::fs::read_to_string(dir.join("extension.lua"))
            .map_err(|e| format!("extension '{}' has no extension.lua: {e}", m.id_for_error()))?;
        let engine = ExtensionEngine::new(m.clone(), dir.to_path_buf())
            .map_err(|e| format!("extension '{}': {e}", m.id_for_error()))?;
        let exports = engine.load(source).await?;

        // Shadows target real built-ins, claimed first-wins in load order.
        // A shadow colliding with an earlier shadow, or targeting a
        // non-built-in, rejects the whole extension.
        // Read-guard scope: tokio RwLock is not upgradeable, so the
        // duplicate checks must release the read guard before the insert
        // takes the write guard below.
        let commands = exports.commands.clone();
        let (tools, events) = {
            let engines = self.engines.read().await;
            for shadow in &exports.shadows {
                if crate::tools::metadata(shadow).is_none() {
                    return Err(format!(
                        "extension '{}' shadows unknown tool '{shadow}'",
                        m.id_for_error()
                    ));
                }
                if engines.values().any(|e| e.shadows.contains(shadow)) {
                    return Err(format!(
                        "extension '{}' shadows '{shadow}', already shadowed",
                        m.id_for_error()
                    ));
                }
            }
            let mut tools: Vec<String> = exports
                .tools
                .iter()
                .filter(|t| !exports.shadows.contains(t))
                .map(|t| full_tool_name(&m.id, t))
                .collect();
            tools.sort();
            // Collision with an earlier extension rejects the whole
            // latecomer — never silent rename.
            if let Some(dup) = tools
                .iter()
                .find(|t| engines.values().any(|e| e.tools.contains(t)))
            {
                return Err(format!("tool '{dup}' already registered"));
            }
            let mut events = exports.events;
            events.sort();
            (tools, events)
        };
        self.engines.write().await.insert(
            m.id.clone(),
            LoadedExtension {
                manifest: m,
                engine,
                tools,
                events,
                shadows: exports.shadows,
                commands,
            },
        );
        Ok(())
    }

    /// Load from the default discovery dirs, honoring scope consent: a
    /// project-scope extension loads only when enabled (trust gate), a
    /// user-scope one unless disabled. Add-only: never unloads (the
    /// bootstrap/one-shot path — a fresh process has nothing stale, and a
    /// racing background refresh must not wipe a concurrent loader).
    pub(crate) async fn refresh(&self) {
        self.refresh_found(discover_scoped()).await;
    }

    /// Explicit reload (`/extensions reload`): like [`refresh`], plus the
    /// reconcile — extensions that vanished from disk or lost consent
    /// unload, so disable/remove takes effect in the running process
    /// without a restart.
    pub(crate) async fn reload(&self) {
        let found = discover_scoped();
        let discovered: HashSet<String> = found.iter().map(|(_, m)| m.id.clone()).collect();
        self.refresh_found(found).await;
        // Anything still loaded but not on disk/consented now unloads.
        self.unload_missing(&discovered).await;
        self.rebuild_cache().await;
    }

    /// The shared load core: discovery happens in the caller, the loop is
    /// refresh-serialized, new ids load, the cache rebuilds.
    async fn refresh_found(&self, found: Vec<(PathBuf, Manifest)>) {
        let _guard = self.refresh_lock.lock().await;
        let mut found = found;
        found.sort_by(|a, b| a.1.id.cmp(&b.1.id));
        for (dir, m) in found {
            if self.engines.read().await.contains_key(&m.id) {
                continue;
            }
            if let Err(e) = self.load_one(&dir, m).await {
                eprintln!("dex: [extensions] skip {}: {e}", dir.display());
            }
        }
        self.rebuild_cache().await;
    }

    /// Unload engines whose id is not in `keep`, dropping their prompt
    /// appendix entries. The reload reconcile half of `refresh`.
    async fn unload_missing(&self, keep: &HashSet<String>) {
        let stale: Vec<String> = {
            let engines = self.engines.read().await;
            engines
                .keys()
                .filter(|id| !keep.contains(*id))
                .cloned()
                .collect()
        };
        for id in &stale {
            self.engines.write().await.remove(id);
            remove_prompt_appendix(id);
        }
    }

    async fn rebuild_cache(&self) {
        let engines = self.engines.read().await;
        let mut tools = Vec::new();
        for ext in engines.values() {
            for full in &ext.tools {
                let short = full
                    .strip_prefix(&format!("lua__{}__", ext.manifest.id))
                    .unwrap_or(full);
                if let Some(declared) = ext.manifest.tools.iter().find(|t| t.name == short) {
                    tools.push(ToolDefinition {
                        tool_type: "function".to_string(),
                        function: FunctionDef {
                            name: full.clone(),
                            description: declared.description.clone(),
                            parameters: declared.parameters.clone(),
                        },
                    });
                }
            }
        }
        tools.sort_by(|a, b| a.function.name.cmp(&b.function.name));
        // Overflow shares the MCP schema budget policy: truncate with the
        // count recorded. (P0: truncate; the count surfaces in P2 doctor.)
        let max = max_extension_tools();
        if tools.len() > max {
            tools.truncate(max);
        }
        *self.cached.write().await = tools;
        let shadowed: HashSet<String> = engines
            .values()
            .flat_map(|e| e.shadows.iter().cloned())
            .collect();
        *self.shadowed.write().await = shadowed;
    }

    /// Dispatch `lua__<ext>__<tool>`: run the tool on its worker, answering
    /// nested `dex.tools.call` with the caller's gates.
    async fn call(
        &self,
        full_name: &str,
        args: &serde_json::Map<String, serde_json::Value>,
        host: &HostCtx<'_>,
    ) -> Result<String, String> {
        let (ext_id, tool) =
            split_lua_name(full_name).ok_or_else(|| format!("unknown tool '{full_name}'"))?;
        // Clone the engine out of the lock: it is channel-based, so no Lua
        // state is held across the `.await` below.
        let (engine, timeout) = {
            let engines = self.engines.read().await;
            let Some(ext) = engines.get(ext_id) else {
                return Err(format!("unknown tool '{full_name}'"));
            };
            (ext.engine.clone(), ext.engine.tool_timeout(tool))
        };
        engine
            .drive(
                CallKind::Tool {
                    tool: tool.to_string(),
                },
                serde_json::Value::Object(args.clone()),
                timeout,
                host.cancel,
                *host,
                None,
            )
            .await
    }

    /// Dispatch a shadowed built-in: run the shadow on its worker. Nested
    /// `dex.tools.call` re-enters the full pipeline; `call_original`
    /// re-dispatches the shadowed built-in with the caller's gates (and the
    /// live shell-evidence slot, so wrapped `bash` still reports).
    async fn call_shadow(
        &self,
        target: &str,
        args: &serde_json::Map<String, serde_json::Value>,
        host: &HostCtx<'_>,
        shell_out: &mut Option<crate::tools::ShellEvidence>,
    ) -> Result<String, String> {
        let (engine, timeout) = {
            let engines = self.engines.read().await;
            let Some(ext) = engines
                .values()
                .find(|e| e.shadows.contains(&target.to_string()))
            else {
                return Err(format!("no shadow registered for '{target}'"));
            };
            (ext.engine.clone(), ext.engine.tool_timeout(target))
        };
        engine
            .drive(
                CallKind::Shadow {
                    target: target.to_string(),
                },
                serde_json::Value::Object(args.clone()),
                timeout,
                host.cancel,
                *host,
                Some(ShadowCtx { shell_out }),
            )
            .await
    }

    /// Run one extension's handlers for `event`, returning the directive
    /// envelope JSON. The manager does not interpret it — the typed runners
    /// in [`hooks`] do.
    async fn run_event(
        &self,
        ext_id: &str,
        event: &str,
        payload: serde_json::Value,
        host: &HostCtx<'_>,
    ) -> Result<String, String> {
        let engine = {
            let engines = self.engines.read().await;
            let Some(ext) = engines.get(ext_id) else {
                return Err(format!("unknown extension '{ext_id}'"));
            };
            if !ext.events.contains(&event.to_string()) {
                return Err(format!("extension '{ext_id}' has no '{event}' handlers"));
            }
            ext.engine.clone()
        };
        engine
            .drive(
                CallKind::Event {
                    event: event.to_string(),
                },
                payload,
                HOOK_TIMEOUT_SECS_DURATION,
                host.cancel,
                *host,
                None,
            )
            .await
    }

    /// `tool.before` chain (H1, plan §8): run in load order, each handler
    /// seeing prior rewrites (middleware). Deny short-circuits before any
    /// gate runs; handler errors fail open (skip) unless the extension
    /// declares `strict = true`, which denies instead.
    pub(crate) async fn apply_before_hooks(
        &self,
        tool: &str,
        args: &serde_json::Map<String, serde_json::Value>,
        host: &HostCtx<'_>,
    ) -> BeforeOutcome {
        let mut current = args.clone();
        let mut mutated_by = Vec::new();
        let subs: Vec<(String, bool)> = {
            let engines = self.engines.read().await;
            engines
                .values()
                .filter(|e| e.events.contains(&"tool.before".to_string()))
                .map(|e| (e.manifest.id.clone(), e.manifest.strict))
                .collect()
        };
        for (id, strict) in subs {
            let payload = serde_json::json!({"tool": tool, "args": current});
            let envelope = match self.run_event(&id, "tool.before", payload, host).await {
                Ok(json) => json,
                Err(error) => {
                    eprintln!("dex: [extensions] '{id}' tool.before failed: {error}");
                    if strict {
                        return BeforeOutcome::Denied {
                            by: id,
                            reason: format!("hook failed (strict): {error}"),
                        };
                    }
                    continue;
                }
            };
            let envelope: serde_json::Map<String, serde_json::Value> =
                match serde_json::from_str(&envelope) {
                    Ok(serde_json::Value::Object(map)) => map,
                    _ => {
                        eprintln!("dex: [extensions] '{id}' tool.before returned bad envelope");
                        if strict {
                            return BeforeOutcome::Denied {
                                by: id,
                                reason: "hook failed (strict): bad envelope".to_string(),
                            };
                        }
                        continue;
                    }
                };
            let (next, deny) = match hooks::parse_before(&envelope) {
                Ok(parsed) => parsed,
                Err(error) => {
                    eprintln!("dex: [extensions] '{id}' tool.before failed: {error}");
                    if strict {
                        return BeforeOutcome::Denied {
                            by: id,
                            reason: format!("hook failed (strict): {error}"),
                        };
                    }
                    continue;
                }
            };
            if let Some((_, reason)) = deny {
                eprintln!("dex: [extensions] '{id}' tool.before denied '{tool}': {reason}");
                return BeforeOutcome::Denied { by: id, reason };
            }
            if next != current {
                mutated_by.push(id.clone());
            }
            current = next;
        }
        BeforeOutcome::Proceed {
            args: current,
            mutated_by,
        }
    }

    /// `tool.after` chain (H2): post-execution middleware over the result.
    /// The result is not a permission gate, so errors always fail open
    /// (keep the host result) — `strict` has no deny to back it here.
    pub(crate) async fn apply_after_hooks(
        &self,
        tool: &str,
        args: &serde_json::Map<String, serde_json::Value>,
        text: &str,
        ok: bool,
        host: &HostCtx<'_>,
    ) -> AfterOutcome {
        let mut current_text = text.to_string();
        let mut current_ok = ok;
        let subs: Vec<String> = {
            let engines = self.engines.read().await;
            engines
                .values()
                .filter(|e| e.events.contains(&"tool.after".to_string()))
                .map(|e| e.manifest.id.clone())
                .collect()
        };
        for id in subs {
            let payload = serde_json::json!({
                "tool": tool,
                "args": args,
                "content": current_text,
                "is_error": !current_ok,
            });
            let envelope = match self.run_event(&id, "tool.after", payload, host).await {
                Ok(json) => json,
                Err(error) => {
                    eprintln!("dex: [extensions] '{id}' tool.after failed: {error}");
                    continue;
                }
            };
            let Ok(serde_json::Value::Object(envelope)) = serde_json::from_str(&envelope) else {
                continue;
            };
            (current_text, current_ok) = hooks::parse_after(&envelope, &current_text, current_ok);
        }
        AfterOutcome {
            text: current_text,
            ok: current_ok,
        }
    }

    /// Fire-and-forget lifecycle event (`turn.start`/`turn.end`): run every
    /// subscriber in load order, log failures, ignore directives — these
    /// events carry no decision the host acts on.
    pub(crate) async fn fire_event(
        &self,
        event: &str,
        payload: serde_json::Value,
        host: &HostCtx<'_>,
    ) {
        let subs: Vec<String> = {
            let engines = self.engines.read().await;
            engines
                .values()
                .filter(|e| e.events.contains(&event.to_string()))
                .map(|e| e.manifest.id.clone())
                .collect()
        };
        for id in subs {
            if let Err(error) = self.run_event(&id, event, payload.clone(), host).await {
                eprintln!("dex: [extensions] '{id}' {event} failed: {error}");
            }
        }
    }

    /// `before_agent_start` chain: each handler may return `{ append = text }`
    /// (or set `ev.append`); strings concatenate in load order. Fail-open: a
    /// handler error is logged and skipped — a broken hook must not stall the
    /// turn before it starts. Read-only influence: no gate interaction (the
    /// hook host still gets the turn's policy for nested `dex.tools.call`).
    pub(crate) async fn apply_before_agent_start(&self, host: &HostCtx<'_>) -> Vec<String> {
        let subs: Vec<String> = {
            let engines = self.engines.read().await;
            engines
                .values()
                .filter(|e| e.events.contains(&"before_agent_start".to_string()))
                .map(|e| e.manifest.id.clone())
                .collect()
        };
        let mut appends = Vec::new();
        for id in subs {
            let envelope = match self
                .run_event(&id, "before_agent_start", serde_json::json!({}), host)
                .await
            {
                Ok(json) => json,
                Err(error) => {
                    eprintln!("dex: [extensions] '{id}' before_agent_start failed: {error}");
                    continue;
                }
            };
            let Ok(serde_json::Value::Object(envelope)) = serde_json::from_str(&envelope) else {
                continue;
            };
            if let Some(append) = envelope.get("append").and_then(|v| v.as_str()) {
                if !append.trim().is_empty() {
                    appends.push(append.to_string());
                }
            }
        }
        appends
    }

    /// `session.before_compact` chain: merged across handlers — any cancel
    /// wins, instruction strings concatenate in load order, the first
    /// summary replacement wins. Fail-open: a handler error is logged and
    /// skipped (compaction must not become extension-hostage).
    pub(crate) async fn apply_before_compact(
        &self,
        emergency: bool,
        message_count: usize,
        host: &HostCtx<'_>,
    ) -> CompactAction {
        let mut action = CompactAction {
            cancel: false,
            instructions: Vec::new(),
            summary: None,
        };
        let payload = serde_json::json!({ "emergency": emergency, "messages": message_count });
        let subs: Vec<String> = {
            let engines = self.engines.read().await;
            engines
                .values()
                .filter(|e| e.events.contains(&"session.before_compact".to_string()))
                .map(|e| e.manifest.id.clone())
                .collect()
        };
        for id in subs {
            let envelope = match self
                .run_event(&id, "session.before_compact", payload.clone(), host)
                .await
            {
                Ok(json) => json,
                Err(error) => {
                    eprintln!("dex: [extensions] '{id}' session.before_compact failed: {error}");
                    continue;
                }
            };
            let Ok(serde_json::Value::Object(envelope)) = serde_json::from_str(&envelope) else {
                continue;
            };
            let parsed = hooks::parse_compact(&envelope);
            action.cancel |= parsed.cancel;
            action.instructions.extend(parsed.instructions);
            if action.summary.is_none() {
                action.summary = parsed.summary;
            }
        }
        action
    }
}

impl ExtensionManager {
    /// `dex.tools.set_active` backing store. Unknown names are dropped: an
    /// extension naming a not-yet-loaded tool must not wedge the schema.
    pub(crate) async fn set_active(&self, tools: Vec<String>) {
        let known: HashSet<String> = {
            let cached = self.cached.read().await;
            cached.iter().map(|d| d.function.name.clone()).collect()
        };
        *self.active.write().expect("active lock") =
            Some(tools.into_iter().filter(|t| known.contains(t)).collect());
    }

    /// The schema slice after the `set_active` filter.
    #[cfg(test)]
    pub(crate) async fn active_cached(&self) -> Vec<ToolDefinition> {
        let all = self.cached.read().await.clone();
        match self.active.read().expect("active lock").clone() {
            None => all,
            Some(active) => all
                .into_iter()
                .filter(|d| active.contains(&d.function.name))
                .collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// System-prompt appendix (load-time, sync read from the prompt builder)
// ---------------------------------------------------------------------------

static PROMPT_APPENDIX: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// `dex.prompt.append(text)`: one entry per extension, load order.
pub(crate) fn push_prompt_appendix(ext_id: &str, text: String) {
    let mut guard = PROMPT_APPENDIX.lock().expect("prompt appendix lock");
    match guard.iter_mut().find(|(id, _)| id == ext_id) {
        Some(entry) => entry.1.push_str(&text),
        None => guard.push((ext_id.to_string(), text)),
    }
}

/// Drop one extension's appendix entry (unload on reload).
pub(crate) fn remove_prompt_appendix(ext_id: &str) {
    PROMPT_APPENDIX
        .lock()
        .expect("prompt appendix lock")
        .retain(|(id, _)| id != ext_id);
}

/// The composed appendix for `system_prompt()`: each extension's text in
/// load order, separated by blank lines.
pub(crate) fn prompt_appendix() -> String {
    let guard = PROMPT_APPENDIX.lock().expect("prompt appendix lock");
    guard
        .iter()
        .map(|(_, text)| text.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(
            "

",
        )
}
pub(crate) fn full_tool_name(ext: &str, tool: &str) -> String {
    format!("lua__{ext}__{tool}")
}

/// Split `lua__<ext>__<tool>`; `None` for anything else (built-ins, MCP).
pub(crate) fn split_lua_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("lua__")?;
    let (ext, tool) = rest.split_once("__")?;
    if ext.is_empty() || tool.is_empty() || tool.contains("__") {
        return None;
    }
    Some((ext, tool))
}

/// Schema cap: extension tools share the per-request schema budget with MCP
/// (plan §9). `DEX_MAX_EXTENSION_TOOLS` overrides for tests.
fn max_extension_tools() -> usize {
    std::env::var("DEX_MAX_EXTENSION_TOOLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64)
}

// ---------------------------------------------------------------------------
// Global (process-wide) manager: sync reads for schema + dispatch paths
// ---------------------------------------------------------------------------

static GLOBAL: OnceLock<std::sync::Arc<ExtensionManager>> = OnceLock::new();

pub(crate) fn global_manager() -> std::sync::Arc<ExtensionManager> {
    GLOBAL
        .get_or_init(|| {
            let mgr = std::sync::Arc::new(ExtensionManager::fresh());
            // Best-effort background load; the schema merges whatever is
            // cached (same contract as MCP). One-shot CLI awaits refresh
            // explicitly instead (see main.rs).
            let clone = std::sync::Arc::clone(&mgr);
            crate::client::http::spawn_task(async move { clone.refresh().await });
            mgr
        })
        .clone()
}

/// Cached extension tools for `tools_schema()` — never blocks, never fails.
pub(crate) fn cached_tools() -> Vec<ToolDefinition> {
    let Some(m) = GLOBAL.get() else {
        return Vec::new();
    };
    let all = m
        .cached
        .try_read()
        .ok()
        .map(|t| t.clone())
        .unwrap_or_default();
    match m.active.read().expect("active lock").clone() {
        None => all,
        Some(active) => all
            .into_iter()
            .filter(|d| active.contains(&d.function.name))
            .collect(),
    }
}

/// Token cost of the cached extension schema slice, for the compaction budget.
pub(crate) fn cached_schema_tokens() -> u64 {
    GLOBAL
        .get()
        .and_then(|m| {
            m.cached
                .try_read()
                .ok()
                .map(|t| crate::agent::tokens::schema_token_estimate(&t))
        })
        .unwrap_or_default()
}

/// Dispatch `lua__<ext>__<tool>`. All errors are plain strings; the caller
/// maps to `ToolError::Internal` for the single audit row.
pub(crate) async fn call_global(
    name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    policy: &crate::tools::Policy,
    filter: Option<&crate::tools::ToolFilter>,
) -> Result<String, String> {
    let host = HostCtx {
        cancel,
        policy,
        filter,
    };
    global_manager().call(name, args, &host).await
}

/// `tool.before` chain for the dispatch path. Zero-cost when no extension
/// subscribes: the args pass through uncloned.
pub(crate) async fn apply_before_hooks(
    tool: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    policy: &crate::tools::Policy,
    filter: Option<&crate::tools::ToolFilter>,
) -> BeforeOutcome {
    let host = HostCtx {
        cancel,
        policy,
        filter,
    };
    if !has_event_handlers("tool.before") {
        return BeforeOutcome::Proceed {
            args: args.clone(),
            mutated_by: Vec::new(),
        };
    }
    global_manager().apply_before_hooks(tool, args, &host).await
}

/// `tool.after` chain for the outcome path. Zero-cost without subscribers.
pub(crate) async fn apply_after_hooks(
    tool: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    text: &str,
    ok: bool,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    policy: &crate::tools::Policy,
    filter: Option<&crate::tools::ToolFilter>,
) -> AfterOutcome {
    let host = HostCtx {
        cancel,
        policy,
        filter,
    };
    if !has_event_handlers("tool.after") {
        return AfterOutcome {
            text: text.to_string(),
            ok,
        };
    }
    global_manager()
        .apply_after_hooks(tool, args, text, ok, &host)
        .await
}

/// Fire-and-forget lifecycle event for the turn loop. Zero-cost without
/// subscribers.
pub(crate) async fn fire_event_global(
    event: &str,
    payload: serde_json::Value,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    policy: &crate::tools::Policy,
    filter: Option<&crate::tools::ToolFilter>,
) {
    if !has_event_handlers(event) {
        return;
    }
    let host = HostCtx {
        cancel,
        policy,
        filter,
    };
    global_manager().fire_event(event, payload, &host).await;
}

/// `before_agent_start` for `process_turn`: appends to the system prompt
/// for this turn (see the manager method for merge/fail-open semantics).
pub(crate) async fn apply_before_agent_start(
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    policy: &crate::tools::Policy,
    filter: Option<&crate::tools::ToolFilter>,
) -> Vec<String> {
    if !has_event_handlers("before_agent_start") {
        return Vec::new();
    }
    let host = HostCtx {
        cancel,
        policy,
        filter,
    };
    global_manager().apply_before_agent_start(&host).await
}

/// `session.before_compact` for `compact_history`. The host runs without a
/// turn policy at this seam, so nested `dex.tools.call` upcalls inherit a
/// read-only policy — a compaction hook cannot mutate anything.
pub(crate) async fn apply_before_compact(
    emergency: bool,
    message_count: usize,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
) -> CompactAction {
    if !has_event_handlers("session.before_compact") {
        return CompactAction {
            cancel: false,
            instructions: Vec::new(),
            summary: None,
        };
    }
    let policy = crate::tools::Policy::turn(
        crate::core::types::PermissionMode::ReadOnly,
        &crate::core::console::Console::none(),
    );
    let host = HostCtx {
        cancel,
        policy: &policy,
        filter: None,
    };
    global_manager()
        .apply_before_compact(emergency, message_count, &host)
        .await
}

/// All registered extension slash commands: (extension id, name,
/// description), in load order.
pub(crate) fn command_list() -> Vec<(String, String, String)> {
    GLOBAL
        .get()
        .and_then(|m| {
            m.engines.try_read().ok().map(|engines| {
                engines
                    .values()
                    .flat_map(|e| {
                        e.commands
                            .iter()
                            .map(|(name, description)| {
                                (e.manifest.id.clone(), name.clone(), description.clone())
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// Dispatch an extension slash command. Fire-and-forget from the TUI; the
/// result lands in the log.
pub(crate) async fn run_command_global(
    ext_id: &str,
    name: &str,
    arg: &str,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
) -> Result<String, String> {
    let policy = crate::tools::Policy::turn(
        crate::core::types::PermissionMode::ReadOnly,
        &crate::core::console::Console::none(),
    );
    let host = HostCtx {
        cancel,
        policy: &policy,
        filter: None,
    };
    let engine = {
        let manager = global_manager();
        let engines = manager.engines.read().await;
        engines
            .get(ext_id)
            .map(|e| e.engine.clone())
            .ok_or_else(|| format!("extension '{ext_id}' is not loaded"))?
    };
    engine
        .drive(
            CallKind::Command {
                name: name.to_string(),
            },
            serde_json::Value::String(arg.to_string()),
            HOOK_TIMEOUT_SECS_DURATION,
            cancel,
            host,
            None,
        )
        .await
}

// ---------------------------------------------------------------------------
// dex.state: per-extension key/value store (JSON file per extension)
// ---------------------------------------------------------------------------

static STATE: std::sync::Mutex<BTreeMap<String, BTreeMap<String, serde_json::Value>>> =
    std::sync::Mutex::new(BTreeMap::new());

fn state_file(ext_id: &str) -> PathBuf {
    data_extensions_dir()
        .join("state")
        .join(format!("{ext_id}.json"))
}

/// One JSON file per extension, loaded lazily, written through on every
/// mutation. Values are JSON — small state only (flags, counters), not
/// documents.
fn load_state_file(ext_id: &str) -> BTreeMap<String, serde_json::Value> {
    std::fs::read_to_string(state_file(ext_id))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn with_state<R>(ext_id: &str, f: impl FnOnce(&mut BTreeMap<String, serde_json::Value>) -> R) -> R {
    let mut guard = STATE.lock().expect("state lock");
    let map = guard
        .entry(ext_id.to_string())
        .or_insert_with(|| load_state_file(ext_id));
    let result = f(map);
    let _ = std::fs::create_dir_all(state_file(ext_id).parent().expect("parent"));
    let _ = std::fs::write(
        state_file(ext_id),
        serde_json::to_string_pretty(map).unwrap_or_default(),
    );
    result
}

/// Read-only variant: no file rewrite on a pure `get`.
fn with_state_read<R>(
    ext_id: &str,
    f: impl FnOnce(&BTreeMap<String, serde_json::Value>) -> R,
) -> R {
    let mut guard = STATE.lock().expect("state lock");
    let map = guard
        .entry(ext_id.to_string())
        .or_insert_with(|| load_state_file(ext_id));
    f(map)
}

/// `dex.state.get(key)` — JSON value or nil.
pub(crate) fn state_get(ext_id: &str, key: &str) -> Option<serde_json::Value> {
    with_state_read(ext_id, |map| map.get(key).cloned())
}

/// `dex.state.set(key, value)` — write-through to the extension's state file.
pub(crate) fn state_set(ext_id: &str, key: String, value: serde_json::Value) {
    with_state(ext_id, |map| {
        map.insert(key, value);
    });
}

/// `dex.tools.list()`: the extension tool names (full `lua__` names).
pub(crate) fn tools_list() -> Vec<String> {
    GLOBAL
        .get()
        .and_then(|m| {
            m.cached
                .try_read()
                .ok()
                .map(|t| t.iter().map(|d| d.function.name.clone()).collect())
        })
        .unwrap_or_default()
}

/// `dex.tools.set_active(list)`: persist the schema slice. Unknown names are
/// dropped (an extension naming a not-yet-loaded tool must not wedge the
/// schema); an empty list means "no extension tools".
pub(crate) async fn set_active_global(tools: Vec<String>) {
    if let Some(m) = GLOBAL.get() {
        m.set_active(tools).await;
    }
}

/// Dispatch a shadowed built-in through its shadow (plan §6.4 step 4).
pub(crate) async fn call_shadow_global(
    target: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
    policy: &crate::tools::Policy,
    filter: Option<&crate::tools::ToolFilter>,
    shell_out: &mut Option<crate::tools::ShellEvidence>,
) -> Result<String, String> {
    let host = HostCtx {
        cancel,
        policy,
        filter,
    };
    global_manager()
        .call_shadow(target, args, &host, shell_out)
        .await
}

/// `dex extensions install <dir>`: copy an extension directory into the
/// user-scope dir. The manifest must parse — install validates before
/// copying so a broken extension never lands.
pub(crate) fn install(src: &str) -> Result<String, String> {
    let src = PathBuf::from(src);
    let text = std::fs::read_to_string(src.join("manifest.yaml"))
        .map_err(|e| format!("{}: {e}", src.display()))?;
    let manifest = manifest::parse_manifest(&text)?;
    let target = user_extensions_dir().join(&manifest.id);
    if target.exists() {
        return Err(format!(
            "{} already exists (remove it first)",
            target.display()
        ));
    }
    copy_dir(&src, &target)?;
    Ok(manifest.id)
}

/// `dex extensions remove <id>`: delete the extension directory wherever it
/// was discovered (user or project scope — removing is always a user act).
pub(crate) fn remove(id: &str) -> Result<(), String> {
    // `dir.join(id)` below must never traverse: reject anything outside the
    // same [a-z0-9_-]+ shape the manifest validator enforces.
    if !manifest::valid_segment(id) {
        return Err(format!(
            "invalid extension id '{id}': use [a-z0-9_-]+, max 64 chars"
        ));
    }
    for (dir, _) in scoped_extension_dirs() {
        let ext_dir = dir.join(id);
        if ext_dir.join("manifest.yaml").is_file() {
            std::fs::remove_dir_all(&ext_dir)
                .map_err(|e| format!("removing {}: {e}", ext_dir.display()))?;
            return Ok(());
        }
    }
    Err(format!("extension '{id}' not found"))
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        // Symlinks are skipped: a cyclic one would recurse to stack
        // overflow, and an escaping one would copy outside the extension.
        if entry.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
            eprintln!(
                "dex: [extensions] install: skipping symlink {}",
                entry.path().display()
            );
            continue;
        }
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &target)?;
        } else {
            std::fs::copy(&path, &target).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Disk discovery for `dex doctor`: (id, version, scope, consent state)
/// per found manifest. Reads no manager state, so it is deterministic
/// regardless of what parallel test runs loaded.
pub(crate) fn discovered_extensions() -> Vec<(String, String, &'static str, String)> {
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

/// Per-extension status for `dex extensions list` / `/extensions`: (id,
/// version, tools, events) for every loaded engine. Sync snapshot, never
/// blocks (same contract as `cached_tools`).
pub(crate) fn loaded_summaries() -> Vec<(String, String, Vec<String>, Vec<String>)> {
    GLOBAL
        .get()
        .and_then(|m| {
            m.engines.try_read().ok().map(|engines| {
                engines
                    .values()
                    .map(|e| {
                        (
                            e.manifest.id.clone(),
                            e.manifest.version.clone(),
                            e.tools.clone(),
                            e.events.clone(),
                        )
                    })
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// One-line summary for `dex extensions list` / `/extensions` (shared so
/// the two surfaces never drift).
pub(crate) fn summary_line(id: &str, version: &str, tools: &[String], events: &[String]) -> String {
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
pub(crate) fn list_command() {
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

/// Sync shadow check for `metadata()`: a shadowed built-in is Shell-gated.
/// Never blocks — empty until the first refresh lands (same as the schema).
pub(crate) fn is_shadowed(name: &str) -> bool {
    GLOBAL
        .get()
        .and_then(|m| m.shadowed.try_read().ok().map(|s| s.contains(name)))
        .unwrap_or(false)
}

/// Does any loaded extension subscribe to `event`? Fast path so the common
/// no-hooks turn skips arg cloning + JSON round-trips entirely.
pub(crate) fn has_event_handlers(event: &str) -> bool {
    GLOBAL
        .get()
        .and_then(|m| {
            m.engines.try_read().ok().map(|e| {
                e.values()
                    .any(|ext| ext.events.contains(&event.to_string()))
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Serializes tests that load fixtures into the process-global manager:
    /// two concurrent fixtures would unload each other's extensions via
    /// `reset_for_tests` (and their hooks would interleave mid-test).
    pub(crate) static TEST_GLOBAL_MANAGER_LOCK: tokio::sync::Mutex<()> =
        tokio::sync::Mutex::const_new(());

    impl ExtensionManager {
        /// Test isolation: the manager is process-global, so a fixture
        /// loaded by one test leaks its hooks into every later turn in the
        /// same process. Drops all engines and caches (prompt appendix
        /// included).
        pub(crate) async fn reset_for_tests(&self) {
            self.engines.write().await.clear();
            *self.cached.write().await = Vec::new();
            *self.shadowed.write().await = HashSet::new();
            *self.active.write().expect("active lock") = None;
            PROMPT_APPENDIX
                .lock()
                .expect("prompt appendix lock")
                .clear();
        }
    }

    /// Restore env vars on drop: the tests below set XDG/DEX vars and must
    /// not leak them into other tests in the process (single-threaded runs
    /// especially — env is global and never restored otherwise).
    struct EnvRestore(Vec<(&'static str, Option<String>)>);
    impl EnvRestore {
        fn take(keys: &[&'static str]) -> Self {
            Self(keys.iter().map(|k| (*k, std::env::var(k).ok())).collect())
        }
    }
    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn split_names() {
        assert_eq!(split_lua_name("lua__myext__tool"), Some(("myext", "tool")));
        assert_eq!(full_tool_name("myext", "tool"), "lua__myext__tool");
        assert!(split_lua_name("read").is_none());
        assert!(split_lua_name("mcp__a__b").is_none());
        assert!(split_lua_name("lua__a").is_none());
        assert!(split_lua_name("lua____t").is_none());
        assert!(split_lua_name("lua__a__b__c").is_none());
    }

    /// Write a fixture extension dir; returns the parent temp dir.
    pub(crate) fn fixture_ext(manifest: &str, lua: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "dex-ext-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let dir = root.join("ext");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.yaml"), manifest).unwrap();
        std::fs::write(dir.join("extension.lua"), lua).unwrap();
        root
    }

    pub(crate) fn hello_manifest() -> &'static str {
        r#"
manifest_version: 1
id: hello
version: 0.1.0
capabilities: [tools, workspace.read]
tools:
  - name: greet
    description: Say hi.
    parameters: {"type": "object", "properties": {"who": {"type": "string"}}}
"#
    }

    pub(crate) fn hello_lua() -> &'static str {
        r#"
return function(dex)
  dex.tools.register({ name = "greet", execute = function(ctx, args)
    return "hi " .. (args.who or "there")
  end })
end
"#
    }

    #[tokio::test]
    async fn loads_tool_into_cache() {
        let root = fixture_ext(hello_manifest(), hello_lua());
        let mgr = ExtensionManager::fresh();
        mgr.refresh_with(std::slice::from_ref(&root)).await;
        let tools = mgr.cached.try_read().unwrap().clone();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "lua__hello__greet");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn skips_bad_extension_whole() {
        let root = fixture_ext("not: [valid", hello_lua());
        let mgr = ExtensionManager::fresh();
        mgr.refresh_with(std::slice::from_ref(&root)).await;
        assert!(mgr.cached.try_read().unwrap().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn sandbox_blocks_stdlib_and_bombs() {
        let lua = r#"
return function(dex)
  dex.tools.register({ name = "evil", execute = function(ctx, args)
    return "unreached"
  end })
  -- load-time probes: all must be nil
  assert(os == nil, "os visible");
  assert(io == nil, "io visible");
  assert(package == nil, "package visible");
  assert(debug == nil, "debug visible");
  assert(require == nil, "require visible");
  assert(load == nil, "load visible");
  assert(dofile == nil, "dofile visible");
end
"#;
        let manifest = hello_manifest()
            .replace("greet", "evil")
            .replace("Say hi.", "E.");
        let root = fixture_ext(&manifest, lua);
        let mgr = ExtensionManager::fresh();
        mgr.refresh_with(std::slice::from_ref(&root)).await;
        assert_eq!(mgr.cached.try_read().unwrap().len(), 1);
        // Runaway chunk aborted by the deadline, not the test harness.
        let lua_bomb = r#"
return function(dex)
  dex.tools.register({ name = "evil", execute = function(ctx, args)
    local i = 0
    while true do i = i + 1 end
    return "unreached"
  end })
end
"#;
        let manifest2 = hello_manifest()
            .replace("id: hello", "id: bomb")
            .replace("greet", "evil")
            .replace("Say hi.", "E.")
            .replace("    parameters:", "    timeout: 2\n    parameters:");
        let root2 = fixture_ext(&manifest2, lua_bomb);
        let mgr2 = ExtensionManager::fresh();
        mgr2.refresh_with(std::slice::from_ref(&root2)).await;
        let policy = crate::tools::Policy::trusted();
        let host = HostCtx {
            cancel: &crate::agent::state::GlobalCancellation,
            policy: &policy,
            filter: None,
        };
        let err = mgr2
            .call("lua__bomb__evil", &serde_json::Map::new(), &host)
            .await
            .unwrap_err();
        assert!(
            err.contains("deadline") || err.contains("timed out"),
            "got: {err}"
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&root2).ok();
    }

    #[tokio::test]
    async fn lua_tools_are_shell_gated_and_filtered() {
        use crate::core::types::PermissionMode;
        use crate::tools::{Policy, ToolFilter};
        // Static metadata arm: Shell requirement, like mcp__.
        let meta = crate::tools::metadata("lua__anything__tool").unwrap();
        assert_eq!(meta.permission, crate::tools::PermissionRequirement::Shell);

        let args = serde_json::Map::new();
        // GLOBAL is empty in tests: Trusted passes the gates and fails at
        // dispatch (unknown tool) — proving the gate passed, not denied.
        let err = crate::tools::execute(
            "lua__noext__notool",
            &args,
            &crate::agent::state::GlobalCancellation,
            &Policy::trusted(),
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown tool"), "got: {err}");
        // ReadOnly denies before dispatch.
        let console = crate::core::console::Console::none();
        let err = crate::tools::execute(
            "lua__noext__notool",
            &args,
            &crate::agent::state::GlobalCancellation,
            &Policy::turn(PermissionMode::ReadOnly, &console),
            None,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("denied"), "got: {err}");
        // Child allowlists reject lua__ before any gate or dispatch.
        let filter = ToolFilter::new("explorer", ["read"]);
        let err = crate::tools::execute(
            "lua__noext__notool",
            &args,
            &crate::agent::state::GlobalCancellation,
            &Policy::trusted(),
            Some(&filter),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("allowlist"), "got: {err}");
    }

    /// Write several fixture extensions under one parent; each item is
    /// (id, manifest, lua). Returns the parent dir for `refresh_with`.
    pub(crate) fn fixture_exts(items: &[(&str, &str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "dex-exts-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        for (id, manifest, lua) in items {
            let dir = root.join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("manifest.yaml"), manifest).unwrap();
            std::fs::write(dir.join("extension.lua"), lua).unwrap();
        }
        root
    }

    fn hook_manifest(id: &str, strict: bool) -> String {
        format!(
            "manifest_version: 1\nid: {id}\nversion: 0.1.0\ncapabilities: []\nstrict: {strict}\n"
        )
    }

    fn test_host() -> (
        crate::agent::state::GlobalCancellation,
        crate::tools::Policy,
    ) {
        (
            crate::agent::state::GlobalCancellation,
            crate::tools::Policy::trusted(),
        )
    }

    #[tokio::test]
    async fn before_hooks_mutate_in_load_order_and_deny() {
        let aaa = hook_manifest("aaa-hook", false);
        let zzz = hook_manifest("zzz-hook", false);
        let root = fixture_exts(&[
            (
                "aaa",
                aaa.as_str(),
                r#"return function(dex)
  dex.events.on("tool.before", function(ctx, ev)
    if ev.tool == "bash" then ev.args.command = ev.args.command .. "-a" end
  end)
end
"#,
            ),
            (
                "zzz",
                zzz.as_str(),
                r#"return function(dex)
  dex.events.on("tool.before", function(ctx, ev)
    if ev.tool == "bash" and ev.args.command:find("bad") then
      return { deny = true, reason = "no bad" }
    end
    if ev.tool == "bash" then ev.args.command = ev.args.command .. "-z" end
  end)
end
"#,
            ),
        ]);
        let mgr = ExtensionManager::fresh();
        mgr.refresh_with(std::slice::from_ref(&root)).await;
        let (cancel, policy) = test_host();
        let host = HostCtx {
            cancel: &cancel,
            policy: &policy,
            filter: None,
        };
        let mut args = serde_json::Map::new();
        args.insert(
            "command".to_string(),
            serde_json::Value::String("echo ok".to_string()),
        );
        match mgr.apply_before_hooks("bash", &args, &host).await {
            BeforeOutcome::Proceed { args, mutated_by } => {
                assert_eq!(args["command"], "echo ok-a-z");
                assert_eq!(
                    mutated_by,
                    vec!["aaa-hook".to_string(), "zzz-hook".to_string()]
                );
            }
            BeforeOutcome::Denied { .. } => panic!("should proceed"),
        }
        args.insert(
            "command".to_string(),
            serde_json::Value::String("bad".to_string()),
        );
        match mgr.apply_before_hooks("bash", &args, &host).await {
            BeforeOutcome::Denied { by, reason } => {
                assert_eq!(by, "zzz-hook");
                assert_eq!(reason, "no bad");
            }
            BeforeOutcome::Proceed { .. } => panic!("should deny"),
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn before_hook_errors_fail_open_unless_strict() {
        let lua = r#"return function(dex)
  dex.events.on("tool.before", function(ctx, ev)
    error("boom")
  end)
end
"#;
        for (id, strict, denied) in [("loose", false, false), ("tight", true, true)] {
            let manifest = hook_manifest(id, strict);
            let root = fixture_exts(&[(id, manifest.as_str(), lua)]);
            let mgr = ExtensionManager::fresh();
            mgr.refresh_with(std::slice::from_ref(&root)).await;
            let (cancel, policy) = test_host();
            let host = HostCtx {
                cancel: &cancel,
                policy: &policy,
                filter: None,
            };
            let mut args = serde_json::Map::new();
            args.insert(
                "command".to_string(),
                serde_json::Value::String("x".to_string()),
            );
            match mgr.apply_before_hooks("bash", &args, &host).await {
                BeforeOutcome::Denied { reason, .. } => {
                    assert!(denied, "fail-open hook denied: {reason}");
                    assert!(reason.contains("strict"), "got: {reason}");
                }
                BeforeOutcome::Proceed { args: out, .. } => {
                    assert!(!denied, "strict hook proceeded");
                    assert_eq!(out["command"], "x");
                }
            }
            std::fs::remove_dir_all(&root).ok();
        }
    }

    #[tokio::test]
    async fn reload_unloads_vanished_extensions_and_their_prompt_appendix() {
        let manifest = "manifest_version: 1\nid: fleeting\nversion: 0.1.0\ncapabilities: []\n";
        let root = fixture_ext(
            manifest,
            "return function(dex)\n  dex.prompt.append(\"hello\")\nend\n",
        );
        let mgr = ExtensionManager::fresh();
        mgr.refresh_with(std::slice::from_ref(&root)).await;
        assert_eq!(mgr.engines.read().await.len(), 1);
        assert!(prompt_appendix().contains("hello"));
        // The reload reconcile drops what is no longer discovered (here:
        // nothing) and its prompt appendix with it.
        mgr.unload_missing(&std::collections::HashSet::new()).await;
        assert!(mgr.engines.read().await.is_empty());
        assert!(!prompt_appendix().contains("hello"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn memory_blowup_fails_the_load_not_the_process() {
        // The per-VM memory ceiling turns a 512 MiB `string.rep` into a Lua
        // error caught at load — not an OOM kill of the whole process.
        let manifest = "manifest_version: 1\nid: hog\nversion: 0.1.0\ncapabilities: []\n";
        let root = fixture_ext(
            manifest,
            "return function(dex)\n  local s = string.rep(\"x\", 512 * 1024 * 1024)\n  return s\nend\n",
        );
        let mgr = ExtensionManager::fresh();
        mgr.refresh_with(std::slice::from_ref(&root)).await;
        assert!(
            mgr.engines.read().await.is_empty(),
            "over-limit setup must fail the whole extension"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn consent_and_remove_reject_non_segment_ids() {
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::take(&["XDG_DATA_HOME"]);
        // Traversal shapes must never reach the marker or data dirs.
        assert!(set_enabled("../../tmp/evil", true).is_err());
        assert!(set_enabled("", true).is_err());
        assert!(set_enabled("Bad Id!", true).is_err());
        assert!(remove("../../some-other-dir").is_err());
        assert!(remove("ok_id-but/with-slash").is_err());
        // Nothing was written.
        assert!(!data_extensions_dir().join("enabled").exists());
    }

    #[tokio::test]
    async fn bad_hook_registration_skips_the_extension_whole() {
        // Unknown event fails legibly at load.
        let unknown_event = (
            "ev",
            &hook_manifest("ev", false),
            r#"return function(dex)
  dex.events.on("context", function(ctx, ev) end)
end
"#,
        );
        // Override without the capability.
        let no_cap = (
            "nocap",
            "manifest_version: 1\nid: nocap\nversion: 0.1.0\ncapabilities: []\n",
            r#"return function(dex)
  dex.tools.register({ name = "read", override = true, execute = function(ctx, args)
    return "x"
  end })
end
"#,
        );
        // Override of a nonexistent tool passes the worker but fails the
        // manager's built-in check.
        let no_target = (
            "notarget",
            "manifest_version: 1\nid: notarget\nversion: 0.1.0\ncapabilities: [tools.override]\n",
            r#"return function(dex)
  dex.tools.register({ name = "nope", override = true, execute = function(ctx, args)
    return "x"
  end })
end
"#,
        );
        let root = fixture_exts(&[
            (unknown_event.0, unknown_event.1, unknown_event.2),
            (no_cap.0, no_cap.1, no_cap.2),
            (no_target.0, no_target.1, no_target.2),
        ]);
        let mgr = ExtensionManager::fresh();
        mgr.refresh_with(std::slice::from_ref(&root)).await;
        assert!(mgr.cached.try_read().unwrap().is_empty());
        assert!(mgr.engines.read().await.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn call_original_outside_a_shadow_is_an_error() {
        let manifest = r#"
manifest_version: 1
id: noshadow
version: 0.1.0
capabilities: [tools]
tools:
  - name: oops
    description: O.
    parameters: {"type": "object"}
"#;
        let lua = r#"return function(dex)
  dex.tools.register({ name = "oops", execute = function(ctx, args)
    return dex.tools.call_original(ctx, args)
  end })
end
"#;
        let root = fixture_ext(manifest, lua);
        let mgr = ExtensionManager::fresh();
        mgr.refresh_with(std::slice::from_ref(&root)).await;
        let (cancel, policy) = test_host();
        let host = HostCtx {
            cancel: &cancel,
            policy: &policy,
            filter: None,
        };
        let err = mgr
            .call("lua__noshadow__oops", &serde_json::Map::new(), &host)
            .await
            .unwrap_err();
        assert!(err.contains("outside a shadow"), "got: {err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn shadow_wraps_ls_and_composes_via_call_original() {
        // Passthrough unless the magic flag is set, so concurrent tests
        // using `ls` normally never observe the shadow.
        let manifest =
            "manifest_version: 1\nid: shadowls\nversion: 0.1.0\ncapabilities: [tools.override]\n";
        let lua = r#"return function(dex)
  dex.tools.register({ name = "ls", override = true, execute = function(ctx, args)
    local out = dex.tools.call_original(ctx, args)
    if args.magic == true then return "wrapped:" .. out end
    return out
  end })
end
"#;
        let _lock = TEST_GLOBAL_MANAGER_LOCK.lock().await;
        let root = fixture_exts(&[("shadowls", manifest, lua)]);
        global_manager()
            .refresh_with(std::slice::from_ref(&root))
            .await;
        assert!(crate::tools::metadata("ls").is_some());
        assert_eq!(
            crate::tools::metadata("ls").unwrap().permission,
            crate::tools::PermissionRequirement::Shell
        );
        // Workspace-confined: list the test process's cwd, which is inside.
        let mut args = serde_json::Map::new();
        args.insert(
            "path".to_string(),
            serde_json::Value::String(".".to_string()),
        );
        let (cancel, policy) = test_host();
        let plain = crate::tools::execute("ls", &args, &cancel, &policy, None)
            .await
            .unwrap();
        assert!(!plain.starts_with("wrapped:"), "got: {plain}");
        args.insert("magic".to_string(), serde_json::Value::Bool(true));
        // The magic flag is not part of the ls schema: strip it for the
        // passthrough comparison by re-running plain below.
        let wrapped = crate::tools::execute("ls", &args, &cancel, &policy, None).await;
        // ls rejects unknown args — the shadow still composed (the error or
        // the wrap proves the shadow ran, not the built-in alone).
        match wrapped {
            Ok(out) => assert_eq!(out, format!("wrapped:{plain}"), "got: {out}"),
            Err(e) => panic!("shadow should pass args through: {e}"),
        }
        std::fs::remove_dir_all(&root).ok();
        // Drop the fixture: the `ls` shadow must not intercept every later
        // `ls` call (and re-gate it) in this test process.
        global_manager().reset_for_tests().await;
    }

    #[test]
    fn config_paths_parse_and_layer_env_over_file() {
        let yaml: serde_yaml::Value =
            serde_yaml::from_str("extensions:\n  paths:\n    - /a/b\n    - /c/d\n").unwrap();
        let paths = parse_config_paths(&yaml);
        assert_eq!(paths, vec![PathBuf::from("/a/b"), PathBuf::from("/c/d")]);
        // Missing/empty shapes are fine.
        assert!(parse_config_paths(&serde_yaml::Value::Null).is_empty());
        let yaml: serde_yaml::Value = serde_yaml::from_str("extensions: {}").unwrap();
        assert!(parse_config_paths(&yaml).is_empty());
        // Non-string entries are skipped, not fatal.
        let yaml: serde_yaml::Value =
            serde_yaml::from_str("extensions:\n  paths:\n    - /ok\n    - 42\n").unwrap();
        assert_eq!(parse_config_paths(&yaml), vec![PathBuf::from("/ok")]);
    }

    #[test]
    fn config_paths_reach_discovery_as_user_scope() {
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::take(&["XDG_CONFIG_HOME", "XDG_DATA_HOME", "DEX_EXTENSIONS_PATHS"]);
        let root = std::env::temp_dir().join(format!("dex-ext-cfgpaths-{}", std::process::id()));
        std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
        std::env::set_var("XDG_DATA_HOME", root.join("data"));
        let cfg_dir = root.join("config/dex");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.yaml"),
            "extensions:\n  paths:\n    - /tmp/dex-ext-cfgpaths-extra\n",
        )
        .unwrap();
        std::fs::create_dir_all("/tmp/dex-ext-cfgpaths-extra/myext").unwrap();
        std::fs::write(
            "/tmp/dex-ext-cfgpaths-extra/myext/manifest.yaml",
            "manifest_version: 1\nid: cfgext\nversion: 0.1.0\ncapabilities: []\n",
        )
        .unwrap();
        let dirs = scoped_extension_dirs();
        assert!(dirs
            .iter()
            .any(|(d, scope)| d.ends_with("dex-ext-cfgpaths-extra") && *scope == Scope::User));
        let found = discovered_extensions();
        assert!(found
            .iter()
            .any(|(id, _, scope, _)| id == "cfgext" && *scope == "user"));
        // Env wins over the file.
        std::env::set_var("DEX_EXTENSIONS_PATHS", "/tmp/dex-ext-cfgpaths-env");
        let dirs = scoped_extension_dirs();
        assert!(dirs
            .iter()
            .any(|(d, _)| d.ends_with("dex-ext-cfgpaths-env")));
        assert!(!dirs
            .iter()
            .any(|(d, _)| d.ends_with("dex-ext-cfgpaths-extra")));
        std::env::remove_var("DEX_EXTENSIONS_PATHS");
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all("/tmp/dex-ext-cfgpaths-extra").ok();
    }

    #[test]
    fn markers_gate_scopes_and_doctor_discovery() {
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK.lock().unwrap();
        let _env = EnvRestore::take(&["XDG_CONFIG_HOME", "XDG_DATA_HOME"]);
        let root = std::env::temp_dir().join(format!("dex-ext-markers-{}", std::process::id()));
        std::env::set_var("XDG_DATA_HOME", &root);
        std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
        // User-scope extension: enabled unless disabled.
        let user_ext = crate::extensions::user_extensions_dir().join("userext");
        std::fs::create_dir_all(&user_ext).unwrap();
        std::fs::write(
            user_ext.join("manifest.yaml"),
            "manifest_version: 1\nid: userext\nversion: 0.1.0\ncapabilities: []\n",
        )
        .unwrap();
        // Project-scope extension: needs the consent marker. Discovered
        // from the test process's cwd (the crate root) — created here and
        // removed afterwards; never committed.
        let proj_ext = std::path::PathBuf::from(".dex/extensions").join("projext");
        std::fs::create_dir_all(&proj_ext).unwrap();
        std::fs::write(
            proj_ext.join("manifest.yaml"),
            "manifest_version: 1\nid: projext\nversion: 0.2.0\ncapabilities: []\n",
        )
        .unwrap();
        let found = discovered_extensions();
        let ids: Vec<&str> = found.iter().map(|(id, ..)| id.as_str()).collect();
        assert_eq!(ids, vec!["projext", "userext"]);
        let proj = found.iter().find(|(id, ..)| id == "projext").unwrap();
        assert_eq!(proj.3, "not enabled (trust gate)");
        // Consent flips the project state; disable flips the user one.
        set_enabled("projext", true).unwrap();
        set_enabled("userext", false).unwrap();
        let found = discovered_extensions();
        assert_eq!(
            found.iter().find(|(id, ..)| id == "projext").unwrap().3,
            "enabled"
        );
        assert_eq!(
            found.iter().find(|(id, ..)| id == "userext").unwrap().3,
            "disabled"
        );
        // Cleanup.
        set_enabled("projext", false).ok();
        set_enabled("userext", true).ok();
        std::fs::remove_dir_all(".dex").ok();
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn lifecycle_events_fire_and_compact_hooks_merge() {
        let m = hook_manifest("lifecycle", false);
        let root = fixture_exts(&[(
            "lifecycle",
            m.as_str(),
            r#"return function(dex)
  dex.events.on("turn.start", function(ctx, ev)
    dex.log.info("turn-start-seen")
  end)
  dex.events.on("session.before_compact", function(ctx, ev)
    if ev.emergency then return { cancel = true } end
    return { instructions = "keep the plan verbatim" }
  end)
end
"#,
        )]);
        let mgr = ExtensionManager::fresh();
        mgr.refresh_with(std::slice::from_ref(&root)).await;
        let (cancel, policy) = test_host();
        let host = HostCtx {
            cancel: &cancel,
            policy: &policy,
            filter: None,
        };
        // Fire-and-forget: no panic, no result.
        mgr.fire_event("turn.start", serde_json::json!({}), &host)
            .await;
        // Merge semantics: normal run -> instructions; emergency -> cancel
        // wins (merged across handlers, here a single one).
        let action = mgr.apply_before_compact(false, 10, &host).await;
        assert!(!action.cancel);
        assert_eq!(
            action.instructions,
            vec!["keep the plan verbatim".to_string()]
        );
        let action = mgr.apply_before_compact(true, 10, &host).await;
        assert!(action.cancel);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn compact_summary_replacement_and_tools_active_slice() {
        let m = hook_manifest("compactor", false);
        let tool_manifest = "manifest_version: 1\nid: compactor\nversion: 0.1.0\ncapabilities: [tools, tools.override]\ntools:\n  - name: probe\n    description: probe\n    parameters: {\"type\":\"object\",\"properties\":{}}\n";
        let root = fixture_exts(&[(
            "compactor",
            tool_manifest,
            r#"return function(dex)
  dex.events.on("session.before_compact", function(ctx, ev)
    return { summary = "HOOK SUMMARY" }
  end)
  dex.tools.register({ name = "probe", execute = function(ctx, args) return "probe" end })
end
"#,
        )]);
        let _ = m;
        let mgr = ExtensionManager::fresh();
        mgr.refresh_with(std::slice::from_ref(&root)).await;
        let (cancel, policy) = test_host();
        let host = HostCtx {
            cancel: &cancel,
            policy: &policy,
            filter: None,
        };
        let action = mgr.apply_before_compact(false, 10, &host).await;
        assert_eq!(action.summary.as_deref(), Some("HOOK SUMMARY"));
        // set_active slice: full name filtering, unknown names dropped.
        mgr.set_active(vec![
            "lua__compactor__probe".to_string(),
            "lua__compactor__ghost".to_string(),
        ])
        .await;
        let names: Vec<String> = mgr
            .active_cached()
            .await
            .iter()
            .map(|d| d.function.name.clone())
            .collect();
        assert_eq!(names, vec!["lua__compactor__probe".to_string()]);
        // Empty list = no extension tools.
        mgr.set_active(vec![]).await;
        assert_eq!(mgr.active_cached().await.len(), 0);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn after_hooks_rewrite_content_and_flip_ok_in_load_order() {
        let aaa = hook_manifest("aaa-after", false);
        let zzz = hook_manifest("zzz-after", false);
        let root = fixture_exts(&[
            (
                "aaa",
                aaa.as_str(),
                r#"return function(dex)
  dex.events.on("tool.after", function(ctx, ev)
    ev.content = ev.content .. "-a"
  end)
end
"#,
            ),
            (
                "zzz",
                zzz.as_str(),
                r#"return function(dex)
  dex.events.on("tool.after", function(ctx, ev)
    if ev.content:find("flip") then ev.is_error = true end
    ev.content = ev.content .. "-z"
  end)
end
"#,
            ),
        ]);
        let mgr = ExtensionManager::fresh();
        mgr.refresh_with(std::slice::from_ref(&root)).await;
        let (cancel, policy) = test_host();
        let host = HostCtx {
            cancel: &cancel,
            policy: &policy,
            filter: None,
        };
        let args = serde_json::Map::new();
        let out = mgr
            .apply_after_hooks("read", &args, "body", true, &host)
            .await;
        assert_eq!(out.text, "body-a-z");
        assert!(out.ok);
        let out = mgr
            .apply_after_hooks("read", &args, "flip me", true, &host)
            .await;
        assert_eq!(out.text, "flip me-a-z");
        assert!(!out.ok);
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn hook_mutation_reaches_the_gates() {
        // The gate check (`session_approved`) runs on the rewritten args:
        // pre-approving ONLY the rewritten call lets the original through.
        let manifest = hook_manifest("gatehook", false);
        let lua = r#"return function(dex)
  dex.events.on("tool.before", function(ctx, ev)
    if ev.tool == "bash" and ev.args.command == "echo original" then
      ev.args.command = "echo rewritten"
    end
  end)
end
"#;
        let _lock = TEST_GLOBAL_MANAGER_LOCK.lock().await;
        let root = fixture_exts(&[("gatehook", manifest.as_str(), lua)]);
        global_manager()
            .refresh_with(std::slice::from_ref(&root))
            .await;
        let console = crate::core::console::Console::none();
        console.record_session_approval("bash", r#"{"command":"echo rewritten"}"#);
        let policy =
            crate::tools::Policy::turn(crate::core::types::PermissionMode::AskShell, &console);
        let mut args = serde_json::Map::new();
        args.insert(
            "command".to_string(),
            serde_json::Value::String("echo original".to_string()),
        );
        let out = crate::tools::execute(
            "bash",
            &args,
            &crate::agent::state::GlobalCancellation,
            &policy,
            None,
        )
        .await
        .unwrap();
        assert!(out.contains("rewritten"), "got: {out}");
        std::fs::remove_dir_all(&root).ok();
        // Drop the fixture: the `echo original` rewriter must not see later
        // tests' bash calls.
        global_manager().reset_for_tests().await;
    }

    #[tokio::test]
    async fn hook_deny_is_attributed_and_never_executes() {
        let manifest = hook_manifest("denyhook", false);
        let lua = r#"return function(dex)
  dex.events.on("tool.before", function(ctx, ev)
    if ev.tool == "bash" and ev.args.command:find("rm %-rf") then
      return { deny = true, reason = "dangerous command" }
    end
  end)
end
"#;
        let _lock = TEST_GLOBAL_MANAGER_LOCK.lock().await;
        let root = fixture_exts(&[("denyhook", manifest.as_str(), lua)]);
        global_manager()
            .refresh_with(std::slice::from_ref(&root))
            .await;
        let mut args = serde_json::Map::new();
        args.insert(
            "command".to_string(),
            serde_json::Value::String("rm -rf /tmp/dex-never".to_string()),
        );
        let (cancel, policy) = test_host();
        let err = crate::tools::execute("bash", &args, &cancel, &policy, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("permission denied"), "got: {err}");
        assert!(err.contains("denyhook"), "got: {err}");
        assert!(err.contains("dangerous command"), "got: {err}");
        std::fs::remove_dir_all(&root).ok();
        // Drop the fixture: the `rm -rf` denier must not see later tests'
        // bash calls.
        global_manager().reset_for_tests().await;
    }

    #[tokio::test]
    async fn call_roundtrip_and_unknown_tool() {
        let root = fixture_ext(hello_manifest(), hello_lua());
        let mgr = ExtensionManager::fresh();
        mgr.refresh_with(std::slice::from_ref(&root)).await;
        let mut args = serde_json::Map::new();
        args.insert(
            "who".to_string(),
            serde_json::Value::String("bob".to_string()),
        );
        let policy = crate::tools::Policy::trusted();
        let host = HostCtx {
            cancel: &crate::agent::state::GlobalCancellation,
            policy: &policy,
            filter: None,
        };
        let out = mgr.call("lua__hello__greet", &args, &host).await.unwrap();
        assert_eq!(out, "hi bob");
        assert!(mgr.call("lua__hello__nope", &args, &host).await.is_err());
        assert!(mgr.call("read", &args, &host).await.is_err());
        std::fs::remove_dir_all(&root).ok();
    }
}
