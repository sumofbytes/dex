//! Workspace: tool path resolution, file-cache, atomic writes.

pub mod atomic;
pub mod cache;
pub mod paths;

pub use atomic::unique_tmp_path;
pub use cache::{cached_parse, fnv_bytes, FileCache};
pub use paths::{
    in_workspace, normalize_conflict_path, process_cwd, require_session_root, resolve_path,
    resolve_workspace_path, scoped_root, spawn_blocking_in_workspace, spawn_in_workspace,
    tool_path, with_workspace_root, with_workspace_root_sync, workspace_root, xdg_path,
};

#[derive(Debug)]
pub enum WorkspaceError {
    Io(std::io::Error),
    OutsideWorkspace(String),
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::OutsideWorkspace(p) => write!(f, "path escapes workspace: {p}"),
        }
    }
}

impl std::error::Error for WorkspaceError {}
