//! Harness-slot registry + resolver — Phase 1 of
//! `docs/runtime-composability-spec.md`.
//!
//! Named implementations register under a slot (`register`); selection picks
//! one by id (`lookup` / config `harness.slots:`); `replace` is re-registering
//! the same slot+id. The resolver runs once at turn start (where the
//! `Arc<DexHarness>` snapshot is already taken) and applies every config
//! selection on top of [`DexHarness::from_env`], so the snapshot semantics are
//! unchanged: one immutable harness per turn.
//!
//! Built-ins register the selectable alternatives that exist today
//! (`catalog: native-only`, `trigger: never`); everything else is a single
//! Rust default reachable via `DexHarness::from_env`. Selecting an id
//! `default` is a no-op (keeps the env-derived defaults, e.g. the
//! `harness:`-table prune thresholds on the scorer). Unknown slots or ids
//! `warn_once` and fall back to the default — never fail a turn.

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use dex_agent_core::{
    CompactionTrigger, ConflictDetector, NeverCompact, OverflowDetector, PruneScorer, Summarizer,
    ToolCatalog,
};
use dex_coding_agent::ApprovalPolicy;

use crate::agent::composable::{
    ComposedCatalog, DexHarness, EventSink, RecoveryPolicy, ToolExecutor, TranscriptStore,
    UsageReporter,
};
use crate::runtime::notice::warn_once;

/// Every overwritable [`DexHarness`] slot, in bundle-field order.
pub const SLOTS: &[&str] = &[
    "catalog",
    "trigger",
    "summarizer",
    "overflow",
    "conflict",
    "scorer",
    "approval",
    "executor",
    "transcript",
    "usage",
    "events",
    "recovery",
];

type Registry = BTreeMap<&'static str, BTreeMap<String, Arc<dyn Any + Send + Sync>>>;

fn registry() -> &'static RwLock<Registry> {
    static REGISTRY: OnceLock<RwLock<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// `register` an implementation under `slot`/`id`. Re-registering the same
/// slot+id overwrites — that is the Rust form of `dex.replace`.
/// Panics only on a poisoned registry lock.
pub fn register<T: ?Sized + Any + Send + Sync>(slot: &'static str, id: &str, impl_: Arc<T>) {
    assert!(
        SLOTS.contains(&slot),
        "registry: unknown slot '{slot}' (known: {})",
        SLOTS.join(", ")
    );
    // Wrap once so the payload's concrete type is exactly `Arc<T>` (the
    // slot's trait object), which is what `lookup` downcasts back to.
    let payload = Arc::new(impl_) as Arc<dyn Any + Send + Sync>;
    registry()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .entry(slot)
        .or_default()
        .insert(id.to_string(), payload);
}

/// Look up the implementation selected for `slot`/`id`, downcast to the
/// slot's trait object type.
pub fn lookup<T: ?Sized + Any + Send + Sync>(slot: &str, id: &str) -> Option<Arc<T>> {
    let reg = registry().read().unwrap_or_else(|e| e.into_inner());
    let boxed = reg.get(slot)?.get(id)?.clone();
    boxed.downcast::<Arc<T>>().ok().map(Arc::unwrap_or_clone)
}

/// The selectable alternatives that ship with dex. Idempotent.
fn register_builtins() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        register(
            "catalog",
            "native-only",
            Arc::new(ComposedCatalog::with_sources(Vec::new())) as Arc<dyn ToolCatalog>,
        );
        register(
            "trigger",
            "never",
            Arc::new(NeverCompact) as Arc<dyn CompactionTrigger>,
        );
    });
}

/// The Lua-activated harness profile (`dex.activate_profile`), if any.
/// Validated at resolution time: an unknown name warns and keeps the
/// config-selected (or no) profile — activation never half-applies.
static ACTIVE_PROFILE: Mutex<Option<String>> = Mutex::new(None);

/// `dex.activate_profile(name)` — transactional profile activation: the
/// name is stored now, validated against the config's `harness_profiles:`
/// when the next turn snapshot resolves (an unknown name applies nothing).
pub fn set_active_profile(name: Option<String>) {
    *ACTIVE_PROFILE.lock().unwrap_or_else(|e| e.into_inner()) = name;
}

pub fn active_profile() -> Option<String> {
    ACTIVE_PROFILE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Whether `slot`/`id` has a registered implementation.
pub fn has(slot: &str, id: &str) -> bool {
    registry()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(slot)
        .is_some_and(|m| m.contains_key(id))
}

/// One resolved slot row for `dex runtime graph` / `dex doctor`.
#[derive(Clone, Debug)]
pub struct SlotRow {
    pub slot: &'static str,
    /// Config-selected id, `None` when the slot was not selected.
    pub selection: Option<String>,
    /// `false` when the selection named an unknown id and the default kept.
    pub applied: bool,
    /// Profile this slot's selection came from, if any.
    pub profile: Option<String>,
}

impl SlotRow {
    pub fn resolved_id(&self) -> &str {
        if self.applied {
            self.selection.as_deref().unwrap_or("default")
        } else {
            "default"
        }
    }

    pub fn origin(&self) -> String {
        if let Some(profile) = &self.profile {
            return format!("profile '{profile}'");
        }
        match self.selection {
            Some(_) if self.applied => "config harness.slots".to_string(),
            Some(_) => "config harness.slots (unknown id — default kept)".to_string(),
            None => "built-in default".to_string(),
        }
    }
}

/// Resolve the turn's harness snapshot: `DexHarness::from_env` plus every
/// `harness.slots:` selection from the config file. This is the resolver the
/// turn loop calls where it already took its per-turn snapshot.
pub fn resolve_snapshot() -> Arc<DexHarness> {
    resolve_snapshot_with_origins().0
}

/// [`resolve_snapshot`] plus the per-slot rows for `dex runtime graph` and
/// `dex doctor`.
pub fn resolve_snapshot_with_origins() -> (Arc<DexHarness>, Vec<SlotRow>) {
    register_builtins();
    let mut harness = DexHarness::from_env();
    let mut selections: BTreeMap<String, String> = BTreeMap::new();
    for (slot, id) in crate::llm::config::load_harness_slots() {
        if !SLOTS.contains(&slot.as_str()) {
            warn_once(
                "harness.slot.unknown",
                &format!(
                    "config harness.slots: unknown slot '{slot}' — ignoring (known slots: {})",
                    SLOTS.join(", ")
                ),
            );
            continue;
        }
        selections.insert(slot, id);
    }
    // Harness profile (spec §30): the Lua activation wins over the config
    // selection; the profile's slot map is applied on top of `harness.slots`.
    // Transactional: every slot and id in the profile is validated before
    // anything applies — an invalid profile changes nothing.
    let (config_profile, profiles) = crate::llm::config::load_harness_profiles();
    let mut profile: Option<(String, BTreeMap<String, String>)> = None;
    let profile_name = active_profile().or(config_profile);
    if let Some(name) = profile_name {
        match profiles.get(&name) {
            Some(map) if map.is_empty() => {}
            Some(map) => {
                let valid = map.iter().all(|(slot, id)| {
                    SLOTS.contains(&slot.as_str()) && (id == "default" || has(slot, id))
                });
                if valid {
                    profile = Some((name.clone(), map.clone()));
                } else {
                    warn_once(
                        "harness.profile.invalid",
                        &format!(
                            "harness profile '{name}': unknown slot or unregistered id — profile not applied"
                        ),
                    );
                }
            }
            None => warn_once(
                "harness.profile.unknown",
                &format!("harness profile '{name}' is not defined in harness_profiles: — ignoring"),
            ),
        }
    }
    let profile_touched: std::collections::HashSet<String> = profile
        .as_ref()
        .map(|(_, map)| map.keys().cloned().collect())
        .unwrap_or_default();
    if let Some((_, map)) = &profile {
        for (slot, id) in map {
            selections.insert(slot.clone(), id.clone());
        }
    }
    // One row per slot, in bundle order — unselected slots report `default`.
    let mut rows = Vec::new();
    for name in SLOTS {
        let from_profile = profile_touched.contains(*name);
        let Some(id) = selections.remove(*name) else {
            rows.push(SlotRow {
                slot: name,
                selection: None,
                applied: true,
                profile: None,
            });
            continue;
        };
        if id == "default" {
            // Explicit default: keep whatever `from_env` built (its scorer
            // carries the `harness:` thresholds).
            rows.push(SlotRow {
                slot: name,
                selection: None,
                applied: true,
                profile: from_profile.then(|| profile_name_display(&profile)),
            });
            continue;
        }
        let applied = apply_slot(&mut harness, name, &id);
        if !applied {
            warn_once(
                "harness.slot.id",
                &format!(
                    "config harness.slots: no '{id}' implementation for slot '{name}' — keeping the default"
                ),
            );
        }
        rows.push(SlotRow {
            slot: name,
            selection: Some(id),
            applied,
            profile: from_profile.then(|| profile_name_display(&profile)),
        });
    }
    (Arc::new(harness), rows)
}

fn profile_name_display(profile: &Option<(String, BTreeMap<String, String>)>) -> String {
    profile
        .as_ref()
        .map(|(name, _)| name.clone())
        .unwrap_or_default()
}

/// Apply one selection to the bundle. `false` = no such id registered.
fn apply_slot(harness: &mut DexHarness, slot: &str, id: &str) -> bool {
    match slot {
        "catalog" => lookup::<dyn ToolCatalog>(slot, id).map(|v| harness.catalog = v),
        "trigger" => lookup::<dyn CompactionTrigger>(slot, id).map(|v| harness.trigger = Some(v)),
        "summarizer" => lookup::<dyn Summarizer>(slot, id).map(|v| harness.summarizer = Some(v)),
        "overflow" => lookup::<dyn OverflowDetector>(slot, id).map(|v| harness.overflow = v),
        "conflict" => lookup::<dyn ConflictDetector>(slot, id).map(|v| harness.conflict = v),
        "scorer" => lookup::<dyn PruneScorer>(slot, id).map(|v| harness.scorer = v),
        "approval" => lookup::<dyn ApprovalPolicy>(slot, id).map(|v| harness.approval = Some(v)),
        "executor" => lookup::<dyn ToolExecutor>(slot, id).map(|v| harness.executor = v),
        "transcript" => lookup::<dyn TranscriptStore>(slot, id).map(|v| harness.transcript = v),
        "usage" => lookup::<dyn UsageReporter>(slot, id).map(|v| harness.usage = v),
        "events" => lookup::<dyn EventSink>(slot, id).map(|v| harness.events = v),
        "recovery" => lookup::<dyn RecoveryPolicy>(slot, id).map(|v| harness.recovery = v),
        _ => None,
    }
    .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Custom overflow impl for the register/lookup roundtrip.
    struct AlwaysOverflow;
    impl OverflowDetector for AlwaysOverflow {
        fn is_overflow(&self, _message: &str) -> bool {
            true
        }
    }

    fn test_config_file(body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dex-registry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), body).unwrap();
        dir
    }

    #[test]
    fn register_and_lookup_roundtrip() {
        register(
            "overflow",
            "test-always",
            Arc::new(AlwaysOverflow) as Arc<dyn OverflowDetector>,
        );
        let got = lookup::<dyn OverflowDetector>("overflow", "test-always").unwrap();
        assert!(got.is_overflow("anything"));
        assert!(lookup::<dyn OverflowDetector>("overflow", "missing").is_none());
        assert!(lookup::<dyn OverflowDetector>("no-such-slot", "test-always").is_none());
    }

    #[test]
    fn builtins_give_catalog_two_implementations() {
        register_builtins();
        let names = |catalog: Arc<dyn ToolCatalog>| {
            catalog
                .tool_schemas()
                .into_iter()
                .map(|t| t.function.name)
                .collect::<Vec<_>>()
        };
        // native-only serves exactly the native schema (empty dynamic tail).
        let native_names = {
            let native_only = lookup::<dyn ToolCatalog>("catalog", "native-only").unwrap();
            names(native_only)
        };
        assert_eq!(
            native_names,
            names(Arc::new(ComposedCatalog::with_sources(Vec::new())))
        );
        // ...which selects away the dynamic tail; the composed default would
        // add MCP/extension tools when caches are populated (env-dependent, so
        // not asserted here).
        let _ = names(Arc::new(ComposedCatalog::dex_default()));
        // trigger: never never compacts.
        let never = lookup::<dyn CompactionTrigger>("trigger", "never").unwrap();
        assert!(!never.should_compact(u64::MAX, u64::MAX, usize::MAX));
    }

    #[test]
    fn resolve_applies_config_selection_and_falls_back_on_unknowns() {
        let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev_config = std::env::var_os("DEX_CONFIG");
        let dir = test_config_file(
            "harness:\n  slots:\n    catalog: native-only\n    trigger: never\n    summarizer: nope\n    bogus_slot: x\n",
        );
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        crate::llm::config::invalidate_config_cache();
        let (harness, rows) = resolve_snapshot_with_origins();
        // Selections applied: the catalog tail is gone and the trigger override fires.
        assert!(harness.trigger.is_some());
        assert!(!harness.should_compact(
            &crate::llm::config::tests::test_cfg(),
            u64::MAX,
            u64::MAX,
            usize::MAX
        ));
        let row = |slot: &str| rows.iter().find(|r| r.slot == slot).cloned();
        let catalog = row("catalog").unwrap();
        assert_eq!(catalog.selection.as_deref(), Some("native-only"));
        assert!(catalog.applied);
        assert_eq!(catalog.origin(), "config harness.slots");
        // Unknown id: default kept, loudly marked.
        let summarizer = row("summarizer").unwrap();
        assert_eq!(summarizer.resolved_id(), "default");
        assert_eq!(
            summarizer.origin(),
            "config harness.slots (unknown id — default kept)"
        );
        // Unknown slot: no row at all.
        assert!(row("bogus_slot").is_none());
        match prev_config {
            Some(v) => std::env::set_var("DEX_CONFIG", v),
            None => std::env::remove_var("DEX_CONFIG"),
        }
        crate::llm::config::invalidate_config_cache();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn explicit_default_selection_is_a_noop() {
        let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev_config = std::env::var_os("DEX_CONFIG");
        let dir = test_config_file("harness:\n  slots:\n    catalog: default\n");
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        crate::llm::config::invalidate_config_cache();
        let (_, rows) = resolve_snapshot_with_origins();
        let catalog = rows.iter().find(|r| r.slot == "catalog").unwrap();
        assert_eq!(catalog.selection, None);
        assert!(catalog.applied);
        match prev_config {
            Some(v) => std::env::set_var("DEX_CONFIG", v),
            None => std::env::remove_var("DEX_CONFIG"),
        }
        crate::llm::config::invalidate_config_cache();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A valid profile applies its slot map on top of `harness.slots:` with
    /// profile origin; an invalid one (unknown id) applies nothing —
    /// transactional (spec §30).
    #[test]
    fn harness_profiles_apply_transactionally() {
        let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        set_active_profile(None);
        let prev_config = std::env::var_os("DEX_CONFIG");
        let profile_rows = |body: &str| {
            let dir = test_config_file(body);
            std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
            crate::llm::config::invalidate_config_cache();
            let rows = resolve_snapshot_with_origins().1;
            std::env::set_var("DEX_CONFIG", prev_config.clone().unwrap_or_default());
            crate::llm::config::invalidate_config_cache();
            let _ = std::fs::remove_dir_all(&dir);
            rows
        };
        // Valid profile: overrides harness.slots, marks its origin.
        let rows = profile_rows(
            "harness:\n  slots:\n    catalog: native-only\nharness_profile: lean\nharness_profiles:\n  lean:\n    trigger: never\n",
        );
        let trigger = rows.iter().find(|r| r.slot == "trigger").unwrap();
        assert_eq!(trigger.selection.as_deref(), Some("never"));
        assert_eq!(trigger.origin(), "profile 'lean'");
        // The non-profile selection keeps its own origin.
        let catalog = rows.iter().find(|r| r.slot == "catalog").unwrap();
        assert_eq!(catalog.origin(), "config harness.slots");
        // Invalid profile (unknown id): nothing from the profile applies —
        // the harness.slots selection survives untouched.
        let rows = profile_rows(
            "harness:\n  slots:\n    catalog: native-only\nharness_profile: broken\nharness_profiles:\n  broken:\n    trigger: no-such-id\n",
        );
        let trigger = rows.iter().find(|r| r.slot == "trigger").unwrap();
        assert_eq!(trigger.selection, None);
        assert_ne!(trigger.origin(), "profile 'broken'");
        // Unknown profile name: ignored wholesale.
        let rows = profile_rows("harness_profile: ghost\n");
        assert!(rows.iter().all(|r| r.profile.is_none()));
        match prev_config {
            Some(v) => std::env::set_var("DEX_CONFIG", v),
            None => std::env::remove_var("DEX_CONFIG"),
        }
        crate::llm::config::invalidate_config_cache();
    }

    /// `dex.activate_profile` stores the name; the next resolution validates
    /// it against the config's profiles and applies on match.
    #[test]
    fn lua_profile_activation_is_validated_at_resolution() {
        let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev_config = std::env::var_os("DEX_CONFIG");
        let dir = test_config_file("harness_profiles:\n  lean:\n    trigger: never\n");
        std::env::set_var("DEX_CONFIG", dir.join("config.yaml"));
        crate::llm::config::invalidate_config_cache();
        set_active_profile(Some("lean".to_string()));
        let (_, rows) = resolve_snapshot_with_origins();
        let trigger = rows.iter().find(|r| r.slot == "trigger").unwrap();
        assert_eq!(trigger.origin(), "profile 'lean'");
        // Deactivation drops it.
        set_active_profile(None);
        let (_, rows) = resolve_snapshot_with_origins();
        assert!(rows.iter().all(|r| r.profile.is_none()));
        match prev_config {
            Some(v) => std::env::set_var("DEX_CONFIG", v),
            None => std::env::remove_var("DEX_CONFIG"),
        }
        crate::llm::config::invalidate_config_cache();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
