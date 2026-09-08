use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub(crate) struct Skill {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) path: PathBuf,
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
        // A role string outside the four canonical ones is rejected, not
        // silently remapped.
        assert!(serde_json::from_str::<ChatMessage>(r#"{"role":"mystery"}"#).is_err());
    }
}

/// A single streamed line destined for the UI transcript. Plain text (no
/// ANSI) so the UI applies its own styling. Ratatui-agnostic.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum SinkLine {
    Assistant(String),
    /// Incremental model reasoning ("thinking") delta. UIs render a collapsed
    /// one-line preview and can expand the full text on demand.
    Thinking(String),
    ToolInput(String),
    ToolOutput {
        name: String,
        summary: String,
        /// Whether the tool call succeeded; rendered as ✓/✗ by the UIs.
        success: bool,
        /// A few informational output lines shown dim under the summary.
        preview: Vec<String>,
        /// Wall-clock seconds the tool took; 0 when unknown.
        duration: f64,
    },
    System(String),
    Error(String),
    /// Prompt tokens reported by the provider after each LLM call, so the
    /// status bar can track context usage live instead of once per turn.
    /// `cached` is the provider-reported cached-token subset, when reported;
    /// `cost` is the USD cost of the call as priced by the daemon; `output`
    /// is the completion-token count for the same call; `gen_ms` is the
    /// wall-clock duration the caller measured for the whole LLM call, when
    /// known (the denominator for the footer's output tokens/s rate).
    Usage {
        tokens: u64,
        cached: Option<u64>,
        cost: f64,
        output: u64,
        gen_ms: Option<u64>,
    },
    Plan(Plan),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApprovalDecision {
    Once,
    Session,
    Deny,
}

pub(crate) struct ApprovalRequest {
    pub name: String,
    pub input: String,
    pub response: tokio::sync::mpsc::Sender<ApprovalDecision>,
}

/// Width of a string as displayed, ignoring ANSI escape sequences.
/// dividers, truncated to fit the terminal width.
/// Message role on the wire. Serialized lowercase; only these four exist —
/// session JSONL from older builds used the same strings.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Role {
    System,
    #[default]
    User,
    Assistant,
    Tool,
}

impl Role {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct ChatMessage {
    pub(crate) role: Role,
    pub(crate) content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_calls: Option<Vec<LlmToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    /// Raw Responses reasoning items emitted with this turn (carrying
    /// `encrypted_content`); replayed verbatim so a stateless `store:false`
    /// request hands the model its own reasoning thread back instead of
    /// making it re-reason from scratch every tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_items: Option<Vec<Value>>,
    /// DeepSeek-style reasoning text, replayed on assistant messages for
    /// chat-completions providers that stream `reasoning_content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_content: Option<String>,
}

impl ChatMessage {
    fn new(role: Role, content: Option<String>) -> Self {
        Self {
            role,
            content,
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_items: None,
            reasoning_content: None,
        }
    }

    pub(crate) fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, Some(content.into()))
    }

    pub(crate) fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, Some(content.into()))
    }

    /// User message carrying a tag in `name` (`summary`, `steering`,
    /// `skill`, `waive`, `follow-up`) that compaction and the UI key on.
    pub(crate) fn user_named(content: impl Into<String>, name: impl Into<String>) -> Self {
        let mut msg = Self::user(content);
        msg.name = Some(name.into());
        msg
    }

    /// Test-only: plain assistant text message. Production assistant
    /// messages are built by the stream parser, not constructors.
    #[cfg(test)]
    pub(crate) fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, Some(content.into()))
    }

    #[cfg(test)]
    pub(crate) fn assistant_calls(content: Option<String>, calls: Vec<LlmToolCall>) -> Self {
        let mut msg = Self::new(Role::Assistant, content);
        msg.tool_calls = Some(calls);
        msg
    }

    pub(crate) fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        let mut msg = Self::new(Role::Tool, Some(content.into()));
        msg.tool_call_id = Some(call_id.into());
        msg
    }

    /// Message text, empty when the wire shape omitted `content`.
    pub(crate) fn content_str(&self) -> &str {
        self.content.as_deref().unwrap_or_default()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct LlmToolCall {
    pub(crate) id: String,
    #[serde(rename = "type")]
    pub(crate) call_type: String,
    pub(crate) function: FunctionCall,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct FunctionCall {
    pub(crate) name: String,
    pub(crate) arguments: String,
}

/// Serialized straight onto the wire; borrows the request history instead of
/// cloning it per call (`messages` can hold the whole compacted session).
#[derive(Serialize)]
pub(crate) struct ChatRequest<'a> {
    pub(crate) model: &'a str,
    pub(crate) messages: &'a [ChatMessage],
    pub(crate) tools: Vec<ToolDefinition>,
    pub(crate) stream: bool,
    pub(crate) stream_options: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_effort: &'a Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ApiProtocol {
    ChatCompletions,
    Responses,
}

impl ApiProtocol {
    /// Parse pi's `api` names (`openai-completions` / `openai-responses`
    /// plus pi's short aliases). None for anything else — including pi APIs
    /// outside dex's OpenAI-compatible subset (`anthropic-messages`, …).
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "responses" | "openai-responses" => Some(Self::Responses),
            "chat" | "chat-completions" | "openai-completions" => Some(Self::ChatCompletions),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Responses => "openai-responses",
            Self::ChatCompletions => "openai-completions",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Provider {
    OpenCode,
    OpenAiCodex,
    /// Any configured OpenAI-compatible provider (`providers:` map in
    /// config.yaml); the string is the catalog/config key ("zai",
    /// "openrouter", …). Auth is a bearer key; endpoint, models, pricing and
    /// protocol all come from the models.dev catalog entry of the same key.
    Generic(String),
}

impl Provider {
    /// Resolve a provider name: builtins plus configured generic providers
    /// (`known`). Returns None for unknown names — call sites that route
    /// model-id prefixes must keep those as part of the model id. The only
    /// constructor used for real resolution; `from_display` covers the
    /// display-only echo path.
    pub(crate) fn parse_known(
        value: &str,
        known: &std::collections::BTreeSet<String>,
    ) -> Option<Self> {
        let lowered = value.trim().to_ascii_lowercase();
        match lowered.as_str() {
            "opencode" => Some(Self::OpenCode),
            "openai-codex" | "codex" => Some(Self::OpenAiCodex),
            _ => known
                .iter()
                .any(|k| k.eq_ignore_ascii_case(&lowered))
                .then_some(Self::Generic(lowered)),
        }
    }

    /// Display-only parse (daemon echo of an already-resolved provider):
    /// unknown names still become `Generic` so the name survives round-trips.
    pub(crate) fn from_display(value: &str) -> Self {
        Self::parse_known(value, &std::collections::BTreeSet::new())
            .unwrap_or_else(|| Self::Generic(value.trim().to_ascii_lowercase()))
    }

    pub(crate) fn name(&self) -> &str {
        match self {
            Self::OpenCode => "opencode",
            Self::OpenAiCodex => "openai-codex",
            Self::Generic(name) => name,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct StreamOptions {
    pub(crate) include_usage: bool,
}

/// Provider-reported usage for one LLM call, threaded from the stream readers
/// through the agent loop to the status bar. `completion_tokens` is the
/// output-token count (billed at the output rate); `cached_tokens` is the
/// provider-reported cache-hit subset (billed at a fraction of full input
/// price); None when the provider does not report cache detail.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Usage {
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) cached_tokens: Option<u64>,
}

/// Normalized terminal condition for a model turn (chat-completions
/// `finish_reason`; the responses API's `response.completed` / `.incomplete`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopReason {
    /// Model finished its reply normally.
    Stop,
    /// Cut off by the output-token limit — the reply is likely truncated.
    Length,
    /// Stopped to execute tool calls.
    ToolUse,
    /// Cut off by a provider-side content filter — the reply is partial.
    ContentFilter,
}

/// Chat-completions wire shape for usage. Cache detail nests under
/// `prompt_tokens_details`, so it needs its own deserialization target.
#[derive(Deserialize, Default)]
pub(crate) struct StreamUsage {
    pub(crate) prompt_tokens: u64,
    #[serde(default)]
    pub(crate) completion_tokens: u64,
    #[serde(rename = "prompt_tokens_details")]
    pub(crate) prompt_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize, Default)]
pub(crate) struct PromptTokensDetails {
    #[serde(default)]
    pub(crate) cached_tokens: u64,
}

#[derive(Serialize, Clone)]
pub(crate) struct ToolDefinition {
    #[serde(rename = "type")]
    pub(crate) tool_type: String,
    pub(crate) function: FunctionDef,
}

#[derive(Serialize, Clone)]
pub(crate) struct FunctionDef {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) parameters: Value,
}

#[derive(Deserialize)]
pub(crate) struct StreamChunk {
    #[serde(default)]
    pub(crate) choices: Vec<StreamChoice>,
    #[serde(default)]
    pub(crate) usage: Option<StreamUsage>,
}

#[derive(Deserialize)]
pub(crate) struct StreamChoice {
    #[serde(default)]
    pub(crate) delta: StreamDelta,
    /// Terminal condition for this choice, sent on the final chunk only
    /// (e.g. "stop", "length", "tool_calls").
    #[serde(default)]
    pub(crate) finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
pub(crate) struct StreamDelta {
    pub(crate) content: Option<String>,
    pub(crate) tool_calls: Option<Vec<StreamToolCall>>,
    /// Reasoning deltas arrive under provider-specific keys (OpenRouter
    /// `reasoning`, DeepSeek-style `reasoning_content`) and some providers
    /// send non-string shapes; `Value` keeps a stray shape from failing the
    /// whole chunk parse.
    #[serde(default)]
    pub(crate) reasoning: Option<Value>,
    #[serde(default)]
    pub(crate) reasoning_content: Option<Value>,
}

#[derive(Deserialize)]
pub(crate) struct StreamToolCall {
    pub(crate) index: usize,
    pub(crate) id: Option<String>,
    pub(crate) function: Option<StreamFunctionCall>,
}

#[derive(Deserialize)]
pub(crate) struct StreamFunctionCall {
    pub(crate) name: Option<String>,
    pub(crate) arguments: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PermissionMode {
    /// Permit reads, but reject all mutations and shell commands.
    ReadOnly,
    /// Prompt before writes and edits; reads are always permitted.
    AskWrites,
    /// Prompt before shell commands; reads and file mutations are permitted.
    AskShell,
    /// Permit every tool without prompting (useful for automation).
    Trusted,
}

impl PermissionMode {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().replace('_', "-").as_str() {
            "read-only" | "readonly" => Ok(Self::ReadOnly),
            "ask-writes" | "ask-write" => Ok(Self::AskWrites),
            "ask-shell" | "ask-commands" => Ok(Self::AskShell),
            "trusted" | "non-interactive" => Ok(Self::Trusted),
            other => Err(format!(
                "invalid permission mode '{}'; use read-only, ask-writes, ask-shell, or trusted",
                other
            )),
        }
    }
    pub(crate) fn permissiveness(self) -> u8 {
        match self {
            Self::ReadOnly => 0,
            Self::AskWrites => 1,
            Self::AskShell => 2,
            Self::Trusted => 3,
        }
    }
    /// Canonical wire spelling (CLI `--permission`, daemon request field):
    /// round-trips through [`Self::parse`]. Single source so call sites
    /// never drift from the accepted spellings.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::AskWrites => "ask-writes",
            Self::AskShell => "ask-shell",
            Self::Trusted => "trusted",
        }
    }
}
