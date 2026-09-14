//! Hook outcome types: the typed reading of an event handler's directive
//! envelope. The orchestration (running `tool.before` across extensions,
//! fail-open vs `strict`) lives on the manager; this module owns the
//! envelope parsing plus its unit tests.

use serde_json::{Map, Value as Json};

/// Result of the `tool.before` chain for one tool call.
pub(crate) enum BeforeOutcome {
    /// No hook objected: the (possibly mutated) args to run.
    Proceed {
        args: Map<String, Json>,
        /// Extensions that mutated the args, in hook order — audited.
        mutated_by: Vec<String>,
    },
    /// A hook denied the call before any gate ran.
    Denied { by: String, reason: String },
}

/// Result of the `tool.after` chain: the (possibly rewritten) result.
pub(crate) struct AfterOutcome {
    pub(crate) text: String,
    pub(crate) ok: bool,
}

/// What `session.before_compact` handlers decided (merged across handlers:
/// any cancel wins, instruction strings concatenate).
pub(crate) struct CompactAction {
    pub(crate) cancel: bool,
    pub(crate) instructions: Vec<String>,
    /// Full replacement summary (P3 custom compaction): when set, the host
    /// uses it verbatim instead of generating one.
    pub(crate) summary: Option<String>,
}

/// Read one extension's `tool.before` envelope: `deny`/`reason` plus the
/// mutated `args` object. Non-object `args` is an error (fail-open skip vs
/// strict deny is the manager's decision).
/// Parsed `tool.before` directive: mutated args plus an optional deny.
type BeforeDirective = (Map<String, Json>, Option<(bool, String)>);

pub(crate) fn parse_before(envelope: &Map<String, Json>) -> Result<BeforeDirective, String> {
    let args = match envelope.get("args") {
        Some(Json::Object(map)) => map.clone(),
        _ => return Err("tool.before handler must leave args as an object".to_string()),
    };
    let deny = match envelope.get("deny") {
        None | Some(Json::Null) => None,
        Some(Json::Bool(true)) => {
            let reason = envelope
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("denied by extension hook");
            Some((true, reason.to_string()))
        }
        Some(Json::Bool(false)) => None,
        Some(other) => {
            return Err(format!("tool.before deny must be a boolean, got {other}"));
        }
    };
    Ok((args, deny))
}

/// Read one extension's `tool.after` envelope: optional `content` rewrite
/// and `is_error` flip. Absent keys keep the host result.
pub(crate) fn parse_after(envelope: &Map<String, Json>, text: &str, ok: bool) -> (String, bool) {
    let content = envelope
        .get("content")
        .and_then(|c| c.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| text.to_string());
    // is_error=true means the result IS an error, so ok=false.
    let ok = envelope
        .get("is_error")
        .and_then(|v| v.as_bool())
        .map(|is_error| !is_error)
        .unwrap_or(ok);
    (content, ok)
}

/// Read one extension's `session.before_compact` envelope.
pub(crate) fn parse_compact(envelope: &Map<String, Json>) -> CompactAction {
    CompactAction {
        cancel: envelope
            .get("cancel")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        instructions: envelope
            .get("instructions")
            .and_then(|v| v.as_str())
            .map(|s| vec![s.to_string()])
            .unwrap_or_default(),
        summary: envelope
            .get("summary")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn before_parses_deny_and_args() {
        let env = json!({"args": {"a": 1}, "deny": true, "reason": "nope"});
        let (args, deny) = parse_before(env.as_object().unwrap()).unwrap();
        assert_eq!(args["a"], json!(1));
        assert_eq!(deny, Some((true, "nope".to_string())));
        let env = json!({"args": {}});
        let (_, deny) = parse_before(env.as_object().unwrap()).unwrap();
        assert_eq!(deny, None);
        let env = json!({"args": [1]});
        assert!(parse_before(env.as_object().unwrap()).is_err());
    }

    #[test]
    fn after_rewrites_content_and_flips_ok() {
        let env = json!({"content": "new", "is_error": true});
        assert_eq!(
            parse_after(env.as_object().unwrap(), "old", true),
            ("new".to_string(), false)
        );
        let env = json!({});
        assert_eq!(
            parse_after(env.as_object().unwrap(), "old", true),
            ("old".to_string(), true)
        );
    }

    #[test]
    fn compact_reads_cancel_and_instructions() {
        let env = json!({"cancel": true, "instructions": "keep X"});
        let action = parse_compact(env.as_object().unwrap());
        assert!(action.cancel);
        assert_eq!(action.instructions, vec!["keep X".to_string()]);
        assert_eq!(action.summary, None);
    }
}
