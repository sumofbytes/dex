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

pub(crate) use changes::undo_last_change;
// The message loaders only serve the TUI (remote reattach, `/resume`);
// headless builds compile them out.
#[cfg_attr(not(feature = "tui"), allow(unused_imports))]
pub(crate) use dex_session::{
    load_llm_messages_from_session, load_messages_from_session, load_session_state, Session,
    SessionHeader,
};

/// Single-scan reattach loader (TUI): messages plus the stored plan. The
/// crate returns the raw plan state value; deserializing into `Plan` is app
/// glue so the storage crate doesn't depend on the agent core.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
pub(crate) fn load_messages_and_plan(path: &Path) -> io::Result<(Vec<ChatMessage>, Plan)> {
    dex_session::load_messages_and_plan_raw(path).map(|(messages, plan)| {
        (
            messages,
            plan.map(|s| Plan::from_json(&s)).unwrap_or_default(),
        )
    })
}

// App-side callers don't load the plan standalone today (reattach rides the
// single-scan loader); kept for `/resume`-style consumers and tests.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn load_plan(path: &Path) -> Plan {
    dex_session::load_session_state(path)
        .ok()
        .and_then(|m| m.get("plan").cloned())
        .map(|s| Plan::from_json(&s))
        .unwrap_or_default()
}

// No production caller yet (plan writes go through the turn loop's state
// path); kept so the app-side plan representation has one save entry point.
#[allow(dead_code)]
pub(crate) fn save_plan(session: &mut Session, plan: &Plan) -> io::Result<()> {
    session.set_state("plan", &plan.to_json())
}

#[cfg(test)]
mod tests;
