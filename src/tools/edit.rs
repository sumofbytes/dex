//! Transactional file edits with fuzzy anchoring.

use serde_json::{Map, Value};
use similar::TextDiff;

use super::args::arg_str;
use super::write::{atomic_write, check_expected_hash_bytes};
use super::{workspace_path, ToolError};

pub(crate) async fn tool_edit(args: &Map<String, Value>) -> Result<String, ToolError> {
    let path = workspace_path(&arg_str(args, "path")?)?;
    let ops = parse_edit_ops(args)?;
    let replace_all = args
        .get("replaceAll")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(ToolError::Io)?;
    check_expected_hash_bytes(args, &path, &content)?;
    let (updated, note) = apply_edit_batch(&content, &ops, replace_all)?;
    atomic_write(&path, &updated).await?;
    Ok(format!("edited {}{note}", path.display()))
}

/// One `oldText` → `newText` replacement; a batch holds several.
type EditOp = (String, String);

/// Accept either the single `oldText`/`newText` pair or a batch `edits[]`
/// array of `{oldText, newText}` objects (pi's shape): every entry is matched
/// against the original file, so disjoint replacements land in one call
/// instead of one round-trip each. The two shapes do not mix.
pub(crate) fn parse_edit_ops(args: &Map<String, Value>) -> Result<Vec<EditOp>, ToolError> {
    // Treat explicit `null` as absent: several clients (and some providers'
    // tool-call argument serializers) emit `{"oldText": null, "newText": null}`
    // for omitted optional fields, which used to trip the both-shapes guard.
    let present = |key: &str| args.get(key).is_some_and(|value| !value.is_null());
    if let Some(edits) = args.get("edits") {
        if !edits.is_null() && (present("oldText") || present("newText")) {
            return Err(ToolError::InvalidArgument(
                "pass either oldText/newText or edits[], not both".to_string(),
            ));
        }
        let entries = edits.as_array().ok_or_else(|| {
            ToolError::InvalidArgument("edits must be an array of {oldText, newText}".to_string())
        })?;
        if entries.is_empty() {
            return Err(ToolError::InvalidArgument(
                "edits must contain at least one {oldText, newText} entry".to_string(),
            ));
        }
        return entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                if !entry.is_object() {
                    return Err(ToolError::InvalidArgument(format!(
                        "edits[{index}] must be an object with oldText and newText"
                    )));
                }
                let old = entry
                    .get("oldText")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ToolError::InvalidArgument(format!("edits[{index}] is missing oldText"))
                    })?;
                let new = entry
                    .get("newText")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ToolError::InvalidArgument(format!("edits[{index}] is missing newText"))
                    })?;
                check_edit_texts(old, new).map_err(|error| {
                    ToolError::InvalidArgument(format!("edits[{index}]: {error}"))
                })?;
                Ok((old.to_string(), new.to_string()))
            })
            .collect();
    }
    let old = arg_str(args, "oldText")?;
    let new = arg_str(args, "newText")?;
    check_edit_texts(&old, &new)?;
    Ok(vec![(old, new)])
}

fn check_edit_texts(old: &str, new: &str) -> Result<(), ToolError> {
    if old.is_empty() {
        return Err(ToolError::InvalidArgument(
            "oldText must not be empty; use `write` to create files".to_string(),
        ));
    }
    if old == new {
        return Err(ToolError::InvalidArgument(
            "oldText and newText are identical; nothing to edit".to_string(),
        ));
    }
    Ok(())
}

/// Git-style unified diff (4 context lines, `--- a/…` / `+++ b/…` headers,
/// `/dev/null` for new files) of a pending write/edit, shown in the
/// transcript before approval and under the tool result. Returns None when
/// the file is missing or the change is empty.
fn build_diff(raw_path: &str, before: Option<&str>, after: &str) -> Option<String> {
    let diff = TextDiff::from_lines(before.unwrap_or(""), after);
    let (old_header, new_header) = match &before {
        Some(_) => (format!("a/{raw_path}"), format!("b/{raw_path}")),
        None => ("/dev/null".to_string(), format!("b/{raw_path}")),
    };
    let out = diff
        .unified_diff()
        .context_radius(4)
        .header(&old_header, &new_header)
        .to_string();
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn diff_after(name: &str, args: &Map<String, Value>, before: Option<&str>) -> Option<String> {
    match name {
        "write" => arg_str(args, "content").ok(),
        "edit" => {
            let ops = parse_edit_ops(args).ok()?;
            let replace_all = args
                .get("replaceAll")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            apply_edit_batch(before.unwrap_or(""), &ops, replace_all)
                .ok()
                .map(|(updated, _)| updated)
        }
        _ => None,
    }
}

pub(crate) async fn change_diff_async(name: &str, args: &Map<String, Value>) -> Option<String> {
    let raw_path = arg_str(args, "path").ok()?;
    let path = workspace_path(&raw_path).ok()?;
    let before = tokio::fs::read_to_string(&path).await.ok();
    let after = diff_after(name, args, before.as_deref())?;
    build_diff(&raw_path, before.as_deref(), &after)
}

#[cfg(test)]
pub(crate) fn apply_edit(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<(String, String), ToolError> {
    apply_edit_batch(content, &[(old.to_string(), new.to_string())], replace_all)
}

/// Fuzzy line comparison for the edit fallback: trim both ends, then fold
/// common lookalike characters to ASCII (smart quotes/dashes/odd spaces,
/// same sets as pi/codex). Every mapping is single-char → single-char, so
/// line alignment is preserved without touching the file's bytes. No new
/// dependencies — stdlib only.
fn fuzzy_line(line: &str) -> String {
    line.trim()
        .chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{00A0}' | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// One pending change as a byte span in the original content plus its
/// replacement text. Both exact and fuzzy matches lower to this shape, so a
/// batch of disjoint edits applies in one reverse-order pass and earlier
/// replacements never shift later spans.
struct PendingReplacement {
    start: usize,
    end: usize,
    text: String,
    /// Human span label (`line 4`, `lines 3-5`) for the result note.
    span: String,
    fuzzy: bool,
}

/// Apply one or more replacements to `content`. Every op is matched against
/// the same original text (never incrementally); overlapping spans across or
/// within ops are rejected so the model merges them into one entry instead of
/// silently double-applying.
pub(crate) fn apply_edit_batch(
    content: &str,
    ops: &[EditOp],
    replace_all: bool,
) -> Result<(String, String), ToolError> {
    let mut pending: Vec<PendingReplacement> = Vec::new();
    for (old, new) in ops {
        pending.extend(find_replacements(content, old, new, replace_all)?);
    }
    pending.sort_by_key(|replacement| replacement.start);
    for pair in pending.windows(2) {
        if pair[0].end > pair[1].start {
            return Err(ToolError::InvalidArgument(
                "edits overlap in the file; merge them into one edit covering the whole region"
                    .to_string(),
            ));
        }
    }
    let mut updated = content.to_string();
    for replacement in pending.iter().rev() {
        updated.replace_range(replacement.start..replacement.end, &replacement.text);
    }
    let single = ops.len() == 1;
    let note = if single && pending.len() == 1 {
        let replacement = &pending[0];
        let suffix = if replacement.fuzzy {
            ", whitespace-insensitive"
        } else {
            ""
        };
        format!(" ({}{suffix})", replacement.span)
    } else if single && !pending[0].fuzzy {
        format!(" ({}, {} occurrences)", pending[0].span, pending.len())
    } else if single {
        format!(" ({} sites, whitespace-insensitive)", pending.len())
    } else {
        let spans = pending
            .iter()
            .map(|replacement| replacement.span.clone())
            .collect::<Vec<_>>()
            .join("; ");
        let suffix = if pending.len() == ops.len() {
            String::new()
        } else {
            format!(", {} sites", pending.len())
        };
        format!(" ({} edits{suffix}: {spans})", ops.len())
    };
    Ok((updated, note))
}

/// Byte offset of every line start in `content` (`offsets[k]` starts line
/// `k`); `lines()` and these offsets agree because both split on `\n`.
fn line_byte_offsets(content: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (index, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(index + 1);
        }
    }
    offsets
}

fn span_label(start_line: usize, end_line: usize) -> String {
    if start_line == end_line {
        format!("line {start_line}")
    } else {
        format!("lines {start_line}-{end_line}")
    }
}

fn find_replacements(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<Vec<PendingReplacement>, ToolError> {
    let count = content.matches(old).count();
    if count == 1 || (replace_all && count > 1) {
        let spans: Vec<(usize, usize)> = if replace_all {
            content
                .match_indices(old)
                .map(|(i, _)| (i, i + old.len()))
                .collect()
        } else {
            let start = content.find(old).unwrap_or(0);
            vec![(start, start + old.len())]
        };
        let first = spans[0].0;
        let start_line = content[..first].matches('\n').count() + 1;
        let end_line = start_line + old.lines().count().saturating_sub(1);
        let span = span_label(start_line, end_line);
        return Ok(spans
            .into_iter()
            .map(|(start, end)| PendingReplacement {
                start,
                end,
                text: new.to_string(),
                span: span.clone(),
                fuzzy: false,
            })
            .collect());
    }
    if count > 1 {
        return Err(ToolError::EditNotUnique(count));
    }

    // Exact match failed: retry with a fuzzy line-window comparison. Beyond
    // indentation/trailing whitespace this now also folds smart quotes,
    // dashes, and odd spaces to ASCII (see `fuzzy_line`), and still works
    // when the file uses CRLF — `lines()` strips the carriage return while
    // the byte spans below keep the surrounding bytes intact.
    let old_lines: Vec<&str> = old.lines().collect();
    let window = old_lines.len();
    let content_lines: Vec<&str> = content.lines().collect();
    let old_fuzzy: Vec<String> = old_lines.iter().map(|line| fuzzy_line(line)).collect();
    let content_fuzzy: Vec<String> = content_lines.iter().map(|line| fuzzy_line(line)).collect();
    let matches_at: Vec<usize> = (0..content_fuzzy.len().saturating_sub(window - 1))
        .filter(|&start| {
            content_fuzzy[start..start + window]
                .iter()
                .zip(&old_fuzzy)
                .all(|(c, o)| c == o)
        })
        .collect();
    if matches_at.is_empty() {
        return Err(ToolError::InvalidArgument(
            "oldText not found; read the file to confirm the exact text (matching ignores indentation, trailing whitespace, and quote/dash variants)"
                .to_string(),
        ));
    }
    if matches_at.len() > 1 && !replace_all {
        return Err(ToolError::EditNotUnique(matches_at.len()));
    }

    // Replace windows from the end so earlier indices stay valid. Each
    // replacement line inherits the indentation of the old line it replaces
    // when it carries none of its own — models frequently resend matched
    // text without the file's leading whitespace.
    let offsets = line_byte_offsets(content);
    let new_lines: Vec<&str> = new.lines().collect();
    let replacements: Vec<PendingReplacement> = matches_at
        .iter()
        .map(|&start| {
            let replacement: Vec<String> = new_lines
                .iter()
                .enumerate()
                .map(|(index, line)| {
                    let old_indent = content_lines
                        .get(start + index)
                        .map(|old_line| &old_line[..old_line.len() - old_line.trim_start().len()])
                        .unwrap_or_default();
                    if !old_indent.is_empty() && !line.is_empty() && line.trim_start() == *line {
                        format!("{old_indent}{line}")
                    } else {
                        line.to_string()
                    }
                })
                .collect();
            let byte_start = offsets[start];
            let byte_end = offsets
                .get(start + window)
                .copied()
                .unwrap_or(content.len());
            let mut text = replacement.join("\n");
            // The span covers whole lines including their terminator, so the
            // replacement must restore it: a mid-file window always ended
            // with `\n`, and a final window keeps the file's trailing newline.
            if byte_end < content.len() || content.ends_with('\n') {
                text.push('\n');
            }
            PendingReplacement {
                start: byte_start,
                end: byte_end,
                text,
                span: span_label(start + 1, start + window),
                fuzzy: true,
            }
        })
        .collect();
    Ok(replacements)
}
