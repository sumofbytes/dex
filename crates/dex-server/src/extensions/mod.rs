//! Lua harness extensions: directory discovery, process-global manager.
//!
//! Mirrors `mcp.rs`: loaded once at daemon bootstrap (not per turn), with
//! `cached_tools()` / `cached_schema_tokens()` / `call_global()` sync
//! surfaces for the schema + dispatch paths. A failed extension is skipped
//! whole — its tools never enter the schema.

#[cfg(test)]
use std::path::PathBuf;

mod engine;
pub(crate) mod hooks;
mod manifest;

pub(crate) use engine::{
    answer_hostcall, CallKind, ExtensionEngine, HostCtx, HostOp, WorkerMsg, HOOK_TIMEOUT_SECS,
    SLOW_HOOK_WARN,
};
#[cfg(test)]
pub(crate) use manifest::parse_manifest;
pub(crate) use manifest::MAX_TOOL_TIMEOUT_SECS;
/// `dex.net.fetch` ceilings: per-request timeout cap (matches the tool
/// budget) and response-body cap (a runaway body fails the call, not the
/// daemon).
pub(crate) const MAX_NET_TIMEOUT_MS: u64 = 120_000;
pub(crate) const MAX_NET_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
pub(crate) use hooks::{
    AfterOutcome, BeforeOutcome, CompactAction, PermissionDecision, SupervisorAction,
};
pub(crate) use manifest::Manifest;

#[cfg(test)]
use std::collections::{BTreeMap, HashSet};
#[cfg(test)]
use std::sync::Arc;

mod appendix;
mod discovery;
mod global;
mod manager;
mod state;
/// Invocation audit ring, printed by `dex runtime trace` (spec §35).
pub mod trace;
// `summary_line` is the `/extensions` sheet (TUI) consumer only.
#[cfg_attr(not(feature = "tui"), allow(unused_imports))]
pub use discovery::{discovered_extensions, list_command, summary_line};
#[cfg(test)]
pub(crate) use global::resolve_active_name;
pub use global::text_preview;
pub use global::{
    call_shadow_global, has_event_handlers, is_shadowed, loaded_summaries, set_active_global,
    tools_list,
};
pub use manager::{install, remove};

#[cfg(test)]
use appendix::full_tool_name;
pub use appendix::{
    is_extension_tool, normalize_tool_name, prompt_appendix, push_prompt_appendix, split_ext_name,
};
#[cfg(test)]
use appendix::{remove_prompt_appendix, PROMPT_APPENDIX};
pub use discovery::{config_extension_paths, set_enabled, set_extra_dirs};
#[cfg(test)]
use discovery::{data_extensions_dir, parse_config_paths};
#[cfg(test)]
pub(crate) use discovery::{scoped_extension_dirs, user_extensions_dir, Scope};
pub use global::{
    agent_loop_global, apply_after_hooks, apply_before_agent_start, apply_before_compact,
    apply_before_hooks, apply_llm_before, cached_schema_tokens, cached_tools, call_global,
    command_list, current_drive_model, drive_model_for, fire_event_global, fire_lifecycle_event,
    fire_model_select_if_changed, global_manager, net_fetch, query_harness_compact,
    query_harness_conflict, query_harness_overflow, query_harness_summarize,
    query_model_selector_global, query_permission_request, query_supervisor_route,
    query_tool_catalog_global, run_command_global, served_model_snapshot, with_drive_model,
    DriveModel,
};
#[cfg(test)]
use global::{current_routing_headers, harvest_routing_headers, with_routing_headers};
#[cfg(test)]
use global::{LAST_MODEL, LAST_ROUTING_HEADERS};
#[cfg(test)]
use state::STATE;
pub(crate) use state::{state_get, state_set};

#[cfg(test)]
pub(crate) mod tests;
