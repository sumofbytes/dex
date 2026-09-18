//! fff-powered search: `grep` (content) and `find` (paths) — aliases `ffgrep`/`fffind` kept for compat, backed by the
//! fff-search crate (also the engine behind fff.nvim).
//! One shared picker per process; fff spawns its own background scan and
//! filesystem watcher, so results stay fresh without rescans.

use std::env;
use std::sync::OnceLock;
use std::time::Duration;

use ::fff::grep::{has_regex_metacharacters, GrepMode, GrepSearchOptions};
use ::fff::{
    FFFMode, FilePicker, FilePickerOptions, FuzzySearchOptions, PaginationArgs, QueryParser,
    SharedFilePicker, SharedFrecency,
};

use super::{arg_str, ToolError};
use serde_json::{Map, Value};

const SCAN_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounds a single grep so a pathological query cannot stall the turn.
const GREP_TIME_BUDGET_MS: u64 = 10_000;

static FFF: OnceLock<Result<(SharedFilePicker, SharedFrecency), String>> = OnceLock::new();

fn fff() -> Result<&'static (SharedFilePicker, SharedFrecency), ToolError> {
    FFF.get_or_init(|| {
        let shared = SharedFilePicker::default();
        let frecency = SharedFrecency::default();
        let base = env::current_dir().map_err(|e| e.to_string())?;
        FilePicker::new_with_shared_state(
            shared.clone(),
            frecency.clone(),
            FilePickerOptions {
                base_path: base.to_string_lossy().into_owned(),
                enable_mmap_cache: true,
                enable_content_indexing: true,
                watch: true,
                mode: FFFMode::Ai,
                ..FilePickerOptions::default()
            },
        )
        .map_err(|e| e.to_string())?;
        if !shared.wait_for_scan(SCAN_TIMEOUT) {
            return Err("fff initial scan did not finish in 30s".to_string());
        }
        Ok((shared, frecency))
    })
    .as_ref()
    .map_err(|e| ToolError::Internal(e.clone()))
}

/// Deterministic index refresh for tests that create fixture files after the
/// initial scan. Serialized: a rescan requested while another scan is active
/// is deferred by fff, so a second trigger after wait_for_scan guarantees one
/// admitted rescan ran after the caller's fixture writes. Explicit rescans
/// bypass fff's throttle.
#[cfg(test)]
pub(crate) fn rescan() {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock();
    if let Ok((picker, frecency)) = fff() {
        for _ in 0..2 {
            let _ = picker.trigger_full_rescan_async(frecency);
            // wait_for_indexing_complete (not wait_for_scan): the walk finishing
            // is not enough — post-scan index builds must be done too, or a
            // concurrent search can hit a half-built index.
            let _ = picker.wait_for_indexing_complete(SCAN_TIMEOUT);
        }
    }
}

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

fn with_picker<T>(
    f: impl FnOnce(&::fff::FilePicker) -> Result<T, ToolError>,
) -> Result<T, ToolError> {
    let (picker, _) = fff()?;
    let guard = picker
        .read()
        .map_err(|e| ToolError::Internal(format!("fff lock: {e}")))?;
    let p = guard
        .as_ref()
        .ok_or_else(|| ToolError::Internal("fff picker missing".to_string()))?;
    f(p)
}

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
    let matches_per_file = if files_mode { 1 } else { 10 };

    with_picker(|p| {
        let parser = QueryParser::new(::fff::AiGrepConfig);
        let parsed = parser.parse(&query);
        let mode = if has_regex_metacharacters(&parsed.grep_text()) {
            GrepMode::Regex
        } else {
            GrepMode::PlainText
        };
        let options = GrepSearchOptions {
            max_matches_per_file: matches_per_file,
            file_offset,
            page_limit: head_limit,
            mode,
            time_budget_ms: GREP_TIME_BUDGET_MS,
            before_context: context,
            after_context: context,
            classify_definitions: true,
            trim_whitespace: true,
            ..Default::default()
        };
        let result = p.grep(&parsed, &options);
        if result.matches.is_empty() {
            // A paged continuation that finds nothing is an honest empty
            // page (the remaining files don't match); fuzzy retry would
            // resurface files from before the offset.
            if file_offset > 0 {
                return Ok("0 matches.".to_string());
            }
            // Typo tolerance: retry the query as fuzzy before giving up.
            let fuzzy_query: String = parsed
                .grep_text()
                .chars()
                .filter(|c| !matches!(c, ':' | '-' | '_'))
                .flat_map(char::to_lowercase)
                .collect();
            let fuzzy = parser.parse(&fuzzy_query);
            let fuzzy_result = p.grep(
                &fuzzy,
                &GrepSearchOptions {
                    mode: GrepMode::Fuzzy,
                    max_matches_per_file: 3,
                    page_limit: head_limit,
                    time_budget_ms: GREP_TIME_BUDGET_MS,
                    trim_whitespace: true,
                    ..Default::default()
                },
            );
            if fuzzy_result.matches.is_empty() {
                return Ok("0 matches.".to_string());
            }
            let mut out = format!(
                "0 exact matches for '{query}'. {} approximate:",
                fuzzy_result.matches.len()
            );
            let mut last_file = String::new();
            for m in fuzzy_result.matches.iter().take(5) {
                let file = fuzzy_result.files[m.file_index].relative_path(p);
                if file != last_file {
                    out.push_str(&format!("\n{file}"));
                    last_file = file;
                }
                // Files mode lists paths only: detail rows would pollute
                // the path list (and the files-matched summary). Each
                // detail gets its own line so content rows stay `N: code`
                // shaped for the summary counter and search preview.
                if !files_mode {
                    out.push_str(&format!("\n  {}: {}", m.line_number, m.line_content));
                }
            }
            return Ok(out);
        }

        let mut out = String::new();
        if files_mode {
            let mut last_file = String::new();
            for m in &result.matches {
                let file = result.files[m.file_index].relative_path(p);
                if file != last_file {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&file);
                    last_file = file;
                }
            }
        } else {
            let mut last_file = String::new();
            for m in &result.matches {
                let file = result.files[m.file_index].relative_path(p);
                if file != last_file {
                    if !out.is_empty() {
                        out.push_str("\n\n");
                    }
                    last_file = file.clone();
                }
                let start = m.line_number - m.context_before.len() as u64;
                for (i, line) in m.context_before.iter().enumerate() {
                    out.push_str(&format!("\n{file}:{}-{line}", start + i as u64));
                }
                out.push_str(&format!("\n{file}:{}:{}", m.line_number, m.line_content));
                for (i, line) in m.context_after.iter().enumerate() {
                    out.push_str(&format!("\n{file}:{}-{line}", m.line_number + 1 + i as u64));
                }
            }
        }
        // Truncation summary, read's trailer standard: say what was shown,
        // that more was left unscanned, and how to continue. Without
        // pagination the model could only widen head_limit blindly.
        if result.next_file_offset != 0 {
            let (kind, shown) = if files_mode {
                ("files", result.files.len())
            } else {
                ("matches", result.matches.len())
            };
            out.push_str(&format!(
                "\n[... {shown} {kind} shown, more files unscanned; continue with file_offset {} or raise head_limit ...]",
                result.next_file_offset
            ));
        } else if !files_mode
            && !result.files.is_empty()
            && result.matches.len() == result.files.len() * matches_per_file
        {
            // No pagination but every file sat at the per-file cap: matches
            // were likely dropped inside the files already shown. Say so
            // instead of truncating silently.
            out.push_str(&format!(
                "\n[... every file hit the {matches_per_file}-match cap; matches may be missing — narrow the query ...]"
            ));
        }
        Ok(out)
    })
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

    with_picker(|p| {
        let parser = QueryParser::default();
        let parsed = parser.parse(&query);
        let options = FuzzySearchOptions {
            max_threads: 0,
            current_file: None,
            project_path: Some(p.base_path()),
            combo_boost_score_multiplier: 100,
            min_combo_count: 3,
            pagination: PaginationArgs {
                offset: 0,
                limit: limit + 1,
            },
        };
        let result = p.fuzzy_search(&parsed, None, options);
        if result.items.is_empty() {
            return Ok("0 matches.".to_string());
        }
        let mut out = result
            .items
            .iter()
            .take(limit)
            .map(|item| item.relative_path(p))
            .collect::<Vec<_>>()
            .join("\n");
        if result.items.len() > limit {
            out.push_str(&format!(
                "\n[... {} of {} paths matched; raise limit or narrow the query ...]",
                result.total_matched.saturating_sub(limit),
                result.total_matched
            ));
        }
        Ok(out)
    })
}
