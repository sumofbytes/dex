//! Process-global runtime shared by every dex crate: console/TTY handling,
//! logging (`log!`), shared async HTTP client pools, cancellation, dedup'd
//! warnings (`notice`), unwind guards, and headless text formatting.
//!
//! This crate is intentionally small and dependency-light: it is the base
//! of the dependency graph, so both the server (`dex-server`) and the thin
//! client (`dex`) can link it.

pub mod runtime;
pub use runtime::*;

/// Test-only env helpers shared across dex's test modules: sessions (and
/// anything resolving user paths) follow `XDG_DATA_HOME`, so tests that
/// redirect it must serialize and must not leak vars on panic. These can't
/// live in `dex-session` — `cfg(test)` items aren't visible to the
/// dependent crate's test binary.
pub mod test_env;
