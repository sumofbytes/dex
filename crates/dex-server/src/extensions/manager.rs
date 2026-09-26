//! The extension manager: load/reload, tool schema, dispatch.

use crate::protocol::{FunctionDef, ToolDefinition};
use std::collections::{BTreeMap, HashSet};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::Arc;

use super::appendix::{
    full_tool_name, max_extension_tools, remove_prompt_appendix, split_ext_name, warn_hidden_tools,
};
use super::discovery::{discover_scoped, scoped_extension_dirs, user_extensions_dir};
use super::hooks;
use super::manifest::{self, Manifest};
use super::{
    AfterOutcome, BeforeOutcome, CallKind, CompactAction, ExtensionEngine, HostCtx,
    SupervisorAction, HOOK_TIMEOUT_SECS, SLOW_HOOK_WARN,
};

/// Directive shared by `before_agent_start` and `llm.before`: an optional
/// non-empty `append` string; appends concatenate in load order. Fail-open —
/// a missing/bad envelope contributes nothing.
fn fold_append(
    envelope: Result<serde_json::Map<String, serde_json::Value>, String>,
    mut appends: Vec<String>,
) -> Vec<String> {
    if let Some(append) = envelope
        .ok()
        .and_then(|m| m.get("append").and_then(|v| v.as_str()).map(str::to_string))
        .filter(|s| !s.trim().is_empty())
    {
        appends.push(append);
    }
    appends
}

pub struct LoadedExtension {
    pub manifest: Manifest,
    pub engine: ExtensionEngine,
    /// Full `ext__<ext>__<tool>` names, sorted (shadows excluded: they ride
    /// the built-in name, never a `ext__` one).
    pub tools: Vec<String>,
    /// Subscribed event names, sorted.
    pub events: Vec<String>,
    /// Shadowed built-in names.
    pub shadows: Vec<String>,
    /// Registered slash commands: (name, description), sorted by name.
    pub commands: Vec<(String, String)>,
    /// Slot names this extension wraps with `dex.wrap` middleware.
    pub wraps: Vec<String>,
    /// Slots the extension backs with `dex.fallback`.
    pub fallbacks: Vec<String>,
    /// Slots the extension implements with `dex.use` (spec §10).
    pub uses: Vec<String>,
    /// `agent_loop` component id when the chunk registered one via
    /// `dex.replace("agent_loop", …)` (spec §9).
    pub agent_loop: Option<String>,
}

impl LoadedExtension {
    /// Whether this extension participates in `event`: an explicit handler
    /// subscription or middleware over a wrappable slot.
    pub fn serves(&self, event: &str) -> bool {
        self.events.iter().any(|e| e == event)
            || self.wraps.iter().any(|e| e == event)
            || self.fallbacks.iter().any(|e| e == event)
    }
}

pub struct ExtensionManager {
    /// Schema cache behind `Arc`: readers clone the `Arc`, never the defs.
    pub cached: tokio::sync::RwLock<Arc<[ToolDefinition]>>,
    /// Token cost of `cached`, precomputed at rebuild: the per-turn budget
    /// reads this instead of re-serializing schemas on every model call.
    pub cached_tokens: tokio::sync::RwLock<u64>,
    /// Per-tool token costs, parallel to `cached` names: the `active`-slice
    /// budget sums these instead of re-serializing the filtered defs live.
    pub cached_costs: tokio::sync::RwLock<Arc<[(String, u64)]>>,
    /// Sorted by id: iteration order is hook order (same contract as the
    /// skills dedup — deterministic, first-wins on collision).
    pub engines: tokio::sync::RwLock<BTreeMap<String, LoadedExtension>>,
    /// Built-in names currently shadowed (sync read for `metadata()`).
    pub shadowed: tokio::sync::RwLock<HashSet<String>>,
    /// `dex.tools.set_active` slice: `None` = all extension tools, `Some`
    /// = exactly these full names. Stored as-given and filtered against the
    /// cache at read time, so a load-time call naming tools that enter the
    /// cache in the same refresh still applies (short names resolve to full
    /// in `set_active_global`, before they land here). Sync read on the
    /// schema path.
    pub active: std::sync::RwLock<Option<Vec<String>>>,
    /// Serializes refreshes: without it the background load and an
    /// explicit one (one-shot / `run`) race, boot two workers per
    /// extension, and log twice. The second refresh then no-ops on the
    /// already-present ids.
    pub refresh_lock: tokio::sync::Mutex<()>,
}

/// Fire `runtime.start` exactly once per process: the first completed
/// bootstrap load (daemon background refresh, one-shot inline, or a racing
/// `ensure_loaded`) announces the runtime to observers. Zero-cost without
/// subscribers.
async fn fire_runtime_start_once() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    let mut first = false;
    STARTED.call_once(|| first = true);
    if !first {
        return;
    }
    crate::extensions::fire_lifecycle_event(
        "runtime.start",
        serde_json::json!({ "version": env!("CARGO_PKG_VERSION") }),
        &crate::runtime::cancel::GlobalCancellation,
    )
    .await;
}

impl ExtensionManager {
    pub fn fresh() -> Self {
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
    pub async fn refresh_with(&self, dirs: &[PathBuf]) {
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
        fire_runtime_start_once().await;
    }

    /// Load one extension: spawn the worker, run its chunk, validate the
    /// exports. Any failure skips the whole extension — nothing
    /// half-registered ever reaches the schema (§9).
    async fn load_one(&self, dir: &std::path::Path, m: Manifest) -> Result<(), String> {
        let source = std::fs::read_to_string(dir.join("extension.lua"))
            .map_err(|e| format!("extension '{}' has no extension.lua: {e}", m.id_for_error()))?;
        let engine = ExtensionEngine::new(m.clone(), dir.to_path_buf())
            .map_err(|e| format!("extension '{}': {e}", m.id_for_error()))?;
        let mut exports = engine.load(source).await?;

        // Declarative component files (spec §31): each loads on the same
        // worker with the same registration contract, after `extension.lua`.
        // Manifest paths are validated (relative, .lua, unique) at parse
        // time; a missing or failing file fails the whole extension.
        for (slot, file) in &m.components {
            let path = dir.join(file);
            let comp_source = std::fs::read_to_string(&path).map_err(|e| {
                format!(
                    "extension '{}' component '{slot}' ({}): {e}",
                    m.id_for_error(),
                    path.display()
                )
            })?;
            let comp = engine
                .load(comp_source)
                .await
                .map_err(|e| format!("extension '{}' component '{slot}': {e}", m.id_for_error()))?;
            exports.tools.extend(comp.tools);
            exports.events.extend(comp.events);
            exports.wraps.extend(comp.wraps);
            exports.fallbacks.extend(comp.fallbacks);
            exports.uses.extend(comp.uses);
            exports.shadows.extend(comp.shadows);
            exports.commands.extend(comp.commands);
            exports.agent_loop = exports.agent_loop.or(comp.agent_loop);
        }
        // Merged component chunks can unsort these; restore the invariants
        // (sorted + deduped) the single-chunk load guarantees.
        exports.commands.sort_by(|a, b| a.0.cmp(&b.0));
        exports.uses.sort();
        exports.uses.dedup();
        exports.shadows.sort();
        exports.shadows.dedup();

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
                wraps: exports.wraps,
                fallbacks: exports.fallbacks,
                uses: exports.uses,
                shadows: exports.shadows,
                commands,
                agent_loop: exports.agent_loop,
            },
        );
        Ok(())
    }

    /// Load from the default discovery dirs, honoring scope consent: a
    /// project-scope extension loads only when enabled (trust gate), a
    /// user-scope one unless disabled. Add-only: never unloads (the
    /// bootstrap/one-shot path — a fresh process has nothing stale, and a
    /// racing background refresh must not wipe a concurrent loader).
    pub async fn refresh(&self) {
        self.refresh_found(discover_scoped()).await;
    }

    /// The agent-loop slot (spec §9): first loaded extension that registered
    /// one via `dex.replace("agent_loop", …)`. Load order is sorted by id,
    /// so first-wins is deterministic. `None` = the Rust `run_turn` default.
    pub async fn agent_loop(&self) -> Option<(String, ExtensionEngine)> {
        self.engines.read().await.values().find_map(|e| {
            e.agent_loop
                .as_ref()
                .map(|id| (id.clone(), e.engine.clone()))
        })
    }

    /// Ensure one extension is loaded, booting just it on first use (§26):
    /// `dex run ext__foo__bar` boots one Lua VM instead of every installed
    /// extension, and a dispatch racing the background refresh self-heals
    /// instead of failing "unknown tool". No-op when already loaded.
    /// Collision rejection still applies (see `load_one`): an explicitly
    /// addressed latecomer whose tool name is already registered fails
    /// instead of silently renaming.
    pub async fn ensure_loaded(&self, ext_id: &str) -> Result<(), String> {
        if self.engines.read().await.contains_key(ext_id) {
            return Ok(());
        }
        self.ensure_loaded_found(ext_id, discover_scoped()).await
    }

    pub async fn ensure_loaded_found(
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
        fire_runtime_start_once().await;
        Ok(())
    }

    /// Explicit reload (`/extensions reload`): like [`refresh`], plus the
    /// reconcile — extensions that vanished from disk or lost consent
    /// unload, so disable/remove takes effect in the running process
    /// without a restart.
    pub async fn reload(&self) {
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
        fire_runtime_start_once().await;
    }

    /// Unload engines whose id is not in `keep`, dropping their prompt
    /// appendix entries. The reload reconcile half of `refresh`.
    pub async fn unload_missing(&self, keep: &HashSet<String>) {
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
    pub async fn call(
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
            )
            .await
    }

    /// Dispatch a shadowed built-in: run the shadow on its worker. Nested
    /// `dex.tools.call` re-enters the full pipeline; `call_original`
    /// re-dispatches the shadowed built-in with the caller's gates.
    pub async fn call_shadow(
        &self,
        target: &str,
        args: &serde_json::Map<String, serde_json::Value>,
        host: &HostCtx<'_>,
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
            if !ext.serves(event) {
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
            )
            .await;
        let elapsed = started.elapsed();
        match &out {
            Ok(_) => super::trace::record(ext_id, event, elapsed, "ok", None),
            Err(error) => {
                super::trace::record(ext_id, event, elapsed, "error", Some(error.clone()))
            }
        }
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
    pub async fn apply_before_hooks(
        &self,
        tool: &str,
        args: &serde_json::Map<String, serde_json::Value>,
        host: &HostCtx<'_>,
    ) -> BeforeOutcome {
        self.run_chain(
            "tool.before",
            host,
            BeforeOutcome::Proceed {
                args: args.clone(),
                mutated_by: Vec::new(),
            },
            |_| false,
            |acc| {
                let current = match acc {
                    BeforeOutcome::Proceed { args, .. } => args,
                    BeforeOutcome::Denied { .. } => {
                        unreachable!("a deny breaks the chain before the next payload")
                    }
                };
                serde_json::json!({ "tool": tool, "args": current })
            },
            |id, strict, envelope, acc| {
                let BeforeOutcome::Proceed {
                    args: current,
                    mutated_by,
                } = acc
                else {
                    return ControlFlow::Break(acc);
                };
                let map = match envelope {
                    Ok(map) => map,
                    // The engine logged it; `strict` turns the failure into
                    // a deny, fail-open keeps the current args.
                    Err(error) => {
                        return if strict {
                            ControlFlow::Break(BeforeOutcome::Denied {
                                by: id.to_string(),
                                reason: format!("hook failed (strict): {error}"),
                            })
                        } else {
                            ControlFlow::Continue(BeforeOutcome::Proceed {
                                args: current,
                                mutated_by,
                            })
                        };
                    }
                };
                let (next, deny) = match hooks::parse_before(&map) {
                    Ok(parsed) => parsed,
                    Err(error) => {
                        eprintln!("dex: [extensions] '{id}' tool.before failed: {error}");
                        return if strict {
                            ControlFlow::Break(BeforeOutcome::Denied {
                                by: id.to_string(),
                                reason: format!("hook failed (strict): {error}"),
                            })
                        } else {
                            ControlFlow::Continue(BeforeOutcome::Proceed {
                                args: current,
                                mutated_by,
                            })
                        };
                    }
                };
                if let Some((_, reason)) = deny {
                    eprintln!("dex: [extensions] '{id}' tool.before denied '{tool}': {reason}");
                    return ControlFlow::Break(BeforeOutcome::Denied {
                        by: id.to_string(),
                        reason,
                    });
                }
                let mutated_by = if next != current {
                    let mut by = mutated_by;
                    by.push(id.to_string());
                    by
                } else {
                    mutated_by
                };
                ControlFlow::Continue(BeforeOutcome::Proceed {
                    args: next,
                    mutated_by,
                })
            },
        )
        .await
    }

    /// `tool.after` chain (H2): post-execution middleware over the result.
    /// The result is not a permission gate, so errors always fail open
    /// (keep the host result) — `strict` has no deny to back it here.
    pub async fn apply_after_hooks(
        &self,
        tool: &str,
        args: &serde_json::Map<String, serde_json::Value>,
        text: &str,
        ok: bool,
        host: &HostCtx<'_>,
    ) -> AfterOutcome {
        let (text, ok) = self
            .run_chain(
                "tool.after",
                host,
                (text.to_string(), ok),
                |_| false,
                |(content, is_error)| {
                    serde_json::json!({
                        "tool": tool,
                        "args": args,
                        "content": content,
                        "is_error": !is_error,
                    })
                },
                |_, _, envelope, (text, ok)| {
                    let folded = envelope.ok().map_or((text.clone(), ok), |map| {
                        hooks::parse_after(&map, &text, ok)
                    });
                    ControlFlow::Continue(folded)
                },
            )
            .await;
        AfterOutcome { text, ok }
    }

    /// Fire-and-forget lifecycle event (`turn.start`/`turn.end`): run every
    /// subscriber in load order, log failures, ignore directives — these
    /// events carry no decision the host acts on.
    pub async fn fire_event(&self, event: &str, payload: serde_json::Value, host: &HostCtx<'_>) {
        self.run_chain(
            event,
            host,
            (),
            |_| false,
            |_| payload.clone(),
            |_, _, _, _| ControlFlow::Continue(()),
        )
        .await;
    }

    /// `before_agent_start` chain: each handler may return `{ append = text }`
    /// (or set `ev.append`); strings concatenate in load order. Fail-open: a
    /// handler error is logged and skipped — a broken hook must not stall the
    /// turn before it starts. Read-only influence: no gate interaction (the
    /// hook host still gets the turn's policy for nested `dex.tools.call`).
    pub async fn apply_before_agent_start(&self, host: &HostCtx<'_>) -> Vec<String> {
        self.run_chain(
            "before_agent_start",
            host,
            Vec::new(),
            |_| false,
            |_| serde_json::json!({}),
            |_, _, envelope, appends| ControlFlow::Continue(fold_append(envelope, appends)),
        )
        .await
    }

    /// `supervisor.route` chain: merged across handlers in load order — the
    /// first redirect wins, any deny wins (attributed). Errors/bad envelopes
    /// fail open to no opinion (normal spawn flow).
    pub async fn query_supervisor_route(
        &self,
        agent: &str,
        task: Option<&str>,
        host: &HostCtx<'_>,
    ) -> SupervisorAction {
        self.run_chain(
            "supervisor.route",
            host,
            SupervisorAction::default(),
            |_| false,
            |_| serde_json::json!({ "agent": agent, "task": task }),
            |id, _, envelope, mut action| {
                if let Ok(map) = envelope {
                    let (redirect, deny) = hooks::parse_supervisor(&map);
                    if action.agent.is_none() {
                        action.agent = redirect;
                    }
                    if action.deny.is_none() {
                        action.deny = deny.map(|reason| (id.to_string(), reason));
                    }
                }
                ControlFlow::Continue(action)
            },
        )
        .await
    }

    /// `permission.request` chain: first explicit `{decision = "allow"|"deny"}`
    /// wins in load order; nil/garbage/errors fail open to `None` (the normal
    /// approval flow). An explicit deny is honored downstream and attributed.
    pub async fn query_permission(
        &self,
        tool: &str,
        args: &serde_json::Map<String, serde_json::Value>,
        requirement: &str,
        mode: &str,
        host: &HostCtx<'_>,
    ) -> Option<(hooks::PermissionDecision, String, String)> {
        self.run_chain(
            "permission.request",
            host,
            None,
            Option::is_some,
            |_| {
                serde_json::json!({
                    "tool": tool,
                    "args": args,
                    "requirement": requirement,
                    "mode": mode,
                })
            },
            |id, _, envelope, _| match envelope.ok().and_then(|m| hooks::parse_permission(&m)) {
                Some((decision, reason)) => {
                    ControlFlow::Break(Some((decision, reason, id.to_string())))
                }
                None => ControlFlow::Continue(None),
            },
        )
        .await
    }

    /// `llm.before` chain: each handler may return `{append = text}` (or set
    /// `ev.append`); strings concatenate in load order and the host persists
    /// them as one user-role note before the compaction gate, so appends
    /// count toward the window. Fail-open. Read-only influence otherwise.
    pub async fn apply_llm_before(
        &self,
        host: &HostCtx<'_>,
        payload: serde_json::Value,
    ) -> Vec<String> {
        self.run_chain(
            "llm.before",
            host,
            Vec::new(),
            |_| false,
            |_| payload.clone(),
            |_, _, envelope, appends| ControlFlow::Continue(fold_append(envelope, appends)),
        )
        .await
    }

    /// `harness.overflow` chain: first non-`nil` `{overflow = bool}` wins in
    /// load order; errors/bad envelopes fail open to `None` (Rust default).
    pub async fn query_harness_overflow(&self, message: &str, host: &HostCtx<'_>) -> Option<bool> {
        let payload = serde_json::json!({ "message": message });
        self.first_harness_bool("harness.overflow", "overflow", payload, host)
            .await
    }

    /// `harness.conflict` chain: first non-`nil` `{conflicts = bool}` wins in
    /// load order; errors/bad envelopes fail open to `None` (Rust default,
    /// which is fail-closed to serialization).
    pub async fn query_harness_conflict(
        &self,
        calls: &serde_json::Value,
        host: &HostCtx<'_>,
    ) -> Option<bool> {
        let payload = serde_json::json!({ "calls": calls });
        self.first_harness_bool("harness.conflict", "conflicts", payload, host)
            .await
    }

    /// `harness.compact` chain: first non-`nil` `{compact = bool}` wins in
    /// load order; errors/bad envelopes fail open to `None` (the Rust
    /// trigger decides). Decision inputs are counts only — no conversation
    /// content crosses the boundary (spec §11).
    pub async fn query_harness_compact(
        &self,
        stored_tokens: u64,
        ephemeral_overhead: u64,
        message_count: usize,
        host: &HostCtx<'_>,
    ) -> Option<bool> {
        let payload = serde_json::json!({
            "stored_tokens": stored_tokens,
            "ephemeral_overhead": ephemeral_overhead,
            "message_count": message_count,
        });
        self.first_harness_bool("harness.compact", "compact", payload, host)
            .await
    }

    /// `harness.summarize` chain: first non-empty `{summary = "..."}` wins
    /// in load order; errors, non-strings, and empty strings fail open to
    /// `None` (the Rust summarizer runs — compaction must not become
    /// extension-hostage).
    pub async fn query_harness_summarize(
        &self,
        conversation: &str,
        previous_summary: Option<&str>,
        host: &HostCtx<'_>,
    ) -> Option<String> {
        self.run_chain(
            "harness.summarize",
            host,
            None,
            Option::is_some,
            |_| {
                serde_json::json!({
                    "conversation": conversation,
                    "previous_summary": previous_summary,
                })
            },
            |_, _, envelope, _| match envelope
                .ok()
                .and_then(|m| hooks::parse_harness_string(&m, "summary"))
            {
                Some(summary) => ControlFlow::Break(Some(summary)),
                None => ControlFlow::Continue(None),
            },
        )
        .await
    }

    /// `model_selector` chain: first non-empty `{model = "..."}` (or a bare
    /// string return, which the envelope carries as `content`) wins in load
    /// order. Errors, non-strings, and empty strings fail open to `None` —
    /// the configured model is served, never a half-resolved one. The host
    /// still resolves the selection against the catalog + credentials
    /// (`apply_model`), so an unresolvable provider fails open there too.
    /// The payload says `current`/`previous`, never `model`: the ev fold
    /// merges payload keys into the directive envelope, and a payload key
    /// must not shadow the directive key.
    pub async fn query_model_selector(
        &self,
        current: &str,
        previous: Option<&str>,
        host: &HostCtx<'_>,
    ) -> Option<String> {
        self.run_chain(
            "model_selector",
            host,
            None,
            Option::is_some,
            |_| {
                serde_json::json!({
                    "current": current,
                    "previous": previous,
                })
            },
            |_, _, envelope, _| {
                let pick = envelope.ok().and_then(|m| {
                    hooks::parse_harness_string(&m, "model")
                        .or_else(|| hooks::parse_harness_string(&m, "content"))
                });
                match pick {
                    Some(model) => ControlFlow::Break(Some(model)),
                    None => ControlFlow::Continue(None),
                }
            },
        )
        .await
    }

    /// `tool_catalog` chain (spec §7/§11): the first handler returning
    /// `{keep = [...]} / {drop = [...]}` narrows the schema list in load
    /// order. Narrowing-only: the applied result is always a subset of the
    /// input (unknown names in `keep` are ignored, `drop` removes). Errors
    /// and empty envelopes fail open to `None` — the unfiltered catalog is
    /// served, never a half-resolved one.
    pub async fn query_tool_catalog(
        &self,
        schemas: &[ToolDefinition],
        host: &HostCtx<'_>,
    ) -> Option<Vec<ToolDefinition>> {
        self.run_chain(
            "tool_catalog",
            host,
            None,
            Option::is_some,
            |_| {
                serde_json::json!({
                    "tools": schemas
                        .iter()
                        .map(|t| serde_json::json!({
                            "name": t.function.name,
                            "description": t.function.description,
                        }))
                        .collect::<Vec<_>>(),
                })
            },
            |_, _, envelope, _| match envelope.ok().and_then(|m| hooks::parse_catalog_filter(&m)) {
                Some(filter) => ControlFlow::Break(Some(filter.apply(schemas))),
                None => ControlFlow::Continue(None),
            },
        )
        .await
    }

    /// `session.before_compact` chain: merged across handlers — any cancel
    /// wins, instruction strings concatenate in load order, the first
    /// summary replacement wins. Fail-open: a handler error is logged and
    /// skipped (compaction must not become extension-hostage).
    pub async fn apply_before_compact(
        &self,
        emergency: bool,
        message_count: usize,
        host: &HostCtx<'_>,
    ) -> CompactAction {
        let payload = serde_json::json!({ "emergency": emergency, "messages": message_count });
        self.run_chain(
            "session.before_compact",
            host,
            CompactAction {
                cancel: false,
                instructions: Vec::new(),
                summary: None,
            },
            |_| false,
            |_| payload.clone(),
            |_, _, envelope, mut action| {
                if let Ok(map) = envelope {
                    let parsed = hooks::parse_compact(&map);
                    action.cancel |= parsed.cancel;
                    action.instructions.extend(parsed.instructions);
                    if action.summary.is_none() {
                        action.summary = parsed.summary;
                    }
                }
                ControlFlow::Continue(action)
            },
        )
        .await
    }

    /// Subscriber snapshot for `event`, in hook order (extensions are
    /// id-sorted): `(extension id, strict)`. Every chain shares this one
    /// selection — `serves` covers explicit handler subscriptions and
    /// `dex.wrap`/`dex.fallback` middleware alike, matching what `run_event`
    /// independently re-checks.
    async fn subscribers(&self, event: &str) -> Vec<(String, bool)> {
        self.engines
            .read()
            .await
            .values()
            .filter(|e| e.serves(event))
            .map(|e| (e.manifest.id.clone(), e.manifest.strict))
            .collect()
    }

    /// The one hook-chain engine.
    ///
    /// Every chain — before/after middleware, first-wins slots, merge
    /// folds, fire-and-forget events — is this loop with a different fold.
    /// The engine owns everything that used to be hand-rolled per chain:
    /// subscriber selection, per-handler payload construction, the run +
    /// envelope parse, error logging, and short-circuiting. The fold owns
    /// only the event's own semantics: read the directive out of one
    /// envelope and fold it into the running value.
    ///
    /// - `seed` — the chain's initial accumulator (`()`, `Vec`, `Option`, …).
    /// - `done` — the accumulator is final: stop *before* running the next
    ///   handler (first-wins chains pass `Option::is_some`).
    /// - `payload` — one handler's request, built from the current
    ///   accumulator (threaded middleware folds its state back in).
    /// - `fold` — one handler's contribution: the extension id, its
    ///   `strict` flag, the parsed directive envelope (or the run error,
    ///   already logged), and the accumulator. `Continue` carries the
    ///   folded value; `Break` ends the chain with that value (deny,
    ///   strict failure, or the first explicit opinion).
    ///
    /// Errors fail open by contract: the engine logs and hands the error
    /// to the fold, which decides — skip (`Continue`) everywhere except
    /// `tool.before`, where `strict = true` denies. Chain failures surface
    /// through the shared warning channel (`dex_runtime::log!`), not bare
    /// stderr.
    async fn run_chain<T>(
        &self,
        event: &str,
        host: &HostCtx<'_>,
        seed: T,
        done: impl Fn(&T) -> bool,
        mut payload: impl FnMut(&T) -> serde_json::Value,
        mut fold: impl FnMut(
            &str,
            bool,
            Result<serde_json::Map<String, serde_json::Value>, String>,
            T,
        ) -> ControlFlow<T, T>,
    ) -> T {
        let mut acc = seed;
        for (id, strict) in self.subscribers(event).await {
            if done(&acc) {
                break;
            }
            let envelope = match self.run_event(&id, event, payload(&acc), host).await {
                Ok(json) => serde_json::from_str::<serde_json::Value>(&json)
                    .ok()
                    .and_then(|v| v.as_object().cloned())
                    .ok_or_else(|| "returned bad envelope".to_string()),
                Err(error) => Err(error),
            };
            if let Err(error) = &envelope {
                dex_runtime::log!(Warn, "extensions: '{id}' {event} failed: {error}");
            }
            acc = match fold(&id, strict, envelope, acc) {
                ControlFlow::Break(final_value) => return final_value,
                ControlFlow::Continue(next) => next,
            };
        }
        acc
    }

    /// First-wins boolean slot (`harness.overflow` / `harness.conflict` /
    /// `harness.compact`): the first handler naming `key` wins in load
    /// order; errors and absent keys fail open to `None` (the Rust default).
    async fn first_harness_bool(
        &self,
        event: &str,
        key: &str,
        payload: serde_json::Value,
        host: &HostCtx<'_>,
    ) -> Option<bool> {
        self.run_chain(
            event,
            host,
            None,
            Option::is_some,
            |_| payload.clone(),
            |_, _, envelope, _| match envelope
                .ok()
                .and_then(|m| hooks::parse_harness_bool(&m, key))
            {
                Some(v) => ControlFlow::Break(Some(v)),
                None => ControlFlow::Continue(None),
            },
        )
        .await
    }
}
impl ExtensionManager {
    /// `dex.tools.set_active` backing store (full names — the extension-facing
    /// `set_active_global` resolves short names first). Stored as-given:
    /// unknown names filter out at read time, once the tools they name enter
    /// the cache (a load-time call races the cache rebuild, so dropping here
    /// would wedge the schema to empty on a fresh process).
    pub async fn set_active(&self, tools: Vec<String>) {
        *self
            .active
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(tools);
    }

    /// The schema slice after the `set_active` filter.
    #[cfg(test)]
    pub async fn active_cached(&self) -> Vec<ToolDefinition> {
        let all = self.cached.read().await.clone();
        match self
            .active
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
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

/// `dex extensions install <dir|git-url>`: install an extension into the
/// user-scope dir. A local path is copied as-is; anything with a `://`
/// scheme (https://, file://, …) is treated as a remote source and shallow
/// git-cloned to a temp dir first. The manifest must parse — install
/// validates before copying so a broken extension never lands. A local
/// install returns the id; a remote install returns a ready-to-print
/// message noting that the extension stays disabled until explicitly
/// enabled (remote code is code the user has never audited).
pub fn install(src: &str) -> Result<String, String> {
    if src.contains("://") {
        install_remote(src)
    } else {
        install_dir(PathBuf::from(src))
            .map(|id| format!("installed '{id}' — run `dex extensions list`"))
    }
}

/// Remote sources (spec §32): `git clone --depth 1 <url>` into a private
/// temp dir, then the same validate+copy path as a local install. The
/// manifest must sit at the repo root (one extension per repo — a
/// multi-extension repo is a packaging concern for the repo author). The
/// temp dir is removed on every path out.
fn install_remote(url: &str) -> Result<String, String> {
    // Plain `http://` is refused: a MITM'd clone is remote code execution
    // by construction. `file://` stays allowed — it is a local repo, the
    // same trust level as `install <dir>` (and it keeps the clone path
    // testable offline).
    if !(url.starts_with("https://") || url.starts_with("git@") || url.starts_with("file://")) {
        return Err(format!(
            "unsupported remote source {url:?}: use an https:// or git@ URL"
        ));
    }
    let tmp = std::env::temp_dir().join(format!(
        "dex-ext-install-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    let result = (|| {
        let out = std::process::Command::new("git")
            .args(["clone", "--depth", "1", url])
            .arg(&tmp)
            .output()
            .map_err(|e| format!("running git: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git clone failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        if !tmp.join("manifest.yaml").is_file() {
            return Err(format!(
                "{url} has no manifest.yaml at the repo root (one extension per repo)"
            ));
        }
        let id = install_dir(tmp.clone())?;
        // Remote code is code the user has not audited: it does not load
        // until `dex extensions enable <id>` — the same explicit consent a
        // project-scope extension needs. (User-scope loads unless disabled,
        // so a bare install would arm it immediately.)
        super::discovery::set_enabled(&id, false)
            .map_err(|e| format!("marking '{id}' disabled: {e}"))?;
        Ok(format!(
            "installed '{id}' from {url} — it stays disabled until \
`dex extensions enable {id}` (remote code needs explicit consent)"
        ))
    })();
    std::fs::remove_dir_all(&tmp).ok();
    result
}

/// Local-dir install: validate the manifest, copy into the user-scope dir.
fn install_dir(src: PathBuf) -> Result<String, String> {
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
pub fn remove(id: &str) -> Result<(), String> {
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
