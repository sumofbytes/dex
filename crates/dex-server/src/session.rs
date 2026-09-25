//! Session JSONL machinery lives in the `dex-session` crate (header,
//! journals, state, history loaders). This module is the app-side
//! composition layer: the re-exports keep the historical `crate::session::`
//! paths working, and the plan helpers live here because `Plan` belongs to
//! the agent core, not to the storage crate.

use std::io;
use std::path::Path;

use dex_ai::ChatMessage;

use crate::protocol::Plan;

pub(crate) mod changes;

pub use changes::undo_last_change;
// The message loaders only serve the TUI (remote reattach, `/resume`);
// headless builds compile them out.
#[cfg_attr(not(feature = "tui"), allow(unused_imports))]
pub use dex_session::{
    load_llm_messages_from_session, load_messages_from_session, load_session_state, Session,
    SessionHeader,
};

/// Single-scan reattach loader (TUI): messages plus the stored plan. The
/// crate returns the raw plan state value; deserializing into `Plan` is app
/// glue so the storage crate doesn't depend on the agent core.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
pub fn load_messages_and_plan(path: &Path) -> io::Result<(Vec<ChatMessage>, Plan)> {
    dex_session::load_messages_and_plan_raw(path).map(|(messages, plan)| {
        (
            messages,
            plan.map(|s| Plan::from_json(&s)).unwrap_or_default(),
        )
    })
}

// App-side callers don't load the plan standalone today (reattach rides the
// single-scan loader); test-only.
#[cfg(test)]
pub fn load_plan(path: &Path) -> Plan {
    dex_session::load_session_state(path)
        .ok()
        .and_then(|m| m.get("plan").cloned())
        .map(|s| Plan::from_json(&s))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
