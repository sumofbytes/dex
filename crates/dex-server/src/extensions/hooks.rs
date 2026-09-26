//! Hook outcome types: the typed reading of an event handler's directive
//! envelope. The orchestration (running `tool.before` across extensions,
//! fail-open vs `strict`) lives on the manager; this module owns the
//! envelope parsing plus its unit tests.

use serde_json::{Map, Value as Json};

/// Result of the `tool.before` chain for one tool call.
pub enum BeforeOutcome {
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
pub struct AfterOutcome {
    pub text: String,
    pub ok: bool,
}

/// What `session.before_compact` handlers decided (merged across handlers:
/// any cancel wins, instruction strings concatenate).
pub struct CompactAction {
    pub cancel: bool,
    pub instructions: Vec<String>,
    /// Full replacement summary (P3 custom compaction): when set, the host
    /// uses it verbatim instead of generating one.
    pub summary: Option<String>,
}

/// Read one extension's `tool.before` envelope: `deny`/`reason` plus the
/// mutated `args` object. Non-object `args` is an error (fail-open skip vs
/// strict deny is the manager's decision).
/// Parsed `tool.before` directive: mutated args plus an optional deny.
type BeforeDirective = (Map<String, Json>, Option<(bool, String)>);

pub fn parse_before(envelope: &Map<String, Json>) -> Result<BeforeDirective, String> {
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
/// and error escalation. Absent keys keep the host result.
pub fn parse_after(envelope: &Map<String, Json>, text: &str, ok: bool) -> (String, bool) {
    let content = envelope
        .get("content")
        .and_then(|c| c.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| text.to_string());
    // A hook may flag a result as an error (escalate), never clear one:
    // `is_error=false` on a failed result is ignored — an extension must not
    // be able to rewrite a host-reported failure into a success the model
    // would trust.
    let ok = if envelope.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
        false
    } else {
        ok
    };
    (content, ok)
}

/// `supervisor.route` action: redirect a spawn to another definition,
/// deny it, or no opinion (normal flow). The deny carries the denying
/// extension id plus its reason for attribution.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SupervisorAction {
    /// Redirect target: must name a known definition, else the host keeps
    /// the requested agent and logs the miss.
    pub agent: Option<String>,
    /// `(by, reason)` when a handler denied the spawn.
    pub deny: Option<(String, String)>,
}

/// Read one extension's `supervisor.route` envelope: `{redirect = "name"}`
/// and/or `{deny = true, reason = "..."}`. The directive key is `redirect`
/// (not `agent`) so it can't collide with the request's `agent` field, which
/// the envelope echoes back. Returns the redirect target and the deny reason
/// (empty/garbage = no opinion on that half).
pub fn parse_supervisor(envelope: &Map<String, Json>) -> (Option<String>, Option<String>) {
    let agent = envelope
        .get("redirect")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    let deny = match envelope.get("deny").and_then(|v| v.as_bool()) {
        Some(true) => Some(
            envelope
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        ),
        _ => None,
    };
    (agent, deny)
}

/// `permission.request` verdict: the hook arbitrates one approval prompt.
/// First explicit decision wins; absent/garbage = no opinion (normal flow).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny,
}

/// Read one extension's `permission.request` envelope: `{decision =
/// "allow"|"deny"}` plus an optional `reason` string. Anything else is no
/// opinion — a broken hook fails open to the normal approval flow, never to
/// a silent allow or a surprise deny.
pub fn parse_permission(envelope: &Map<String, Json>) -> Option<(PermissionDecision, String)> {
    let decision = match envelope.get("decision").and_then(|v| v.as_str()) {
        Some("allow") => PermissionDecision::Allow,
        Some("deny") => PermissionDecision::Deny,
        _ => return None,
    };
    let reason = envelope
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some((decision, reason))
}

/// Read one extension's `harness.overflow` / `harness.conflict` envelope:
/// `{overflow = bool}` or `{conflicts = bool}`. Absent/non-bool = no opinion
/// (`None`), so the host falls back to the Rust default.
pub fn parse_harness_bool(envelope: &Map<String, Json>, key: &str) -> Option<bool> {
    envelope.get(key).and_then(|v| v.as_bool())
}

/// Read one extension's `session.before_compact` envelope.
pub fn parse_compact(envelope: &Map<String, Json>) -> CompactAction {
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
        // Escalation only: a hook cannot clear a host-reported failure.
        let env = json!({"is_error": false});
        assert_eq!(
            parse_after(env.as_object().unwrap(), "old", false),
            ("old".to_string(), false)
        );
    }

    #[test]
    fn harness_bool_reads_named_key() {
        let env = json!({"overflow": true});
        assert_eq!(
            parse_harness_bool(env.as_object().unwrap(), "overflow"),
            Some(true)
        );
        let env = json!({"conflicts": false});
        assert_eq!(
            parse_harness_bool(env.as_object().unwrap(), "conflicts"),
            Some(false)
        );
        // Absent/non-bool = no opinion, host keeps the Rust default.
        let env = json!({});
        assert_eq!(
            parse_harness_bool(env.as_object().unwrap(), "overflow"),
            None
        );
        let env = json!({"overflow": "yes"});
        assert_eq!(
            parse_harness_bool(env.as_object().unwrap(), "overflow"),
            None
        );
    }

    #[test]
    fn permission_parses_allow_deny_and_rejects_garbage() {
        let env = json!({"decision": "allow"});
        assert_eq!(
            parse_permission(env.as_object().unwrap()),
            Some((PermissionDecision::Allow, String::new()))
        );
        let env = json!({"decision": "deny", "reason": "nope"});
        assert_eq!(
            parse_permission(env.as_object().unwrap()),
            Some((PermissionDecision::Deny, "nope".to_string()))
        );
        // Absent/garbage = no opinion, host keeps the normal approval flow.
        for env in [
            json!({}),
            json!({"decision": "maybe"}),
            json!({"decision": true}),
        ] {
            assert_eq!(parse_permission(env.as_object().unwrap()), None);
        }
    }

    #[test]
    fn compact_reads_cancel_and_instructions() {
        let env = json!({"cancel": true, "instructions": "keep X"});
        let action = parse_compact(env.as_object().unwrap());
        assert!(action.cancel);
        assert_eq!(action.instructions, vec!["keep X".to_string()]);
        assert_eq!(action.summary, None);
    }

    #[test]
    fn supervisor_parses_redirect_deny_and_no_opinion() {
        let env = json!({"redirect": "researcher"});
        assert_eq!(
            parse_supervisor(env.as_object().unwrap()),
            (Some("researcher".to_string()), None)
        );
        let env = json!({"deny": true, "reason": "nope"});
        assert_eq!(
            parse_supervisor(env.as_object().unwrap()),
            (None, Some("nope".to_string()))
        );
        // Both halves at once.
        let env = json!({"redirect": "coder", "deny": true});
        assert_eq!(
            parse_supervisor(env.as_object().unwrap()),
            (Some("coder".to_string()), Some(String::new()))
        );
        // Empty/garbage = no opinion on that half.
        let env = json!({});
        assert_eq!(parse_supervisor(env.as_object().unwrap()), (None, None));
        let env = json!({"redirect": "  ", "deny": false});
        assert_eq!(parse_supervisor(env.as_object().unwrap()), (None, None));
    }
}
