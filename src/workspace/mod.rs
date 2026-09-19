//! Workspace: paths, file-cache, atomic writes. Single owner of FS confinement.

pub(crate) mod atomic;
pub(crate) mod cache;
pub(crate) mod paths;

pub(crate) use atomic::unique_tmp_path;
pub(crate) use cache::{cached_parse, fnv_bytes, FileCache};
pub(crate) use paths::{
    normalize_conflict_path, resolve_workspace_path, workspace_path, workspace_root, xdg_path,
};

#[derive(Debug)]
pub(crate) enum WorkspaceError {
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
