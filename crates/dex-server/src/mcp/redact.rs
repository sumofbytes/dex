//! Secret redaction for MCP errors.

// ---------------------------------------------------------------------------
// Error hygiene: servers and proxies echo our headers/args back in failures.
// Redact `key: value` / `key=value` secrets at the manager boundary so they
// never reach turn output, logs, or the model.
// ---------------------------------------------------------------------------

const SECRET_MARKERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "bearer",
    "api-key",
    "apikey",
    "secret",
    "passwd",
    "password",
    "cookie",
    "set-cookie",
];

/// Scrub secret values from an error string, keeping `Key: [redacted]` shape.
pub(crate) fn redact_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        out.push_str(&redact_line(line));
    }
    out
}

/// Byte offset where the value of `marker` starts in `line`, if `marker` is
/// actually followed by `:`/`=` (or is a bare `Bearer <value>`). Offsets are
/// valid in `line`: the scan runs on an ASCII-lowercased copy
/// (`to_ascii_lowercase` preserves byte offsets; `to_lowercase` can expand
/// non-ASCII, e.g. `İ` -> i + U+0307, and desync the indexes — same fix as
/// mcp/oauth.rs).
fn secret_value_start(line: &str, marker: &str) -> Option<usize> {
    let lower = line.to_ascii_lowercase();
    let mut search = 0;
    while let Some(rel) = lower[search..].find(marker) {
        let abs = search + rel;
        search = abs + marker.len();
        let mut rest = abs + marker.len();
        while matches!(line.as_bytes().get(rest), Some(b' ' | b'\t' | b'\r')) {
            rest += 1;
        }
        // `Authorization: Bearer x`, `api_key=abc`, or a bare `Bearer x`.
        let is_bearer = marker == "bearer"
            && line
                .as_bytes()
                .get(rest)
                .is_some_and(|b| !b.is_ascii_whitespace());
        if matches!(line.as_bytes().get(rest), Some(b':') | Some(b'=')) || is_bearer {
            if !is_bearer {
                rest += 1;
                while matches!(line.as_bytes().get(rest), Some(b' ' | b'\t' | b'\r')) {
                    rest += 1;
                }
            }
            return Some(rest);
        }
    }
    None
}

pub(crate) fn redact_line(line: &str) -> String {
    match SECRET_MARKERS
        .iter()
        .filter_map(|marker| secret_value_start(line, marker))
        .min()
    {
        Some(s) => {
            let end = line.find('\n').unwrap_or(line.len());
            format!("{}[redacted]{}", &line[..s], &line[end..])
        }
        None => line.to_string(),
    }
}
