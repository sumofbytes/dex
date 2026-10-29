//! `grep` (content) and `find` (paths) — aliases `ffgrep`/`fffind` kept for
//! compat. Backed by a fresh gitignore-respecting walk (`ignore` crate) on
//! every call: no index or filesystem watcher to build and keep warm, which
//! keeps the search deps (and their compile weight) out and makes order
//! deterministic — files sort by path, so `file_offset` paging resumes
//! exactly. Typo tolerance: zero-match grep queries retry as fuzzy
//! (adjacent transpositions + single deletions per term); `find` and the
//! `@`-completion route (`complete_paths`) use fuzzy path matching ranked
//! by score plus a small in-process frecency counter.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use ::ignore::{WalkBuilder, WalkState};
use regex::Regex;

use super::args::arg_str;
use super::ToolError;
use serde_json::{Map, Value};

/// Bounds a single grep so a pathological query cannot stall the turn;
/// checked between files, so the file being scanned always completes.
const GREP_TIME_BUDGET: Duration = Duration::from_secs(10);
/// Files above this size are skipped: rarely meaningful matches, and
/// slurping multi-hundred-MB logs would stall the turn.
const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
/// Per-file match cap; scans one past the cap so saturated files (matches
/// likely dropped inside files already shown) stay identifiable.
const MAX_MATCHES_PER_FILE: usize = 10;

const HAS_GLOB: fn(&str) -> bool = |t: &str| t.chars().any(|c| matches!(c, '*' | '?' | '['));

fn query_arg(args: &Map<String, Value>) -> Result<String, ToolError> {
    // `pattern` keeps dex's search-tool convention; `query` matches fff's.
    let query = arg_str(args, "pattern").or_else(|_| arg_str(args, "query"))?;
    if query.trim().is_empty() {
        return Err(ToolError::InvalidArgument(
            "search pattern must not be empty (it would match everything)".to_string(),
        ));
    }
    Ok(query)
}

fn has_regex_metacharacters(s: &str) -> bool {
    s.contains(|c| {
        matches!(
            c,
            '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' | '\\'
        )
    })
}

// --------------------------------------------------------------------- walk

struct ScannedPath {
    /// Workspace-relative, `/`-separated.
    rel: String,
    is_dir: bool,
}

/// Files and directories under the workspace root, sorted by path so the
/// order (and therefore `file_offset` paging) is deterministic.
fn scan() -> Result<(PathBuf, Vec<ScannedPath>), ToolError> {
    let root = super::workspace_root().map_err(|e| ToolError::Internal(e.to_string()))?;
    let out = Mutex::new(Vec::new());
    WalkBuilder::new(&root)
        .filter_entry(|e| e.file_name() != ".git")
        .build_parallel()
        .run(|| {
            // Per-thread batch, merged once when the worker finishes, so
            // the shared lock is not taken per entry.
            let mut batch = Flush {
                local: Vec::new(),
                out: &out,
            };
            let root = &root;
            Box::new(move |entry| {
                // One unreadable branch (permissions, a file vanishing
                // mid-walk) is a skip, not a failed search.
                let Ok(entry) = entry else {
                    return WalkState::Continue;
                };
                let Ok(rel) = entry.path().strip_prefix(root) else {
                    return WalkState::Continue;
                };
                if !rel.as_os_str().is_empty() {
                    batch.local.push(ScannedPath {
                        rel: rel.to_string_lossy().replace('\\', "/"),
                        is_dir: entry.file_type().is_some_and(|t| t.is_dir()),
                    });
                }
                WalkState::Continue
            })
        });
    let mut out = out.into_inner().unwrap_or_else(|e| e.into_inner());
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok((root, out))
}

/// Merges a walker thread's batch into the shared result on drop.
struct Flush<'a> {
    local: Vec<ScannedPath>,
    out: &'a Mutex<Vec<ScannedPath>>,
}

impl Drop for Flush<'_> {
    fn drop(&mut self) {
        self.out
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append(&mut self.local);
    }
}

// ---------------------------------------------------------------- frecency

fn frecency() -> &'static Mutex<HashMap<String, u64>> {
    static F: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    F.get_or_init(Default::default)
}

fn frecency_key(root: &str, rel: &str) -> String {
    format!("{root}/{rel}")
}

/// Bump the paths actually returned, so files the tools keep surfacing
/// rank higher in later `find`/`@`-completion queries of this process.
fn bump_frecency(root: &str, rels: impl IntoIterator<Item = String>) {
    let mut map = frecency().lock().unwrap_or_else(|e| e.into_inner());
    for rel in rels {
        let slot = map.entry(frecency_key(root, &rel)).or_insert(0);
        *slot = slot.saturating_add(1).min(4096);
    }
}

/// Frecency bonus, capped so a heavily-used path cannot swamp match score.
fn frecency_of(root: &str, rel: &str) -> i64 {
    frecency()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&frecency_key(root, rel))
        .copied()
        .unwrap_or(0)
        .min(16) as i64
}

// ------------------------------------------------------------ query parsing

/// One content term compiled once and reused across every file.
struct Term {
    raw: String,
    re: Regex,
}

impl Term {
    /// Plaintext terms are literal and case-insensitive; regex mode passes
    /// each term through raw.
    fn compile(raw: &str, regex_mode: bool) -> Result<Term, ToolError> {
        let pattern = if regex_mode {
            raw.to_string()
        } else {
            format!("(?i){}", regex::escape(raw))
        };
        Regex::new(&pattern)
            .map(|re| Term {
                raw: raw.to_string(),
                re,
            })
            .map_err(|e| ToolError::InvalidArgument(format!("invalid pattern '{raw}': {e}")))
    }
    fn is_match(&self, line: &str) -> bool {
        self.re.is_match(line)
    }
}

/// Translate one glob token into an anchored path regex: `**/` walks any
/// number of directories (including none), `*`/`?` stay within a segment.
fn glob_regex(tok: &str) -> Option<Regex> {
    let mut re = String::from("^");
    let mut chars = tok.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' => {
                if chars.next() == Some('*') {
                    if chars.next_if(|&n| n == '/').is_some() {
                        re.push_str("(?:.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            _ => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    Regex::new(&re).ok()
}

/// Does a path-scope/exclude token (`src`, `src/utils`, `skip.txt`) hit this
/// path? Multi-segment tokens match as directory prefixes; single tokens hit
/// when any slash-delimited segment starts with them (so both `src/` folders
/// and files like `tests.rs` or `skip.txt` resolve).
fn dir_token_matches(tok: &str, rel: &str) -> bool {
    if tok.contains('/') {
        rel.starts_with(&format!("{tok}/")) || rel.contains(&format!("/{tok}/"))
    } else {
        rel.split('/').any(|seg| seg.starts_with(tok))
    }
}

struct ParsedGrep {
    /// Path-glob tokens (`src/**/*.rs`); the path must match all of them.
    path_globs: Vec<Regex>,
    /// Path-scope tokens like `src/` or `tests` (contain `/`); all must hit.
    scopes: Vec<String>,
    /// `!`-prefixed exclusions: (is_glob, pattern); none may hit.
    excludes: Vec<(bool, String)>,
    /// Content terms, all ANDed per line.
    terms: Vec<Term>,
}

fn parse_grep(query: &str, root: &Path) -> Result<ParsedGrep, ToolError> {
    let mut path_globs = Vec::new();
    let mut scopes = Vec::new();
    let mut excludes = Vec::new();
    let mut content: Vec<String> = Vec::new();
    for tok in query.split_whitespace() {
        if let Some(excl) = tok.strip_prefix('!') {
            if !excl.is_empty() {
                excludes.push((HAS_GLOB(excl), excl.trim_end_matches('/').to_string()));
                continue;
            }
        }
        if tok.contains('/') || (!HAS_GLOB(tok) && root.join(tok).is_dir()) {
            if HAS_GLOB(tok) {
                // Unparseable globs would over-match everything; drop them.
                if let Some(re) = glob_regex(tok) {
                    path_globs.push(re);
                }
            } else {
                scopes.push(tok.trim_end_matches('/').to_string());
            }
            continue;
        }
        content.push(tok.to_string());
    }
    // One token with regex metacharacters switches ALL content tokens to
    // raw-regex mode, matching the whole-query behavior of the old engine.
    let regex_mode = content.iter().any(|t| has_regex_metacharacters(t));
    let mut terms = Vec::new();
    for tok in &content {
        terms.push(Term::compile(tok, regex_mode)?);
    }
    if terms.is_empty() {
        return Err(ToolError::InvalidArgument(
            "search pattern must contain a text term (not just path scopes)".to_string(),
        ));
    }
    Ok(ParsedGrep {
        path_globs,
        scopes,
        excludes,
        terms,
    })
}

fn path_allowed(p: &ParsedGrep, rel: &str) -> bool {
    for re in &p.path_globs {
        if !re.is_match(rel) {
            return false;
        }
    }
    for scope in &p.scopes {
        if !dir_token_matches(scope, rel) {
            return false;
        }
    }
    for (is_glob, pat) in &p.excludes {
        let hit = if *is_glob {
            glob_regex(pat).is_some_and(|re| re.is_match(rel))
        } else {
            dir_token_matches(pat, rel)
        };
        if hit {
            return false;
        }
    }
    true
}

// --------------------------------------------------------------------- grep

/// One matched line plus surrounding context.
struct LineHit {
    line_no: u64,
    text: String,
    before: Vec<String>,
    after: Vec<String>,
}

/// A file whose content matched: `saturated` marks files that hit the
/// per-file cap (matches were likely dropped).
struct FileHit {
    rel: String,
    hits: Vec<LineHit>,
    saturated: bool,
}

/// Match file content: case-insensitive literal terms (or raw regex in
/// regex mode), all ANDed per line. Binary-sniffed; oversized and
/// unreadable files are zero matches, not failures.
fn grep_file(root: &Path, rel: &str, terms: &[Term], ctx: usize) -> Option<FileHit> {
    // Stat first: a multi-GB log must be skipped before it is read.
    if std::fs::metadata(root.join(rel)).ok()?.len() as usize > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(root.join(rel)).ok()?;
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let mut indexes = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if terms.iter().all(|t| t.is_match(line)) {
            indexes.push(i);
            if indexes.len() > MAX_MATCHES_PER_FILE {
                break;
            }
        }
    }
    let saturated = indexes.len() > MAX_MATCHES_PER_FILE;
    indexes.truncate(MAX_MATCHES_PER_FILE);
    if indexes.is_empty() {
        return None;
    }
    let hits = indexes
        .iter()
        .map(|&i| LineHit {
            line_no: i as u64 + 1,
            text: lines[i].to_string(),
            before: lines[i.saturating_sub(ctx)..i]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            after: lines[(i + 1).min(lines.len())..(i + 1 + ctx).min(lines.len())]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        })
        .collect();
    Some(FileHit {
        rel: rel.to_string(),
        hits,
        saturated,
    })
}

/// Files scanned per parallel batch: large enough to keep every core busy,
/// small enough that a page filled early wastes little work.
const GREP_CHUNK: usize = 256;

/// `grep_file` over a chunk on scoped worker threads. Results keep the
/// input order, so callers consume them exactly as a sequential scan would.
fn grep_chunk(
    root: &Path,
    chunk: &[&ScannedPath],
    terms: &[Term],
    ctx: usize,
) -> Vec<Option<FileHit>> {
    let workers = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(chunk.len());
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<FileHit>>> = chunk.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(sp) = chunk.get(i) else { break };
                *slots[i].lock().unwrap_or_else(|e| e.into_inner()) =
                    grep_file(root, &sp.rel, terms, ctx);
            });
        }
    });
    slots
        .into_iter()
        .map(|m| m.into_inner().unwrap_or_else(|e| e.into_inner()))
        .collect()
}

/// Emit content rows for one file, merging overlapping context windows.
/// `first` decides the blank-line separator ("\n\n") between file blocks.
fn emit_content(out: &mut String, f: &FileHit, first: bool) {
    if !first {
        out.push_str("\n\n");
    }
    let mut last_emitted: Option<u64> = None;
    for hit in &f.hits {
        let start = hit.line_no - hit.before.len() as u64;
        for (i, line) in hit.before.iter().enumerate() {
            let n = start + i as u64;
            if last_emitted.is_none_or(|l| l < n) {
                out.push_str(&format!("\n{}:{}-{}", f.rel, n, line));
            }
        }
        out.push_str(&format!("\n{}:{}:{}", f.rel, hit.line_no, hit.text));
        for (i, line) in hit.after.iter().enumerate() {
            out.push_str(&format!(
                "\n{}:{}-{}",
                f.rel,
                hit.line_no + 1 + i as u64,
                line
            ));
        }
        last_emitted = Some(hit.line_no + hit.after.len() as u64);
    }
}

// ---- fuzzy fallback for zero-match greps (transpositions + deletions) ----

/// Approximate variants of one term: every adjacent transposition and every
/// single-character deletion, so a mistyped query still lands on lines one
/// small edit away. Too-short terms stay exact (variants would be noise).
fn fuzzy_variants(token: &str) -> Option<Regex> {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() < 3 {
        return None;
    }
    let mut variants: Vec<String> = Vec::with_capacity(2 * chars.len());
    for i in 0..chars.len() {
        let mut del = String::with_capacity(chars.len() - 1);
        del.extend(
            chars
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, c)| c),
        );
        variants.push(del);
    }
    for i in 0..chars.len() - 1 {
        let mut swap = chars.clone();
        swap.swap(i, i + 1);
        variants.push(swap.into_iter().collect());
    }
    variants.sort();
    variants.dedup();
    let alternation = variants
        .iter()
        .filter(|v| !v.is_empty())
        .map(|v| regex::escape(v))
        .collect::<Vec<_>>()
        .join("|");
    if alternation.is_empty() {
        return None;
    }
    Regex::new(&format!("(?i)(?:{alternation})")).ok()
}

/// Fuzzy retry after zero exact matches: up to 3 matches per file, files in
/// path order — the shapes the old engine's fallback produced.
fn fuzzy_retry(
    root: &Path,
    files: &[&ScannedPath],
    terms: &[Term],
    head_limit: usize,
) -> Vec<FileHit> {
    let mut fuzzy_terms = Vec::new();
    for t in terms {
        match fuzzy_variants(&t.raw) {
            Some(re) => fuzzy_terms.push(Term {
                raw: t.raw.clone(),
                re,
            }),
            None => return Vec::new(),
        }
    }
    let mut out = Vec::new();
    for chunk in files.chunks(GREP_CHUNK) {
        for mut hit in grep_chunk(root, chunk, &fuzzy_terms, 0)
            .into_iter()
            .flatten()
        {
            hit.hits.truncate(3);
            hit.saturated = false;
            out.push(hit);
            if out.len() >= head_limit {
                return out;
            }
        }
    }
    out
}

// ------------------------------------------------------------- tool: grep

pub(crate) fn tool_ffgrep(args: &Map<String, Value>) -> Result<String, ToolError> {
    let query = query_arg(args)?;
    let files_mode = match args.get("output_mode").and_then(Value::as_str) {
        None | Some("files") => true,
        Some("content") => false,
        Some(other) => {
            return Err(ToolError::InvalidArgument(format!(
                "unknown output_mode '{other}' (files, content)"
            )));
        }
    };
    let head_limit = args
        .get("head_limit")
        .and_then(Value::as_u64)
        .map(|n| n.max(1) as usize)
        .unwrap_or(50);
    let file_offset = args.get("file_offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let context = args
        .get("context")
        .and_then(Value::as_u64)
        .map(|n| n.min(10) as usize)
        .unwrap_or(0);

    let (root, paths) = scan()?;
    let parsed = parse_grep(&query, &root)?;
    let files: Vec<&ScannedPath> = paths
        .iter()
        .filter(|sp| !sp.is_dir && path_allowed(&parsed, &sp.rel))
        .collect();

    // Pages are slices of the *matched* files in path order, so a page
    // always shows whole files and `file_offset` resumes exactly.
    let mut out = String::new();
    let mut matched_seen = 0usize;
    let mut shown_files = 0usize;
    let mut shown_matches = 0usize;
    let mut saturated_shown = 0usize;
    let mut next_offset: Option<usize> = None;
    let mut shown: Vec<String> = Vec::new();
    let deadline = Instant::now() + GREP_TIME_BUDGET;

    'scan: for chunk in files.chunks(GREP_CHUNK) {
        if Instant::now() >= deadline {
            next_offset = Some(matched_seen.max(file_offset));
            break;
        }
        for hit in grep_chunk(&root, chunk, &parsed.terms, context)
            .into_iter()
            .flatten()
        {
            matched_seen += 1;
            if matched_seen <= file_offset {
                continue;
            }
            if shown_files >= head_limit || Instant::now() >= deadline {
                // First unshown matched file = the resume index.
                next_offset = Some(matched_seen - 1);
                break 'scan;
            }
            shown_files += 1;
            saturated_shown += hit.saturated as usize;
            if files_mode {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&hit.rel);
            } else {
                shown_matches += hit.hits.len();
                let first = out.is_empty();
                emit_content(&mut out, &hit, first);
            }
            shown.push(hit.rel);
        }
    }
    bump_frecency(&root.to_string_lossy(), shown);

    if matched_seen > 0 {
        // Truncation summary, read's trailer standard: say what was shown,
        // that more was left unscanned, and how to continue. Without
        // pagination the model could only widen head_limit blindly.
        if let Some(next) = next_offset {
            let (kind, shown_n) = if files_mode {
                ("files", shown_files)
            } else {
                ("matches", shown_matches)
            };
            out.push_str(&format!(
                "\n[... {shown_n} {kind} shown, more files unscanned; continue with file_offset {next} or raise head_limit ...]"
            ));
        } else if !files_mode && shown_files > 0 && saturated_shown == shown_files {
            // No pagination but every file sat at the per-file cap: matches
            // were likely dropped inside the files already shown. Say so
            // instead of truncating silently.
            out.push_str(&format!(
                "\n[... every file hit the {MAX_MATCHES_PER_FILE}-match cap; matches may be missing — narrow the query ...]"
            ));
        }
        return Ok(out);
    }

    // Zero exact matches: a paged continuation that finds nothing is an
    // honest empty page (the remaining files don't match); fuzzy retry
    // would resurface files from before the offset. Otherwise retry as
    // fuzzy before giving up.
    if file_offset > 0 {
        return Ok("0 matches.".to_string());
    }
    let fuzzy = fuzzy_retry(&root, &files, &parsed.terms, head_limit);
    if fuzzy.is_empty() {
        return Ok("0 matches.".to_string());
    }
    let mut out = format!(
        "0 exact matches for '{query}'. {} approximate:",
        fuzzy.len()
    );
    let mut last_file = String::new();
    for f in fuzzy.iter().take(5) {
        if f.rel != last_file {
            out.push_str(&format!("\n{}", f.rel));
            last_file = f.rel.clone();
        }
        // Files mode lists paths only: detail rows would pollute the path
        // list (and the files-matched summary).
        if !files_mode {
            for h in &f.hits {
                out.push_str(&format!("\n  {}: {}", h.line_no, h.text));
            }
        }
    }
    Ok(out)
}

// ------------------------------------------------------------- tool: find

/// Lowercased fuzzy tokens + path globs, all of which must hit a path.
struct PathQuery {
    globs: Vec<Regex>,
    tokens: Vec<String>,
}

fn parse_path_query(query: &str) -> Result<PathQuery, ToolError> {
    let mut q = PathQuery {
        globs: Vec::new(),
        tokens: Vec::new(),
    };
    for tok in query.split_whitespace() {
        if tok.chars().any(|c| matches!(c, '*' | '?' | '[')) {
            match glob_regex(tok) {
                Some(re) => q.globs.push(re),
                None => {
                    return Err(ToolError::InvalidArgument(format!(
                        "invalid glob pattern '{tok}'"
                    )));
                }
            }
            continue;
        }
        q.tokens.push(tok.to_lowercase());
    }
    Ok(q)
}

/// Rank one workspace-relative path: boundary-anchored substrings score
/// best, in-order subsequences less; frecency nudges repeated winners.
fn fuzzy_token_score(rel_lower: &str, tok: &str) -> Option<i64> {
    if tok.is_empty() {
        return None;
    }
    if let Some(p) = rel_lower.find(tok) {
        let boundary = p == 0 || !rel_lower.as_bytes()[p - 1].is_ascii_alphanumeric();
        return Some(if boundary {
            8 + tok.len() as i64
        } else {
            4 + tok.len() as i64
        });
    }
    let tchars: Vec<char> = tok.chars().collect();
    let mut first: usize = 0;
    let mut last = 0usize;
    let mut ti = 0usize;
    for (bi, c) in rel_lower.char_indices() {
        if ti < tchars.len() && c == tchars[ti] {
            if ti == 0 {
                first = bi;
            }
            last = bi;
            ti += 1;
        }
    }
    if ti == tchars.len() {
        // In-order subsequence, denser matches (fewer skipped chars) better.
        let span = (last - first + 1) as i64;
        Some(2 + (4 * tchars.len() as i64 / span))
    } else {
        None
    }
}

fn path_score(root: &str, rel: &str, q: &PathQuery) -> Option<i64> {
    for re in &q.globs {
        if !re.is_match(rel) {
            return None;
        }
    }
    let rel_lower = rel.to_lowercase();
    let mut score = 0i64;
    for tok in &q.tokens {
        score += fuzzy_token_score(&rel_lower, tok)?;
    }
    Some(score + frecency_of(root, rel))
}

pub(crate) fn tool_fffind(args: &Map<String, Value>) -> Result<String, ToolError> {
    let query = query_arg(args)?;
    if query.trim() == "*" {
        return Err(ToolError::InvalidArgument(
            "find pattern must be targeted (alias fffind) (not '*'); use grep-style constraints instead"
                .to_string(),
        ));
    }
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n.max(1) as usize)
        .unwrap_or(20);
    let q = parse_path_query(&query)?;
    let (root, paths) = scan()?;
    let root_str = root.to_string_lossy().into_owned();
    let mut matched: Vec<(i64, &ScannedPath)> = paths
        .iter()
        .filter(|sp| !sp.is_dir)
        .filter_map(|sp| path_score(&root_str, &sp.rel, &q).map(|s| (s, sp)))
        .collect();
    matched.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.rel.cmp(&b.1.rel)));
    if matched.is_empty() {
        return Ok("0 matches.".to_string());
    }
    let total = matched.len();
    let mut out = matched
        .iter()
        .take(limit)
        .map(|(_, sp)| sp.rel.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if total > limit {
        out.push_str(&format!(
            "\n[... {} of {} paths matched; raise limit or narrow the query ...]",
            total - limit,
            total
        ));
    }
    bump_frecency(
        &root_str,
        matched.iter().take(limit).map(|(_, sp)| sp.rel.clone()),
    );
    Ok(out)
}

// --------------------------------------------------------------- @-popup

/// One `@` completion candidate: workspace-relative path, `is_dir` for folders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathMatch {
    pub(crate) path: String,
    pub(crate) is_dir: bool,
}

/// Files and directories ranked by fuzzy score + frecency for the
/// composer's `@` popup. An empty query lists the best-ranked entries.
pub(crate) fn complete_paths(query: &str, limit: usize) -> Result<Vec<PathMatch>, ToolError> {
    let q = parse_path_query(query)?;
    let (root, paths) = scan()?;
    let root_str = root.to_string_lossy().into_owned();
    let mut matched: Vec<(i64, &ScannedPath)> = paths
        .iter()
        .filter_map(|sp| path_score(&root_str, &sp.rel, &q).map(|s| (s, sp)))
        .collect();
    matched.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.rel.cmp(&b.1.rel)));
    Ok(matched
        .into_iter()
        .take(limit)
        .map(|(_, sp)| PathMatch {
            path: sp.rel.clone(),
            is_dir: sp.is_dir,
        })
        .collect())
}

// -------------------------------------------------------------- test hook

/// Kept so tests keep calling it: there is no persistent index anymore —
/// every grep/find walks fresh, so fixture files are visible immediately.
#[cfg(test)]
pub(crate) fn rescan() {}
