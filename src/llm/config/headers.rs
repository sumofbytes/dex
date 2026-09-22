use super::warn_once;
use super::LlmConfig;
use std::collections::BTreeMap;

use std::env;

/// Parse one custom-header value into `name -> value` pairs.
///
/// Accepts a JSON object (`{"X-Foo":"bar"}` — `headers` map /
/// `http_headers` shape) or `Name: Value` / `Name=Value` pairs separated by
/// commas or newlines (the `ANTHROPIC_CUSTOM_HEADERS` shape).
/// Entries without a name, without a separator, or with an empty value are
/// skipped; later entries win on duplicate names (case-insensitive, last
/// casing wins). `authorization` is dropped (the api key owns it) and a
/// `{...}` value that isn't a JSON object falls back to pair parsing instead
/// of silently yielding nothing.
pub(crate) fn parse_headers_str(raw: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return out;
    }
    if trimmed.starts_with('{') {
        if let Ok(serde_json::Value::Object(map)) =
            serde_json::from_str::<serde_json::Value>(trimmed)
        {
            for (key, value) in map {
                let name = key.trim().to_string();
                if name.is_empty() {
                    continue;
                }
                let val = match &value {
                    serde_json::Value::String(s) => s.trim().to_string(),
                    serde_json::Value::Number(_) | serde_json::Value::Bool(_) => value.to_string(),
                    _ => continue,
                };
                if val.is_empty() {
                    continue;
                }
                insert_extra_header(&mut out, &name, &val);
            }
            return out;
        }
        // Not a JSON object (`{bad`, a JSON array, …) — fall through to
        // pair parsing instead of silently dropping everything. Outer braces
        // are stripped so `{X-Foo: bar}` still yields `X-Foo`.
    }
    let mut body = trimmed;
    if body.starts_with('{') {
        body = body
            .strip_prefix('{')
            .unwrap_or(body)
            .strip_suffix('}')
            .unwrap_or(body)
            .trim();
        if body.is_empty() {
            return out;
        }
    }
    // Newline-separated values may themselves contain commas, so only split
    // on commas when the value is a single line.
    let pieces: Vec<&str> = if body.contains('\n') {
        body.split('\n').collect()
    } else {
        body.split(',').collect()
    };
    for piece in pieces {
        let piece = piece.trim().trim_end_matches(',').trim();
        if piece.is_empty() {
            continue;
        }
        let split = piece.split_once(':').or_else(|| piece.split_once('='));
        let Some((name, value)) = split else {
            continue;
        };
        let name = name.trim().to_string();
        let value = value.trim().to_string();
        insert_extra_header(&mut out, &name, &value);
    }
    out
}

/// Scalar YAML value as a header value string (`"abc"`, `42`, `true`).
/// Empty strings yield `None` so blank entries are skipped.
fn yaml_scalar_str(v: &serde_yaml::Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .map(str::to_string)
        .or_else(|| v.as_u64().map(|n| n.to_string()))
        .or_else(|| v.as_i64().map(|n| n.to_string()))
        .or_else(|| v.as_f64().map(|n| n.to_string()))
        .or_else(|| v.as_bool().map(|b| b.to_string()))
        .filter(|s| !s.is_empty())
}

/// Insert one header with case-insensitive "later wins" semantics: a later
/// `x-foo` replaces an earlier `X-Foo` (last casing wins). Empty names/values
/// and `authorization` (the api key owns that) are skipped so a bad entry
/// can never poison the map — send-time filtering remains as defense in depth.
pub(crate) fn insert_extra_header(out: &mut BTreeMap<String, String>, name: &str, value: &str) {
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() || value.is_empty() {
        return;
    }
    if name.eq_ignore_ascii_case("authorization") {
        return;
    }
    if let Some(existing) = out.keys().find(|k| k.eq_ignore_ascii_case(name)).cloned() {
        if existing != name {
            out.remove(&existing);
        }
    }
    out.insert(name.to_string(), value.to_string());
}

/// One merge for the AGENTS.md header precedence (global file `headers:` <
/// provider-scoped file `headers:` < env/CLI extras, per key): later layers
/// overwrite earlier ones, `authorization` never survives (the api key owns
/// it). Shared by the wire merge (`llm::http::merged_headers`) and the
/// extension auth view (`extension_model_auth_for`) so the two can never
/// drift apart by re-implementing the same order.
pub(crate) fn merge_header_layers(
    global: &BTreeMap<String, String>,
    scoped: &BTreeMap<String, String>,
    extra: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = global.clone();
    for (name, value) in scoped {
        insert_extra_header(&mut out, name, value);
    }
    for (name, value) in extra {
        insert_extra_header(&mut out, name, value);
    }
    out
}

/// Gateway routing affinity: the opencode zen/go endpoints reject requests
/// without `x-opencode-session` (`MissingSessionID`). opencode is an
/// ordinary provider now — no builtin enum arm — so the gate is the
/// *destination*: the provider name (`opencode`/`opencode-go`) or an
/// `opencode.ai` base URL (covers a generic alias pointed at the same
/// gateway). Filled from the dex session id. Keys already present (any
/// casing) are left alone — including in the two file layers, which merge
/// BELOW `extra_headers` on the wire — so explicit user headers always win
/// regardless of call order.
pub(crate) fn apply_opencode_session_headers(config: &mut LlmConfig, session_id: &str) {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return;
    }
    let is_opencode = matches!(config.provider.name(), "opencode" | "opencode-go")
        || reqwest::Url::parse(config.base_url.as_str())
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .is_some_and(|h| h.eq_ignore_ascii_case("opencode.ai"));
    if !is_opencode {
        return;
    }
    // A name pinned in ANY layer — file (`global_headers`/`provider_headers`)
    // or env/CLI/per-request (`extra_headers`) — suppresses the auto-fill:
    // the function writes into `extra_headers`, which merges last, so a
    // file-level user header would otherwise lose to it. Resolve both
    // predicates before mutating (they borrow `config` immutably).
    let taken = |name: &str| {
        config
            .global_headers
            .keys()
            .chain(config.provider_headers.keys())
            .chain(config.extra_headers.keys())
            .any(|k| k.eq_ignore_ascii_case(name))
    };
    let session_taken = taken("x-opencode-session");
    let client_taken = taken("x-opencode-client");
    if !session_taken {
        config
            .extra_headers
            .insert("x-opencode-session".to_string(), session_id.to_string());
    }
    if !client_taken {
        config
            .extra_headers
            .insert("x-opencode-client".to_string(), "dex".to_string());
    }
}

fn merge_config_headers_map(out: &mut BTreeMap<String, String>, map: &serde_yaml::Mapping) {
    for (k, v) in map {
        let name = k.as_str().unwrap_or_default();
        if let Some(val) = yaml_scalar_str(v) {
            insert_extra_header(out, name, &val);
        }
    }
}

/// Custom headers from one config file key. Accepts a mapping,
/// a text-header block (`"X-Foo: bar\nX-Baz: qux"`, same syntax as
/// the env vars / `--header`), or a list mixing both. Later entries win.
/// Parse a raw header string ("K: V" pairs or a JSON object) and merge it
/// into `out` under the standard precedence rules. One funnel for every
/// `parse_headers_str` call site.
pub(crate) fn insert_parsed_headers(out: &mut BTreeMap<String, String>, raw: &str) {
    for (k, v) in parse_headers_str(raw) {
        insert_extra_header(out, &k, &v);
    }
}

/// Merge one YAML header value — a mapping or a raw "K: V" text — into
/// `out`. Other scalar shapes are ignored, as before.
fn merge_one(out: &mut BTreeMap<String, String>, value: &serde_yaml::Value) {
    if let Some(map) = value.as_mapping() {
        merge_config_headers_map(out, map);
    } else if let Some(text) = value.as_str() {
        insert_parsed_headers(out, text);
    }
}

pub(crate) fn config_headers_map(
    file: &Option<serde_yaml::Value>,
    key: &str,
) -> BTreeMap<String, String> {
    let Some(value) = file.as_ref().and_then(|f| f.get(key)) else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    if let Some(items) = value.as_sequence() {
        for item in items {
            merge_one(&mut out, item);
        }
    } else {
        merge_one(&mut out, value);
    }
    out
}

/// Custom headers from the config file. `headers:` wins per-key over
/// `http_headers:` when both set the same name. Both keys are
/// deprecated top-level mirrors of provider state — still global, with a
/// one-time pointer at the canonical spots.
pub(crate) fn load_config_headers(file: &Option<serde_yaml::Value>) -> BTreeMap<String, String> {
    let mut out = config_headers_map(file, "http_headers");
    let file_headers = config_headers_map(file, "headers");
    if out.is_empty() && file_headers.is_empty() {
        return out;
    }
    if !out.is_empty() {
        warn_once(
            "config:http_headers",
            "top-level config key 'http_headers:' is deprecated — use 'headers:' under the provider's entry in 'providers:' (provider-scoped) or DEX_HEADERS (global)",
        );
    }
    if !file_headers.is_empty() {
        warn_once(
            "config:headers",
            "top-level config key 'headers:' is deprecated — use 'headers:' under the provider's entry in 'providers:' (provider-scoped) or DEX_HEADERS (global)",
        );
    }
    for (k, v) in file_headers {
        insert_extra_header(&mut out, &k, &v);
    }
    out
}

/// Custom headers from the environment. Later sources win per-key:
/// `ANTHROPIC_CUSTOM_HEADERS` (claude) < `OPENAI_HEADERS` < `DEX_HEADERS`.
/// The first two are deprecated aliases — honored, with a one-time notice
/// pointing at `DEX_HEADERS`.
pub(crate) fn custom_headers_from_env() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for key in ["ANTHROPIC_CUSTOM_HEADERS", "OPENAI_HEADERS", "DEX_HEADERS"] {
        if let Ok(raw) = env::var(key) {
            if key != "DEX_HEADERS" && !raw.trim().is_empty() {
                warn_once(
                    key,
                    &format!("env var {key} is deprecated — use DEX_HEADERS (same syntax)"),
                );
            }
            insert_parsed_headers(&mut out, &raw);
        }
    }
    out
}
