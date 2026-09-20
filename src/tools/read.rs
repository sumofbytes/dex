//! File reading: single, fan-out, and glob expansion.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use tokio::io::AsyncReadExt as _;

use crate::runtime::cancel::wait_cancelled;
use crate::ui::format::clamp_lines;

use super::args::arg_str;
use super::{resolve_workspace_path, workspace_path, workspace_root, ToolError};

/// Multi-file read caps: enough for the "search, then read the hits" pattern
/// in one call, small enough that a fan-out cannot flood the context.
pub(crate) const READ_MAX_LINES: usize = 2_000;
pub(crate) const READ_MAX_BYTES: usize = 256 * 1024;
pub(crate) const READ_FANOUT_MAX_FILES: usize = 10;
pub(crate) const READ_FANOUT_GLOB_MAX_FILES: usize = 8;
/// Cap on buffered `find -print0` output before splitting: a huge monorepo
/// lists megabytes of names and `wait_with_output` already collected them,
/// so this bounds the parse (not the walk — the 30 s timeout bounds that).
/// 1 MiB holds ~10k typical paths, far above the truncate window below.
pub(crate) const FIND_GLOB_STDOUT_CAP: usize = 1024 * 1024;
pub(crate) const READ_FANOUT_PER_FILE_LINES: usize = 200;

/// `read` returns line-numbered content (right-aligned number + two-space gap
/// + tab-expanded content per .editorconfig/language) so subsequent `edit`
/// oldText anchors are cheap to construct. Default: first 2000 lines, capped
/// by a byte budget; paginate with offset/limit. Binary files are refused
/// rather than dumped into the context window.
///
/// Round-trip reduction: `paths` (explicit list) or `glob` (pattern fan-out)
/// read several files in ONE call — the "search, then read what it found"
/// chain collapses into a single tool call. Per-file errors are isolated and
/// the call succeeds when at least one file is readable.
pub(crate) async fn tool_read(args: &Map<String, Value>) -> Result<String, ToolError> {
    if let Some(paths) = args.get("paths").and_then(Value::as_array) {
        return fanout_read(parse_path_list(paths)?, args).await;
    }
    if let Some(glob) = args.get("glob").and_then(Value::as_str) {
        return fanout_read(expand_glob(glob).await?, args).await;
    }
    let path = workspace_path(&arg_str(args, "path")?)?;
    let (body, _more) =
        read_file_numbered(&path, read_offset(args), read_limit(args, READ_MAX_LINES)).await?;
    Ok(body)
}

fn read_offset(args: &Map<String, Value>) -> usize {
    args.get("offset")
        .and_then(Value::as_u64)
        .map(|n| n.max(1) as usize)
        .unwrap_or(1)
}

fn read_limit(args: &Map<String, Value>, default: usize) -> usize {
    args.get("limit")
        .and_then(Value::as_u64)
        .map(|n| n.max(1) as usize)
        .unwrap_or(default)
}

fn parse_path_list(paths: &[Value]) -> Result<Vec<PathBuf>, ToolError> {
    if paths.is_empty() {
        return Err(ToolError::InvalidArgument(
            "paths must not be empty".to_string(),
        ));
    }
    if paths.len() > READ_FANOUT_MAX_FILES {
        return Err(ToolError::InvalidArgument(format!(
            "paths accepts at most {READ_FANOUT_MAX_FILES} files per call (got {}); split into batches",
            paths.len()
        )));
    }
    paths
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(|s| workspace_path(s).map_err(ToolError::from))
                .unwrap_or_else(|| {
                    Err(ToolError::InvalidArgument(
                        "paths entries must be strings".to_string(),
                    ))
                })
        })
        .collect()
}

/// Line-numbered content for one file, within the read budgets. Also returns
/// how many lines were omitted past the end (for pagination notes).
async fn read_file_numbered(
    path: &Path,
    offset: usize,
    limit: usize,
) -> Result<(String, usize), ToolError> {
    // Budgeted read: `take(MAX+1)` caps the read at the byte budget + one
    // probe byte, so a multi-GB log never lands in memory whole. The extra
    // byte distinguishes over-budget from exact-fit.
    let file = tokio::fs::File::open(path).await.map_err(ToolError::Io)?;
    let mut bytes = Vec::new();
    file.take(u64::try_from(READ_MAX_BYTES + 1).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)
        .await
        .map_err(ToolError::Io)?;
    let over_budget = bytes.len() > READ_MAX_BYTES;
    bytes.truncate(READ_MAX_BYTES);
    if bytes.contains(&0) {
        return Err(ToolError::InvalidArgument(format!(
            "binary file; use `bash` with a targeted command such as `strings` or `hexdump` on {}",
            path.display()
        )));
    }
    let text = String::from_utf8_lossy(&bytes);

    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if offset > total {
        return Err(ToolError::InvalidArgument(format!(
            "offset {offset} is past the end; {} has {total} lines",
            path.display()
        )));
    }
    let numbered: Vec<String> = lines
        .iter()
        .skip(offset - 1)
        .take(limit)
        .enumerate()
        .map(|(index, line)| format!("{:>4}  {}", offset + index, line))
        .collect();
    let mut out = clamp_lines(&numbered.join("\n"), READ_MAX_LINES, READ_MAX_BYTES);
    let shown = numbered.len();
    let more = total.saturating_sub(offset - 1 + shown);
    if more > 0 && limit >= READ_MAX_LINES {
        out.push_str(&format!(
            "\n[... {more} more lines; continue with offset {} ...]",
            offset + shown
        ));
    }
    if over_budget {
        out.push_str("\n[... file exceeds the read byte budget; use offset/limit ...]");
    }
    Ok((out, more))
}

/// Multi-file read: `==> path <==` sections (grep-style), per-file limits,
/// isolated per-file errors, one shared byte budget.
#[allow(clippy::type_complexity)]
async fn fanout_read(paths: Vec<PathBuf>, args: &Map<String, Value>) -> Result<String, ToolError> {
    // Concurrent reads (S1 latency win: 10x5ms serial ~50ms -> ~5-10ms).
    // JoinSet + semaphore: at most 10 files read concurrently no matter
    // how the input caps evolve; join + sort restores input order, budget
    // is enforced on join.
    let per_file = read_limit(args, READ_FANOUT_PER_FILE_LINES);
    let offset = read_offset(args);
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(10));
    let mut set = tokio::task::JoinSet::new();
    for (idx, path) in paths.iter().enumerate() {
        let path = path.clone();
        let sem = sem.clone();
        set.spawn(async move {
            let _permit = sem.acquire_owned().await;
            let res = read_file_numbered(&path, offset, per_file).await;
            (idx, path.display().to_string(), res)
        });
    }
    let mut joined: Vec<(usize, String, Result<(String, usize), ToolError>)> = Vec::new();
    while let Some(r) = set.join_next().await {
        if let Ok(v) = r {
            joined.push(v);
        }
    }
    joined.sort_by_key(|(idx, _, _)| *idx);
    let mut sections: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut used = 0usize;
    for (_, display, res) in joined {
        match res {
            Ok((body, _)) => {
                used += body.len();
                sections.push(format!("==> {display} <==\n{body}"));
            }
            Err(error) => errors.push(format!("==> {display}: error: {error}")),
        }
        if used > READ_MAX_BYTES {
            sections.push("[... read budget reached; remaining files skipped ...]".to_string());
            break;
        }
    }
    if sections.is_empty() {
        return Err(ToolError::InvalidArgument(format!(
            "no file could be read: {}",
            errors.join("; ")
        )));
    }
    let mut out = clamp_lines(&sections.join("\n"), READ_MAX_LINES, READ_MAX_BYTES);
    if !errors.is_empty() {
        out.push('\n');
        out.push_str(&errors.join("\n"));
    }
    Ok(out)
}

/// Expand a glob into workspace paths. Patterns with `/` match full paths
/// (`src/tools/*.rs`); bare patterns match basenames anywhere (`*.rs`).
async fn expand_glob(glob: &str) -> Result<Vec<PathBuf>, ToolError> {
    let glob = glob.trim().trim_start_matches("./").to_string();
    if glob.is_empty() || !glob.contains(['*', '?']) {
        return Err(ToolError::InvalidArgument(
            "glob must contain wildcard characters (* or ?); use `path` for a single file"
                .to_string(),
        ));
    }
    expand_glob_in(&workspace_root()?, &glob).await
}

/// `expand_glob` rooted at `root` (the live one uses the workspace root):
/// hermetic in tests, no `current_dir` redirect needed.
pub(crate) async fn expand_glob_in(root: &Path, glob: &str) -> Result<Vec<PathBuf>, ToolError> {
    // No shell: the old path interpolated the pattern into a single-quoted
    // `find` command through `run_bash`, so a `'` or space in the pattern
    // broke the command — and every call paid the sh spawn + drain tasks +
    // 120 s-timeout machinery for a sub-second directory walk. `find` itself
    // stays on Unix (exact `-prune`/`-path` semantics; serving from the fff index
    // would miss files never read, and a hand-rolled matcher would drift
    // from `find`); only the wrapping changes. Windows has no usable `find`
    // (System32 `find.exe` takes different flags), so it uses the native
    // walk below directly.
    #[cfg(windows)]
    {
        return expand_glob_native(root, glob).await;
    }
    #[cfg(not(windows))]
    {
        return expand_glob_via_find(root, glob).await;
    }
}

/// Native recursive walk with the same prune + files-only semantics as the
/// `find` path (Windows fallback; also used when `find` is missing).
async fn expand_glob_native(root: &Path, glob: &str) -> Result<Vec<PathBuf>, ToolError> {
    let root_canon = root.canonicalize().map_err(ToolError::Io)?;
    let mut rels: Vec<String> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root_canon.clone()];
    // Bound the walk like the `find` path bounds its resolve window.
    const WALK_CAP: usize = 4096;
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() && !ft.is_symlink() {
                if matches!(name.as_str(), ".git" | "target" | "node_modules") {
                    continue;
                }
                if rels.len() + stack.len() < WALK_CAP {
                    stack.push(path);
                }
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let Ok(rel) = path.strip_prefix(&root_canon).map(|p| p.to_path_buf()) else {
                continue;
            };
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let hit = if glob.contains('/') {
                glob_match_path(glob, &rel_str)
            } else {
                glob_match_path(glob, &name)
            };
            if hit {
                rels.push(format!("./{rel_str}"));
            }
            if rels.len() >= READ_FANOUT_GLOB_MAX_FILES * 8 {
                break;
            }
        }
        if rels.len() >= READ_FANOUT_GLOB_MAX_FILES * 8 {
            break;
        }
    }
    finish_glob_matches(&root_canon, rels, glob)
}

/// `*` spans `/` (like `find -path`), `?` matches one char. Minimal — only
/// the two wildcards `expand_glob` admits.
fn glob_match_path(pattern: &str, text: &str) -> bool {
    let (mut px, mut tx) = (0usize, 0usize);
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut star, mut match_tx) = (None::<usize>, 0usize);
    while tx < t.len() {
        if px < p.len() && (p[px] == b'?' || p[px] == t[tx]) {
            px += 1;
            tx += 1;
        } else if px < p.len() && p[px] == b'*' {
            star = Some(px);
            match_tx = tx;
            px += 1;
        } else if let Some(s) = star {
            px = s + 1;
            match_tx += 1;
            tx = match_tx;
        } else {
            return false;
        }
    }
    while px < p.len() && p[px] == b'*' {
        px += 1;
    }
    px == p.len()
}

#[cfg(not(windows))]
async fn expand_glob_via_find(root: &Path, glob: &str) -> Result<Vec<PathBuf>, ToolError> {
    let mut cmd = tokio::process::Command::new("find");
    cmd.arg(".")
        .arg("(")
        .arg("-name")
        .arg(".git")
        .arg("-o")
        .arg("-name")
        .arg("target")
        .arg("-o")
        .arg("-name")
        .arg("node_modules")
        .arg(")")
        .arg("-prune")
        .arg("-o");
    if glob.contains('/') {
        cmd.arg("-path").arg(format!("./{glob}"));
    } else {
        cmd.arg("-name").arg(glob);
    }
    // Files only at the source: without this a bare `*` returns directories
    // (and `.` itself) that then eat the pre-filter truncate window and
    // starve real files. `-print0`: newline-containing filenames would split
    // on `lines()` below into phantom paths; NUL-separated output keeps them
    // intact. Note `find` interprets `[ ]` as character classes while the
    // native fallback (`glob_match_path`) matches them literally, and both
    // skip symlinked dirs but follow symlinked files the same way — the
    // native path is only a no-`find` fallback, so the drift is documented,
    // not papered over.
    cmd.arg("-type")
        .arg("f")
        .arg("-print0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .current_dir(root);
    let child = match cmd.spawn() {
        Ok(c) => c,
        // No `find` on PATH (minimal containers): same semantics, no error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return expand_glob_native(root, glob).await;
        }
        Err(e) => return Err(ToolError::Io(e)),
    };
    // A pruned walk is bounded, but a wedge (stalled FS) or Ctrl+C must not
    // park the turn: race the wait against cancellation, like `run_bash`,
    // with a wall-clock cap so a wedged FS can't park a turn past cancel.
    let output = tokio::select! {
        out = child.wait_with_output() => out.map_err(ToolError::Io)?,
        _ = wait_cancelled(&crate::runtime::cancel::GlobalCancellation) => {
            return Err(ToolError::Shell {
                output: "Error: shell command cancelled".to_string(),
                code: None,
            });
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
            return Err(ToolError::Shell {
                output: "Error: glob walk timed out".to_string(),
                code: None,
            });
        }
    };
    if !matches!(output.status.code(), Some(0) | Some(1)) {
        return Err(ToolError::Shell {
            output: String::from_utf8_lossy(&output.stdout).into_owned(),
            code: output.status.code(),
        });
    }
    // Hoisted root canonicalization (was re-canonicalized per match) and
    // truncate-before-resolve: at most 8 candidates pay the symlink check.
    // The stdout is capped before splitting (a huge monorepo's `find`
    // output would otherwise sit whole in memory): the trailing partial
    // token of a cut is dropped so a truncated name can never resolve to a
    // wrong file.
    let root = root.canonicalize().map_err(ToolError::Io)?;
    let mut stdout = output.stdout;
    let cut = stdout.len() > FIND_GLOB_STDOUT_CAP;
    if cut {
        stdout.truncate(FIND_GLOB_STDOUT_CAP);
    }
    let mut rels: Vec<String> = stdout
        .split(|&b| b == 0)
        .filter_map(|chunk| {
            let s = std::str::from_utf8(chunk).ok()?.trim();
            (!s.is_empty()).then(|| s.to_owned())
        })
        .collect();
    if cut {
        rels.pop();
    }
    rels.sort_unstable();
    rels.dedup();
    // `find -type f` already excludes directories, so truncating here can't
    // starve files behind dirs (was: truncate-then-`is_file`).
    rels.truncate(READ_FANOUT_GLOB_MAX_FILES * 8);
    finish_glob_matches(&root, rels, glob)
}

/// Shared tail: workspace-confine + symlink check, files-only, cap, error
/// when nothing survives.
fn finish_glob_matches(
    root_canon: &Path,
    rels: Vec<String>,
    glob: &str,
) -> Result<Vec<PathBuf>, ToolError> {
    let mut paths = Vec::new();
    for rel in &rels {
        // Directories (and the `.` root itself for a bare `*`) used to ride
        // along and die as per-file read errors in `fanout_read`, eating cap
        // slots real files could have used — keep files only.
        if let Ok(path) = resolve_workspace_path(root_canon, rel) {
            if path.is_file() {
                paths.push(path);
            }
        }
    }
    paths.sort_unstable();
    paths.dedup();
    paths.truncate(READ_FANOUT_GLOB_MAX_FILES);
    if paths.is_empty() {
        return Err(ToolError::InvalidArgument(format!(
            "glob '{glob}' matched no files"
        )));
    }
    Ok(paths)
}
