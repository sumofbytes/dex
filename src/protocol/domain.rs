use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::protocol::ApprovalDecision;

#[derive(Clone, Debug)]
pub(crate) struct Skill {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) path: PathBuf,
}

/// An approval request parked in the daemon state or the tools policy
/// layer while the human decides. `response` carries the decision back.
pub(crate) struct ApprovalRequest {
    pub name: String,
    pub input: String,
    /// Set when the requester is a background child agent (plan §12 V1b):
    /// children outlive the parent turn, so turn-end teardown must not deny
    /// their parked approvals. `None` for the parent turn's own tools.
    pub agent_id: Option<String>,
    /// The child's definition name for the labeled prompt (V1b): rendered
    /// as "explorer wants to run bash: …". `None` for the parent's own.
    pub agent: Option<String>,
    pub response: tokio::sync::mpsc::Sender<ApprovalDecision>,
}

/// Durable task contract: goal, constraints, acceptance criteria, plan steps
/// with done flags, and a derived completion state. Persisted as JSON in
/// `session_state "plan"`; `#[serde(default)]` keeps partial state loadable
/// (missing keys default instead of failing the whole session load).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub goal: Option<String>,
    pub steps: Vec<(String, bool)>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub acceptance: Vec<(String, bool)>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.goal.is_none()
            && self.steps.is_empty()
            && self.constraints.is_empty()
            && self.acceptance.is_empty()
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
    pub fn from_json(s: &str) -> Self {
        serde_json::from_str(s).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ChatMessage, PermissionMode, Role};

    #[test]
    fn permission_mode_wire_spelling_round_trips() {
        for mode in [
            PermissionMode::ReadOnly,
            PermissionMode::AskWrites,
            PermissionMode::AskShell,
            PermissionMode::Trusted,
        ] {
            assert_eq!(PermissionMode::parse(mode.as_str()), Ok(mode));
        }
    }

    #[test]
    fn plan_round_trip_keeps_contract() {
        let plan = Plan {
            goal: Some("g".into()),
            constraints: vec!["c1".into()],
            steps: vec![("s".into(), false)],
            acceptance: vec![("a1".into(), true)],
        };
        assert_eq!(Plan::from_json(&plan.to_json()), plan);
        // Missing keys default instead of failing the load.
        let parsed = Plan::from_json(r#"{"goal":"g","steps":[["s",false]]}"#);
        assert!(parsed.constraints.is_empty() && parsed.acceptance.is_empty());
        assert_eq!(parsed.steps, vec![("s".to_string(), false)]);
    }

    #[test]
    fn chat_message_round_trips_wire_shape() {
        // Session JSONL stores `role` as a plain string; the typed enum
        // must deserialize it and serialize back to the same bytes.
        let line = r#"{"role":"assistant","content":"hi","tool_calls":[{"id":"c1","type":"function","function":{"name":"read","arguments":"{}"}}],"name":"skill"}"#;
        let msg: ChatMessage = serde_json::from_str(line).expect("line loads");
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.name.as_deref(), Some("skill"));
        let round: ChatMessage = serde_json::from_str(&serde_json::to_string(&msg).unwrap())
            .expect("own output reloads");
        assert_eq!(
            serde_json::to_string(&round).unwrap(),
            serde_json::to_string(&msg).unwrap()
        );
        // Option fields stay omitted when None (compaction/splice compat).
        assert_eq!(
            serde_json::to_string(&ChatMessage::user("hey")).unwrap(),
            r#"{"role":"user","content":"hey"}"#
        );
    }
}
