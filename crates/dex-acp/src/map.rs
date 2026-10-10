//! Pure ACP <-> dex translation: content blocks, tool kinds, session
//! updates, permission options. No I/O, so it is unit-testable.

use dex_client::protocol::{ApprovalDecision, StreamEvent};
use serde_json::{json, Value};

pub const PROTOCOL_VERSION: u64 = 1;

/// Flatten ACP prompt content blocks to the plain text dex sends the model.
/// Text passes through; links and embedded resources are inlined as
/// references; unsupported media is noted rather than silently dropped.
pub fn prompt_text(blocks: &[Value]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => parts.push(block["text"].as_str().unwrap_or_default().to_string()),
            Some("resource_link") => {
                let uri = block["uri"].as_str().unwrap_or_default();
                let name = block["name"].as_str().unwrap_or(uri);
                parts.push(format!("[{name}]({uri})"));
            }
            Some("resource") => {
                let res = &block["resource"];
                let uri = res["uri"].as_str().unwrap_or_default();
                match res["text"].as_str() {
                    Some(text) => {
                        parts.push(format!("<context ref=\"{uri}\">\n{text}\n</context>"))
                    }
                    None => parts.push(format!("[binary resource: {uri}]")),
                }
            }
            Some(other) => parts.push(format!("[unsupported {other} content omitted]")),
            None => {}
        }
    }
    parts.join("\n\n")
}

/// ACP `ToolKind` for a dex tool name.
pub fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "ls" => "read",
        "grep" | "find" => "search",
        "write" | "edit" => "edit",
        "bash" | "task" => "execute",
        "delegate" => "think",
        _ => "other",
    }
}

fn title(name: &str, args: &Value) -> String {
    match args.as_str().filter(|s| !s.is_empty()) {
        Some(arg) => format!("{name} {arg}"),
        None => name.to_string(),
    }
}

fn text_content(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn chunk(kind: &str, text: &str) -> Value {
    json!({"sessionUpdate": kind, "content": text_content(text)})
}

/// User-level prompt content (steered/follow-up messages): the transcript
/// shows who authored it, unlike agent text chunks.
fn user_chunk(text: &str) -> Value {
    chunk("user_message_chunk", text)
}

/// Tracks `tool_call` ids so results pair with their starters even when an
/// older daemon omits ids (pair by name, oldest first).
#[derive(Default)]
pub struct ToolIds {
    open: Vec<(String, String)>,
    counter: u64,
}

impl ToolIds {
    fn start(&mut self, name: &str, id: &str) -> String {
        let id = if id.is_empty() {
            self.counter += 1;
            format!("call_{}", self.counter)
        } else {
            id.to_string()
        };
        self.open.push((name.to_string(), id.clone()));
        id
    }

    fn finish(&mut self, name: &str, id: &str) -> String {
        let pos = if id.is_empty() {
            self.open.iter().position(|(n, _)| n == name)
        } else {
            self.open.iter().position(|(_, i)| i == id)
        };
        match pos {
            Some(i) => self.open.remove(i).1,
            None if !id.is_empty() => id.to_string(),
            None => {
                self.counter += 1;
                format!("call_{}", self.counter)
            }
        }
    }

    /// Id of the most recent open call for `name` — the call an approval
    /// request is gating.
    pub fn pending_for(&self, name: &str) -> Option<&str> {
        self.open
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, id)| id.as_str())
    }
}

/// `session/update` payload for a stream event, or `None` for events ACP
/// has no counterpart for (approvals, questions and terminals are handled by
/// the caller; usage/task/child-agent events are dropped; managed sessions
/// expose steered/follow-up prompts as user chunks instead).
pub fn update_for(event: &StreamEvent, ids: &mut ToolIds) -> Option<Value> {
    match event {
        StreamEvent::AssistantText(text) => Some(chunk("agent_message_chunk", text)),
        StreamEvent::Thinking(text) => Some(chunk("agent_thought_chunk", text)),
        StreamEvent::Error(msg) => Some(chunk("agent_message_chunk", &format!("\nerror: {msg}\n"))),
        StreamEvent::ToolCall { name, args, id } => Some(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": ids.start(name, id),
            "title": title(name, args),
            "kind": tool_kind(name),
            "status": "in_progress",
            "rawInput": args,
        })),
        StreamEvent::ToolResult {
            name,
            summary,
            success,
            preview,
            id,
            ..
        } => {
            let mut text = summary.clone();
            for line in preview {
                text.push('\n');
                text.push_str(line);
            }
            Some(json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": ids.finish(name, id),
                "status": if *success { "completed" } else { "failed" },
                "content": [{"type": "content", "content": text_content(&text)}],
            }))
        }
        StreamEvent::Plan { steps, .. } => Some(json!({
            "sessionUpdate": "plan",
            "entries": steps.iter().map(|(content, done)| json!({
                "content": content,
                "priority": "medium",
                "status": if *done { "completed" } else { "pending" },
            })).collect::<Vec<_>>(),
        })),
        // Steered/follow-up prompts are the user's own words; render them as
        // user chunks so a replayed transcript stays readable.
        StreamEvent::SteeringAccepted { content } | StreamEvent::FollowupAccepted { content } => {
            Some(user_chunk(content))
        }
        _ => None,
    }
}

/// Permission options offered for every approval, in display order.
pub fn permission_options() -> Value {
    json!([
        {"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
        {"optionId": "allow_session", "name": "Allow for session", "kind": "allow_always"},
        {"optionId": "deny", "name": "Deny", "kind": "reject_once"},
    ])
}

/// Decision for a `session/request_permission` response. Anything but an
/// explicit allow (cancelled, unknown option, malformed) denies: fail closed.
pub fn decision_from_outcome(result: &Value) -> ApprovalDecision {
    let outcome = &result["outcome"];
    if outcome["outcome"] != "selected" {
        return ApprovalDecision::Deny;
    }
    match outcome["optionId"].as_str() {
        Some("allow_once") => ApprovalDecision::AllowOnce,
        Some("allow_session") => ApprovalDecision::AllowSession,
        _ => ApprovalDecision::Deny,
    }
}

/// ACP session modes, backed by dex agent modes.
pub const MODES: [(&str, &str, &str); 3] = [
    ("auto", "Auto", "Run tools without asking"),
    (
        "manual",
        "Manual",
        "Ask before writes, edits and shell commands",
    ),
    ("plan", "Plan", "Read-only exploration and planning"),
];

pub fn modes_state(current: &str) -> Value {
    json!({
        "currentModeId": current,
        "availableModes": MODES.iter().map(|(id, name, description)| json!({
            "id": id, "name": name, "description": description,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_prompt_blocks() {
        let blocks = vec![
            json!({"type": "text", "text": "fix it"}),
            json!({"type": "resource_link", "name": "main.rs", "uri": "file:///a/main.rs"}),
            json!({"type": "resource", "resource": {"uri": "file:///b", "text": "hi"}}),
            json!({"type": "image", "data": "..", "mimeType": "image/png"}),
        ];
        let text = prompt_text(&blocks);
        assert!(text.starts_with("fix it\n\n[main.rs](file:///a/main.rs)"));
        assert!(text.contains("<context ref=\"file:///b\">\nhi\n</context>"));
        assert!(text.ends_with("[unsupported image content omitted]"));
    }

    #[test]
    fn pairs_results_with_calls_by_id_and_by_name() {
        let mut ids = ToolIds::default();
        let call = |name: &str, id: &str| StreamEvent::ToolCall {
            name: name.into(),
            args: json!("x"),
            id: id.into(),
        };
        let result = |name: &str, id: &str, success| StreamEvent::ToolResult {
            name: name.into(),
            summary: "ok".into(),
            success,
            preview: vec!["l1".into()],
            duration: 0.0,
            id: id.into(),
        };
        let started = update_for(&call("bash", "c1"), &mut ids).unwrap();
        assert_eq!(started["toolCallId"], "c1");
        assert_eq!(started["kind"], "execute");
        assert_eq!(started["title"], "bash x");
        assert_eq!(ids.pending_for("bash"), Some("c1"));
        let done = update_for(&result("bash", "c1", false), &mut ids).unwrap();
        assert_eq!(done["toolCallId"], "c1");
        assert_eq!(done["status"], "failed");
        assert_eq!(done["content"][0]["content"]["text"], "ok\nl1");
        // Old daemon: no ids on either side.
        let started = update_for(&call("read", ""), &mut ids).unwrap();
        let done = update_for(&result("read", "", true), &mut ids).unwrap();
        assert_eq!(started["toolCallId"], done["toolCallId"]);
    }

    #[test]
    fn permission_outcomes_fail_closed() {
        let sel = |id: &str| json!({"outcome": {"outcome": "selected", "optionId": id}});
        assert_eq!(
            decision_from_outcome(&sel("allow_once")),
            ApprovalDecision::AllowOnce
        );
        assert_eq!(
            decision_from_outcome(&sel("allow_session")),
            ApprovalDecision::AllowSession
        );
        assert_eq!(decision_from_outcome(&sel("deny")), ApprovalDecision::Deny);
        assert_eq!(decision_from_outcome(&sel("bogus")), ApprovalDecision::Deny);
        assert_eq!(
            decision_from_outcome(&json!({"outcome": {"outcome": "cancelled"}})),
            ApprovalDecision::Deny
        );
        assert_eq!(decision_from_outcome(&json!({})), ApprovalDecision::Deny);
    }

    #[test]
    fn plan_and_unmapped_events() {
        let mut ids = ToolIds::default();
        let plan = StreamEvent::Plan {
            goal: None,
            steps: vec![("a".into(), true), ("b".into(), false)],
            constraints: vec![],
            acceptance: vec![],
        };
        let update = update_for(&plan, &mut ids).unwrap();
        assert_eq!(update["entries"][0]["status"], "completed");
        assert_eq!(update["entries"][1]["status"], "pending");
        let steer = update_for(
            &StreamEvent::SteeringAccepted {
                content: "nudge".into(),
            },
            &mut ids,
        )
        .unwrap();
        assert_eq!(steer["sessionUpdate"], "user_message_chunk");
        assert_eq!(steer["content"]["text"], "nudge");
        let followup = update_for(
            &StreamEvent::FollowupAccepted {
                content: "next turn".into(),
            },
            &mut ids,
        )
        .unwrap();
        assert_eq!(followup["sessionUpdate"], "user_message_chunk");
        assert!(update_for(&StreamEvent::System("x".into()), &mut ids).is_none());
    }
}
