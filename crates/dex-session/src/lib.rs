//! Append-only JSONL session store: header line, message/events journals,
//! key/value session state, history loaders, and session discovery.
//!
//! App-side composition (plan persistence, the undo ledger, test-env
//! helpers) lives in the `dex` crate, which re-exports this machinery under
//! its `session` module.

pub mod discovery;
pub mod events;
pub mod header;
pub mod store;

pub use header::{file_id, FileId, PathCache, SessionHeader};
pub use store::{
    load_llm_messages_from_session, load_messages_and_plan_raw, load_messages_from_session,
    load_session_state, Session,
};

/// Where session data lives (`XDG_DATA_HOME`, else `~/.local/share`).
/// Mirrors `runtime::logging::data_home` in the app crate; kept duplicate
/// (3 lines) so this crate stays dependency-free of app plumbing.
pub(crate) fn data_home() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })
}

/// Serializes tests that redirect XDG_DATA_HOME (it decides where ALL
/// sessions live, including other tests' fixtures). Only visible when this
/// crate is tested in-tree; the app crate keeps its own copy in
/// `crate::test_env`.
#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) static TEST_SESSIONS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Panic-safe env restore for tests: saved on construction, reverted on
    /// drop even when the test panics, so a failed test can't leak vars into
    /// others running in the same process. Take it while holding
    /// [`TEST_SESSIONS_ENV_LOCK`].
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
}
