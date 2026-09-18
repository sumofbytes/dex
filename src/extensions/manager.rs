//! The extension manager: load/reload, tool schema, dispatch.

use crate::protocol::{FunctionDef, ToolDefinition};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use super::appendix::{
    full_tool_name, max_extension_tools, remove_prompt_appendix, split_ext_name, warn_hidden_tools,
};
use super::discovery::{discover_scoped, scoped_extension_dirs, user_extensions_dir};
use super::hooks;
use super::manifest::{self, Manifest};
use super::{
    AfterOutcome, BeforeOutcome, CallKind, CompactAction, ExtensionEngine, HostCtx, ShadowCtx,
    HOOK_TIMEOUT_SECS, SLOW_HOOK_WARN,
};

pub(crate) struct LoadedExtension {
    pub(crate) manifest: Manifest,
    pub(crate) engine: ExtensionEngine,
    /// Full `ext__<ext>__<tool>` names, sorted (shadows excluded: they ride
    /// the built-in name, never a `ext__` one).
    pub(crate) tools: Vec<String>,
    /// Subscribed event names, sorted.
    pub(crate) events: Vec<String>,
    /// Shadowed built-in names.
    pub(crate) shadows: Vec<String>,
    /// Registered slash commands: (name, description), sorted by name.
    pub(crate) commands: Vec<(String, String)>,
}

pub(crate) struct ExtensionManager {
    /// Schema cache behind `Arc`: readers clone the `Arc`, never the defs.
    pub(crate) cached: tokio::sync::RwLock<Arc<[ToolDefinition]>>,
    /// Token cost of `cached`, precomputed at rebuild: the per-turn budget
    /// reads this instead of re-serializing schemas on every model call.
    pub(crate) cached_tokens: tokio::sync::RwLock<u64>,
    /// Per-tool token costs, parallel to `cached` names: the `active`-slice
    /// budget sums these instead of re-serializing the filtered defs live.
    pub(crate) cached_costs: tokio::sync::RwLock<Arc<[(String, u64)]>>,
    /// Sorted by id: iteration order is hook order (same contract as the
    /// skills dedup — deterministic, first-wins on collision).
    pub(crate) engines: tokio::sync::RwLock<BTreeMap<String, LoadedExtension>>,
    /// Built-in names currently shadowed (sync read for `metadata()`).
    pub(crate) shadowed: tokio::sync::RwLock<HashSet<String>>,
    /// `dex.tools.set_active` slice: `None` = all extension tools, `Some`
    /// = exactly these full names. Stored as-given and filtered against the
    /// cache at read time, so a load-time call naming tools that enter the
    /// cache in the same refresh still applies (short names resolve to full
    /// in `set_active_global`, before they land here). Sync read on the
    /// schema path.
    pub(crate) active: std::sync::RwLock<Option<Vec<String>>>,
    /// Serializes refreshes: without it the background load and an
    /// explicit one (one-shot / `run`) race, boot two workers per
    /// extension, and log twice. The second refresh then no-ops on the
    /// already-present ids.
    pub(crate) refresh_lock: tokio::sync::Mutex<()>,
}

impl ExtensionManager {
    pub(crate) fn fresh() -> Self {
        Self {
            cached: tokio::sync::RwLock::new(Arc::new([])),
            cached_tokens: tokio::sync::RwLock::new(0),
            cached_costs: tokio::sync::RwLock::new(Arc::new([])),
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
                match super::manifest::parse_manifest(&text) {
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

    /// Ensure one extension is loaded, booting just it on first use (§26):
    /// `dex run ext__foo__bar` boots one Lua VM instead of every installed
    /// extension, and a dispatch racing the background refresh self-heals
    /// instead of failing "unknown tool". No-op when already loaded.
    /// Collision rejection still applies (see `load_one`): an explicitly
    /// addressed latecomer whose tool name is already registered fails
    /// instead of silently renaming.
    pub(crate) async fn ensure_loaded(&self, ext_id: &str) -> Result<(), String> {
        if self.engines.read().await.contains_key(ext_id) {
            return Ok(());
        }
        self.ensure_loaded_found(ext_id, discover_scoped()).await
    }

    pub(crate) async fn ensure_loaded_found(
        &self,
        ext_id: &str,
        found: Vec<(PathBuf, Manifest)>,
    ) -> Result<(), String> {
        if self.engines.read().await.contains_key(ext_id) {
            return Ok(());
        }
        let Some((dir, m)) = found.into_iter().find(|(_, m)| m.id == ext_id) else {
            return Err(format!("unknown extension '{ext_id}'"));
        };
        // Same serialized core as `refresh_found` (load + cache rebuild),
        // minus the scan — concurrent refreshes can't double-load.
        let _guard = self.refresh_lock.lock().await;
        if self.engines.read().await.contains_key(&m.id) {
            return Ok(());
        }
        self.load_one(&dir, m).await?;
        self.rebuild_cache().await;
        Ok(())
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
    pub(crate) async fn unload_missing(&self, keep: &HashSet<String>) {
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
                    .strip_prefix(&format!("ext__{}__", ext.manifest.id))
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
            let dropped = tools.len() - max;
            tools.truncate(max);
            warn_hidden_tools(dropped, max);
        }
        // Per-tool char shares for the `active`-slice budget (see
        // `cached_schema_tokens`); the whole-cache total keeps the exact
        // `schema_token_estimate` formula.
        let costs: Arc<[(String, u64)]> = tools
            .iter()
            .map(|d| {
                (
                    d.function.name.clone(),
                    crate::agent::tokens::schema_chars(std::slice::from_ref(d)) as u64,
                )
            })
            .collect();
        *self.cached_tokens.write().await = crate::agent::tokens::schema_token_estimate(&tools);
        *self.cached_costs.write().await = costs;
        *self.cached.write().await = Arc::from(tools);
        let shadowed: HashSet<String> = engines
            .values()
            .flat_map(|e| e.shadows.iter().cloned())
            .collect();
        *self.shadowed.write().await = shadowed;
    }

    /// Dispatch `ext__<ext>__<tool>`: run the tool on its worker, answering
    /// nested `dex.tools.call` with the caller's gates.
    pub(crate) async fn call(
        &self,
        full_name: &str,
        args: &serde_json::Map<String, serde_json::Value>,
        host: &HostCtx<'_>,
    ) -> Result<String, String> {
        let (ext_id, tool) =
            split_ext_name(full_name).ok_or_else(|| format!("unknown tool '{full_name}'"))?;
        // Clone the engine out of the lock: it is channel-based, so no Lua
        // state is held across the `.await` below.
        let (engine, timeout) = {
            let engines = self.engines.read().await;
            if let Some(ext) = engines.get(ext_id) {
                (ext.engine.clone(), ext.engine.tool_timeout(tool))
            } else {
                // Lazy (§26): the background refresh may still be running
                // (or this process never refreshed) — boot the addressed
                // extension on first use. A genuinely unknown tool keeps
                // the same error.
                drop(engines);
                self.ensure_loaded(ext_id)
                    .await
                    .map_err(|_| format!("unknown tool '{full_name}'"))?;
                let engines = self.engines.read().await;
                let Some(ext) = engines.get(ext_id) else {
                    return Err(format!("unknown tool '{full_name}'"));
                };
                (ext.engine.clone(), ext.engine.tool_timeout(tool))
            }
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
    pub(crate) async fn call_shadow(
        &self,
        target: &str,
        args: &serde_json::Map<String, serde_json::Value>,
        host: &HostCtx<'_>,
        shell_out: &mut Option<crate::tools::ShellEvidence>,
    ) -> Result<String, String> {
        let (engine, timeout) = {
            let engines = self.engines.read().await;
            if let Some(ext) = engines
                .values()
                .find(|e| e.shadows.contains(&target.to_string()))
            {
                (ext.engine.clone(), ext.engine.tool_timeout(target))
            } else {
                // Same lazy self-heal as `call`: a dispatch racing the
                // background refresh boots the owner instead of failing.
                // Scoped candidate-by-candidate (`ensure_loaded_found`),
                // not a full `refresh()` — the shadow owner is only known
                // after load (shadows come from Lua exports, not the
                // manifest), so boot each undiscovered candidate until
                // one claims the target. A genuinely unknown target pays
                // one `discover_scoped()` scan plus the remaining boots,
                // same worst case as before, but the common race heals
                // with a single VM instead of all of them (§26).
                drop(engines);
                let found = discover_scoped();
                for (_, m) in found.clone() {
                    if self
                        .engines
                        .read()
                        .await
                        .values()
                        .any(|e| e.shadows.contains(&target.to_string()))
                    {
                        break;
                    }
                    let _ = self.ensure_loaded_found(&m.id, found.clone()).await;
                }
                let engines = self.engines.read().await;
                let Some(ext) = engines
                    .values()
                    .find(|e| e.shadows.contains(&target.to_string()))
                else {
                    return Err(format!("no shadow registered for '{target}'"));
                };
                (ext.engine.clone(), ext.engine.tool_timeout(target))
            }
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
    /// in [`hooks`] do. Calls slower than [`SLOW_HOOK_WARN`] log a warning:
    /// hooks run inline on the dispatch path (perf doc §9), so a slow hook
    /// is per-turn latency with no other signal.
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
        let started = std::time::Instant::now();
        let out = engine
            .drive(
                CallKind::Event {
                    event: event.to_string(),
                },
                payload,
                std::time::Duration::from_secs(HOOK_TIMEOUT_SECS),
                host.cancel,
                *host,
                None,
            )
            .await;
        let elapsed = started.elapsed();
        if elapsed >= SLOW_HOOK_WARN {
            eprintln!(
                "dex: [extensions] '{ext_id}' {event} took {}ms (slow hook adds per-turn latency)",
                elapsed.as_millis()
            );
        }
        out
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
    /// `dex.tools.set_active` backing store (full names — the extension-facing
    /// `set_active_global` resolves short names first). Stored as-given:
    /// unknown names filter out at read time, once the tools they name enter
    /// the cache (a load-time call races the cache rebuild, so dropping here
    /// would wedge the schema to empty on a fresh process).
    pub(crate) async fn set_active(&self, tools: Vec<String>) {
        *self.active.write().expect("active lock") = Some(tools);
    }

    /// The schema slice after the `set_active` filter.
    #[cfg(test)]
    pub(crate) async fn active_cached(&self) -> Vec<ToolDefinition> {
        let all = self.cached.read().await.clone();
        match self.active.read().expect("active lock").clone() {
            None => all.iter().cloned().collect(),
            Some(active) => {
                let known: HashSet<String> = all.iter().map(|d| d.function.name.clone()).collect();
                let wanted: HashSet<String> =
                    active.into_iter().filter(|t| known.contains(t)).collect();
                all.iter()
                    .filter(|d| wanted.contains(&d.function.name))
                    .cloned()
                    .collect()
            }
        }
    }
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
    // same [a-z0-9_-]+ (no `__`) shape the manifest validator enforces.
    if !manifest::valid_segment(id) {
        return Err(format!(
            "invalid extension id '{id}': use [a-z0-9_-]+, max 64 chars, no `__`"
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
