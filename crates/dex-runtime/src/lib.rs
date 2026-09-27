//! Process-global runtime shared by every dex crate: console/TTY handling,
//! logging (`log!`), shared async HTTP client pools, cancellation, dedup'd
//! warnings (`notice`), unwind guards, and headless text formatting.
//!
//! This crate sits at the base of the dependency graph, so both the server
//! (`dex-server`) and the thin client (`dex`) can link it. It is small, but
//! not dependency-free: it pulls `dex-ai` (the `CancellationSource` contract
//! the console and agent loop share) and `dex-agent-core` (the
//! transcript/approval vocabulary the console emits, from its `lines`
//! module), plus `reqwest` and full-feature `tokio`. The vocabulary's data
//! lives in `dex-agent-core`; this crate no longer depends on `dex-protocol`.

pub mod runtime;
pub use runtime::*;

/// Test-only env helpers shared across dex's test modules: sessions (and
/// anything resolving user paths) follow `XDG_DATA_HOME`, so tests that
/// redirect it must serialize and must not leak vars on panic. These can't
/// live in `dex-session` — `cfg(test)` items aren't visible to the
/// dependent crate's test binary.
pub mod test_env;
