//! Workspace confinement: the single owner of "which paths may tools touch".
//!
//! Existing path components are canonicalized so symlinks cannot silently
//! escape the workspace; for a new file, the existing parent is
//! canonicalized instead.

use std::env;
use std::path::{Path, PathBuf};

use super::ToolError;

pub(crate) fn workspace_root() -> Result<PathBuf, ToolError> {
    env::current_dir().map_err(ToolError::Io)
}

/// Resolve a user-provided path within the current workspace. Existing path
/// components are canonicalized so symlinks cannot silently escape it; for a
/// new file, the existing parent is canonicalized instead.
pub(crate) fn workspace_path(raw: &str) -> Result<PathBuf, ToolError> {
    let root = workspace_root()?.canonicalize().map_err(ToolError::Io)?;
    resolve_workspace_path(&root, raw)
}

pub(crate) fn resolve_workspace_path(root: &Path, raw: &str) -> Result<PathBuf, ToolError> {
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        root.join(raw)
    };
    let resolved = if candidate.exists() {
        candidate.canonicalize().map_err(ToolError::Io)?
    } else {
        let file_name = candidate
            .file_name()
            .ok_or_else(|| ToolError::OutsideWorkspace(candidate.display().to_string()))?;
        let parent = candidate
            .parent()
            .unwrap_or(root)
            .canonicalize()
            .map_err(ToolError::Io)?;
        parent.join(file_name)
    };
    if resolved == root || resolved.starts_with(root) {
        Ok(resolved)
    } else {
        Err(ToolError::OutsideWorkspace(raw.to_string()))
    }
}

/// Normalize a tool-call `path` argument for same-path conflict detection:
/// `./foo.rs`, `src/../foo.rs` and `foo.rs` are the same file, but the raw
/// strings differ — the turn loop would then fan out two edits to one file
/// concurrently and the second edit would silently clobber the first.
/// Best-effort with known gaps: paths needing FS state we can't resolve
/// (missing parent dirs), case-only differences on case-insensitive FS,
/// and same-file writes via `bash` redirection (no `path` arg) still miss.
/// Unresolvable paths fall back to a lexical clean (no symlink resolution),
/// which still catches `./` and `a/../` spelling differences.
pub(crate) fn normalize_conflict_path(raw: &str) -> String {
    if let Ok(resolved) = workspace_path(raw) {
        return resolved.display().to_string();
    }
    lexical_normalize_fallback(raw)
}

/// Join `raw` against the workspace root and clean `.`/`..` lexically
/// without touching the FS. Catches `./newdir/f` vs `newdir/f` when the
/// parent doesn't exist yet (canonicalize would fail).
fn lexical_normalize_fallback(raw: &str) -> String {
    let raw_path = Path::new(raw);
    let joined = if raw_path.is_absolute() {
        raw_path.to_path_buf()
    } else {
        let root = workspace_root()
            .ok()
            .and_then(|r| r.canonicalize().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        root.join(raw_path)
    };
    let mut out = PathBuf::new();
    for comp in joined.components() {
        use std::path::Component as C;
        match comp {
            C::CurDir => {}
            C::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        raw.to_string()
    } else {
        out.display().to_string()
    }
}
