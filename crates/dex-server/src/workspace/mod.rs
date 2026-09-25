//! Workspace: paths, file-cache, atomic writes. Single owner of FS confinement.

pub mod atomic;
pub mod cache;
pub mod paths;

pub use atomic::unique_tmp_path;
pub use cache::{cached_parse, fnv_bytes, FileCache};
pub use paths::{
    normalize_conflict_path, resolve_workspace_path, workspace_path, workspace_root, xdg_path,
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
