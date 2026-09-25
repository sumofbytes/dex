//! Test-only env helpers shared across dex's test modules: sessions (and
//! anything resolving user paths) follow `XDG_DATA_HOME`, so tests that
//! redirect it must serialize and must not leak vars on panic. These can't
//! live in `dex-session` — `cfg(test)` items aren't visible to the
//! dependent crate's test binary.

/// Serializes tests that redirect XDG_DATA_HOME (it decides where ALL
/// sessions live, including other tests' fixtures).
pub static TEST_SESSIONS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Panic-safe env restore for tests: saved on construction, reverted on
/// drop even when the test panics, so a failed test can't leak vars into
/// others running in the same process. Take it while holding
/// [`TEST_SESSIONS_ENV_LOCK`].
pub struct EnvGuard(pub Vec<(&'static str, Option<std::ffi::OsString>)>);

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
