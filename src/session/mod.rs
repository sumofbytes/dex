#![allow(dead_code, unused_variables, unused_imports)]
pub(crate) mod changes;
pub(crate) mod discovery;
pub(crate) mod events;
pub(crate) mod header;
pub(crate) mod store;

// The undo ledger lives in `changes.rs`, the events journal in `events.rs`;
// re-exported here so existing `session::...` paths keep working.
pub(crate) use changes::{
    load_changes, make_change_record, record_change, save_changes, undo_last_change, ChangeRecord,
};
pub(crate) use events::{events_cache, EVENTS_PAGE_LIMIT};
pub(crate) use header::{file_id, FileId, PathCache, SessionHeader};
pub(crate) use store::{
    load_llm_messages_from_session, load_messages_and_plan, load_messages_from_session,
    load_messages_from_session_async, load_plan, load_session_state, save_plan, Session,
};

/// Serializes tests that redirect XDG_DATA_HOME (it decides where ALL
/// sessions live, including other tests' fixtures).
#[cfg(test)]
pub(crate) static TEST_SESSIONS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Panic-safe env restore for tests: saved on construction, reverted on drop
/// even when the test panics, so a failed test can't leak vars into others
/// running in the same process. Take it while holding TEST_SESSIONS_ENV_LOCK.
#[cfg(test)]
pub(crate) struct EnvGuard(pub Vec<(&'static str, Option<std::ffi::OsString>)>);

#[cfg(test)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, prev) in self.0.drain(..) {
            match prev {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
