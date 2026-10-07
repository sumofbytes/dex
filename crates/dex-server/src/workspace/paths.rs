//! Tool paths: how a tool `path` argument becomes a filesystem path.
//!
//! Tools are not confined to the workspace: `bash` can reach anything the
//! user can, so a path gate on `read`/`write`/`edit` only forces detours.
//! Run dex in a sandbox or container to restrict what it can touch.
//! [`resolve_workspace_path`] stays confined for Lua extensions, whose
//! `workspace.read` capability promises that scope.

use std::env;
use std::io;
use std::path::{Path, PathBuf};

use super::WorkspaceError;

pub fn workspace_root() -> io::Result<PathBuf> {
    env::current_dir()
}

/// Resolve a tool `path` argument: `~` expands to `$HOME`, relative paths
/// join the workspace root. Existing paths are canonicalized (for a new
/// file, its existing parent) so caches and conflict detection see one
/// spelling per file.
pub fn tool_path(raw: &str) -> io::Result<PathBuf> {
    resolve_path(&workspace_root()?.canonicalize()?, raw)
}

/// [`tool_path`] against an explicit `root`.
pub fn resolve_path(root: &Path, raw: &str) -> io::Result<PathBuf> {
    let candidate = root.join(expand_home(raw));
    if candidate.exists() {
        return candidate.canonicalize();
    }
    let file_name = candidate.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a file path: {raw}"),
        )
    })?;
    let parent = candidate.parent().unwrap_or(root).canonicalize()?;
    Ok(parent.join(file_name))
}

/// [`resolve_path`] confined to `root`: symlinks resolve to their targets,
/// so a link out of `root` is refused too.
pub fn resolve_workspace_path(root: &Path, raw: &str) -> Result<PathBuf, WorkspaceError> {
    let resolved = resolve_path(root, raw).map_err(|e| match e.kind() {
        io::ErrorKind::InvalidInput => WorkspaceError::OutsideWorkspace(raw.to_string()),
        _ => WorkspaceError::Io(e),
    })?;
    if resolved.starts_with(root) {
        Ok(resolved)
    } else {
        Err(WorkspaceError::OutsideWorkspace(raw.to_string()))
    }
}

fn expand_home(raw: &str) -> PathBuf {
    let rest = match raw.strip_prefix('~') {
        Some("") => "",
        Some(rest) if rest.starts_with('/') => &rest[1..],
        _ => return PathBuf::from(raw),
    };
    match env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(rest),
        None => PathBuf::from(raw),
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
pub fn normalize_conflict_path(raw: &str) -> String {
    if let Ok(resolved) = tool_path(raw) {
        return resolved.display().to_string();
    }
    lexical_normalize_fallback(raw)
}

/// Join `raw` against the workspace root and clean `.`/`..` lexically
/// without touching the FS. Catches `./newdir/f` vs `newdir/f` when the
/// parent doesn't exist yet (canonicalize would fail).
pub fn lexical_normalize_fallback(raw: &str) -> String {
    let raw_path = expand_home(raw);
    let joined = if raw_path.is_absolute() {
        raw_path
    } else {
        let root = workspace_root()
            .ok()
            .and_then(|r| r.canonicalize().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        root.join(&raw_path)
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

/// Resolve `rel` under `$env_var`, falling back to `$HOME/home_sub/rel.`
pub fn xdg_path(env_var: &str, home_sub: &str, rel: &str) -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os(env_var) {
        return Some(std::path::PathBuf::from(dir).join(rel));
    }
    std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(home_sub).join(rel))
}
