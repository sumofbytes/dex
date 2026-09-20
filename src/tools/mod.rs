#![allow(clippy::doc_lazy_continuation)]
mod args;
mod audit;
mod dispatch;
mod edit;
pub(crate) mod error;
mod meta;
pub(crate) mod outcome;
pub(crate) mod policy;
mod read;
pub(crate) mod sandbox;
mod search;
mod shell;
pub(crate) mod then_run;
mod write;

// Workspace confinement lives in `sandbox.rs`; re-exported here so existing
// `tools::...` paths keep working.
pub(crate) use dispatch::{dispatch_original, execute, execute_outcome, execute_sync};
pub(crate) use error::ToolError;
pub(crate) use outcome::ToolOutcome;
pub(crate) use policy::{metadata, PermissionRequirement, ToolFilter};
pub(crate) use sandbox::{
    normalize_conflict_path, resolve_workspace_path, workspace_path, workspace_root,
};
// Leaf tool implementations live in per-tool modules; re-exported so
// existing `tools::...` paths keep working.
#[cfg(test)]
use edit::{apply_edit, apply_edit_batch, change_diff_async, parse_edit_ops};
pub(crate) use meta::Policy;
#[cfg(test)]
use read::expand_glob_in;
pub(crate) use shell::parse_shell_escape;
#[cfg(test)]
use shell::run_bash_with_limits;
pub(crate) use write::hash_file;

use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) static CONFIGURED_OUTPUT_LIMIT: AtomicUsize = AtomicUsize::new(1_048_576);

pub(crate) fn set_output_limit(limit: usize) {
    if limit > 0 {
        CONFIGURED_OUTPUT_LIMIT.store(limit, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;
