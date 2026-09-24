use serde::{Deserialize, Serialize};

/// Durable task contract persisted with a conversation.
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
