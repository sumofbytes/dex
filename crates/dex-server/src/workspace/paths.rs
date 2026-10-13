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
use std::sync::atomic::{AtomicBool, Ordering};

use super::WorkspaceError;

tokio::task_local! {
    static SESSION_ROOT: PathBuf;
}

static REQUIRE_SESSION_ROOT: AtomicBool = AtomicBool::new(false);

/// Make [`workspace_root`] fail outside [`with_workspace_root`] instead of
/// answering with the process cwd. A daemon serves many sessions from one
/// process, so an unscoped caller there is a bug (a task spawned without
/// [`spawn_in_workspace`]) that would otherwise touch the daemon's own tree
/// silently. Daemon-level code that really wants the process directory calls
/// [`process_cwd`].
pub fn require_session_root() {
    REQUIRE_SESSION_ROOT.store(true, Ordering::Relaxed);
}

/// The directory the process was started in; never session-scoped.
pub fn process_cwd() -> io::Result<PathBuf> {
    env::current_dir()
}

/// The workspace tools resolve against: the session's directory inside
/// [`with_workspace_root`], else the process cwd (or an error once
/// [`require_session_root`] is set).
pub fn workspace_root() -> io::Result<PathBuf> {
    root_or_fallback(REQUIRE_SESSION_ROOT.load(Ordering::Relaxed))
}

fn root_or_fallback(strict: bool) -> io::Result<PathBuf> {
    match SESSION_ROOT.try_with(PathBuf::clone) {
        Ok(root) => Ok(root),
        Err(_) if strict => Err(io::Error::other(
            "no session workspace in scope (task spawned without spawn_in_workspace?)",
        )),
        Err(_) => process_cwd(),
    }
}

/// Run `fut` with `root` as its [`workspace_root`]. Tasks it spawns must use
/// [`spawn_in_workspace`] to inherit the root.
pub async fn with_workspace_root<F: std::future::Future>(root: PathBuf, fut: F) -> F::Output {
    SESSION_ROOT.scope(root, fut).await
}

/// The session root in scope, if any (unlike [`workspace_root`], never the
/// process cwd). For handing the root to threads that tokio cannot scope.
pub fn scoped_root() -> Option<PathBuf> {
    SESSION_ROOT.try_with(PathBuf::clone).ok()
}

/// Run blocking `f` with `root` as its [`workspace_root`]; `None` leaves it
/// unscoped.
pub fn with_workspace_root_sync<R>(root: Option<PathBuf>, f: impl FnOnce() -> R) -> R {
    match root {
        Some(root) => SESSION_ROOT.sync_scope(root, f),
        None => f(),
    }
}

/// Wrap `fut` so it runs with the caller's [`workspace_root`]. For spawn
/// sites that are not [`tokio::spawn`], such as `JoinSet::spawn`.
pub fn in_workspace<F: std::future::Future>(
    fut: F,
) -> impl std::future::Future<Output = F::Output> {
    let root = SESSION_ROOT.try_with(PathBuf::clone).ok();
    async move {
        match root {
            Some(root) => SESSION_ROOT.scope(root, fut).await,
            None => fut.await,
        }
    }
}

/// [`tokio::spawn`] that keeps the caller's [`workspace_root`].
pub fn spawn_in_workspace<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(in_workspace(fut))
}

/// [`tokio::task::spawn_blocking`] that keeps the caller's [`workspace_root`].
pub fn spawn_blocking_in_workspace<F, R>(f: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    match SESSION_ROOT.try_with(PathBuf::clone) {
        Ok(root) => tokio::task::spawn_blocking(move || SESSION_ROOT.sync_scope(root, f)),
        Err(_) => tokio::task::spawn_blocking(f),
    }
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

#[cfg(test)]
mod session_root_tests {
    use super::*;
    use crate::runtime::console::CancellationToken;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!("dex-root-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[tokio::test]
    async fn scoped_root_drives_paths_bash_and_spawned_tasks() {
        let (a, b) = (temp_dir("a"), temp_dir("b"));
        let run = |root: PathBuf| async move {
            with_workspace_root(root.clone(), async move {
                assert_eq!(workspace_root().unwrap(), root);
                assert_eq!(tool_path("f.txt").unwrap(), root.join("f.txt"));
                let (out, _) = crate::tools::shell::run_bash("pwd", &CancellationToken::new())
                    .await
                    .unwrap();
                assert_eq!(PathBuf::from(out.trim()).canonicalize().unwrap(), root);
                let spawned = spawn_in_workspace(async { workspace_root().unwrap() })
                    .await
                    .unwrap();
                assert_eq!(spawned, root);
                let blocking = spawn_blocking_in_workspace(|| workspace_root().unwrap())
                    .await
                    .unwrap();
                assert_eq!(blocking, root);
                assert_eq!(root_or_fallback(true).unwrap(), root);
                let mut set = tokio::task::JoinSet::new();
                set.spawn(in_workspace(async { workspace_root().unwrap() }));
                assert_eq!(set.join_next().await.unwrap().unwrap(), root);
            })
            .await
        };
        // Concurrent sessions each see their own directory.
        tokio::join!(run(a.clone()), run(b.clone()));
        assert_eq!(workspace_root().unwrap(), env::current_dir().unwrap());
        // Unscoped under the daemon's strict mode is an error, not the cwd.
        assert!(root_or_fallback(true).is_err());
    }
}
