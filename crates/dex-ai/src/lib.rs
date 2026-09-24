//! Provider-neutral data types for LLM requests, responses, and streaming.
//!
//! These types form the shared API surface between dex provider clients and
//! the agent runtime; this crate does not depend on the dex application.

pub mod anthropic;
pub mod provider;
pub mod transport;
pub mod streaming {
    pub mod parser;
}
pub mod wire;

pub use provider::{AuthScheme, ANTHROPIC_VERSION};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

/// Cancellation probe supplied by the host runtime.
pub trait CancellationSource: Send + Sync {
    fn is_cancelled(&self) -> bool;
    fn take_cancelled(&self) -> bool;
}

/// Provider output intended for a host's streaming presentation layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelEvent {
    Assistant(String),
    Thinking(String),
    System(String),
}

/// One normalized model response, independent of provider wire format.
#[derive(Debug)]
pub struct Turn {
    pub message: ChatMessage,
    pub usage: Option<Usage>,
    pub stop_reason: Option<StopReason>,
}

/// Host-implementable model API used by the agent runtime.
#[allow(async_fn_in_trait)]
pub trait ModelClient: Clone + Send + Sync {
    async fn complete(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        events: Option<mpsc::Sender<ModelEvent>>,
        cancel: &(dyn CancellationSource + Send + Sync),
    ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>>;
}

/// Message role on the wire. Serialized lowercase; only these four exist —
/// session JSONL from older builds used the same strings.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    #[default]
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// `name` tag for `!!` shell runs: persisted in the session and shown in
/// the transcript, but filtered out of the model-bound history (`!!`
/// runs are saved and shown without sending output to the LLM). The single spelling lives
/// here; `ChatMessage::is_context_excluded` is the only check.
pub const BASH_EXCLUDED_NAME: &str = "bash-excluded";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatMessage {
    pub role: Role,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<LlmToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Raw Responses reasoning items emitted with this turn (carrying
    /// `encrypted_content`); replayed verbatim so a stateless `store:false`
    /// request hands the model its own reasoning thread back instead of
    /// making it re-reason from scratch every tool call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_items: Option<Vec<Value>>,
    /// DeepSeek-style reasoning text, replayed on assistant messages for
    /// chat-completions providers that stream `reasoning_content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
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

    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, Some(content.into()))
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, Some(content.into()))
    }

    /// User message carrying a tag in `name` (`summary`, `steering`,
    /// `skill`, `waive`, `follow-up`) that compaction and the UI key on.
    pub fn user_named(content: impl Into<String>, name: impl Into<String>) -> Self {
        let mut msg = Self::user(content);
        msg.name = Some(name.into());
        msg
    }

    /// Plain assistant text message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, Some(content.into()))
    }

    /// Assistant message carrying provider tool calls.
    pub fn assistant_calls(content: Option<String>, calls: Vec<LlmToolCall>) -> Self {
        let mut msg = Self::new(Role::Assistant, content);
        msg.tool_calls = Some(calls);
        msg
    }

    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        let mut msg = Self::new(Role::Tool, Some(content.into()));
        msg.tool_call_id = Some(call_id.into());
        msg
    }

    /// Message text, empty when the wire shape omitted `content`.
    pub fn content_str(&self) -> &str {
        self.content.as_deref().unwrap_or_default()
    }

    /// True for `!!` shell runs, which the transcript shows but the model
    /// must never see (see [`BASH_EXCLUDED_NAME`]).
    pub fn is_context_excluded(&self) -> bool {
        self.name.as_deref() == Some(BASH_EXCLUDED_NAME)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LlmToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Chat-completions wire view of a [`ChatMessage`]: borrowed, serialized
/// straight to bytes by reqwest with no `Value` middleman (perf doc §8).
/// Dex-internal fields (`name`, `reasoning_items`) are absent by
/// construction, so strict OpenAI-compatible endpoints never see them.
/// Every other field mirrors [`ChatMessage`]'s serde shape exactly
/// (`content: None` still serializes as `null`, as before).
#[derive(Serialize)]
pub struct WireMessage<'a> {
    pub role: Role,
    pub content: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<&'a Vec<LlmToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<&'a str>,
}

impl ChatMessage {
    /// Borrowed wire view for the chat-completions request body.
    pub fn wire(&self) -> WireMessage<'_> {
        WireMessage {
            role: self.role,
            content: self.content.as_deref(),
            tool_calls: self.tool_calls.as_ref(),
            tool_call_id: self.tool_call_id.as_deref(),
            reasoning_content: self.reasoning_content.as_deref(),
        }
    }
}

/// Serialized straight onto the wire; `messages` arrive wire-shaped after
/// provider-specific message conversion, so dex-internal
/// `ChatMessage` fields never reach a strict OpenAI-compatible endpoint.
/// (Named `...Completions...` to stay clear of `protocol::ChatRequest`, the
/// daemon HTTP request.)
#[derive(Serialize)]
pub struct ChatCompletionsRequest<'a> {
    pub model: &'a str,
    pub messages: Vec<WireMessage<'a>>,
    pub tools: Vec<ToolDefinition>,
    pub stream: bool,
    pub stream_options: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: &'a Option<String>,
}

/// Wire protocol spoken by the active endpoint. Canonical names
/// (`openai-completions`, `openai-responses`, `anthropic-messages`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ApiProtocol {
    ChatCompletions,
    Responses,
    /// Anthropic Messages API: `POST /v1/messages` with block-shaped
    /// content, `x-api-key` auth, and its own SSE event vocabulary.
    Anthropic,
}

impl ApiProtocol {
    /// Parse `api` names (`openai-completions` / `openai-responses` /
    /// `anthropic-messages` plus short aliases). None for anything
    /// else.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "responses" | "openai-responses" => Some(Self::Responses),
            "chat" | "chat-completions" | "openai-completions" => Some(Self::ChatCompletions),
            "anthropic" | "anthropic-messages" => Some(Self::Anthropic),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Responses => "openai-responses",
            Self::ChatCompletions => "openai-completions",
            Self::Anthropic => "anthropic-messages",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Provider {
    OpenAiCodex,
    /// Native Anthropic Messages provider (`anthropic/<model>`): speaks
    /// `anthropic-messages`, authenticates with `x-api-key` +
    /// `anthropic-version`, lands on api.anthropic.com. Pricing, models and
    /// the key env var (`ANTHROPIC_API_KEY`) come from the models.dev
    /// catalog entry of the same key.
    Anthropic,
    /// Any configured OpenAI-compatible provider (`providers:` map in
    /// config.yaml); the string is the catalog/config key ("zai",
    /// "openrouter", …). Auth is a bearer key; endpoint, models, pricing and
    /// protocol all come from the models.dev catalog entry of the same key.
    Generic(String),
}

impl Provider {
    /// Builtin provider names (no file entry needed): every spelling
    /// `parse_known` accepts without consulting `known`, including the
    /// `codex` alias (alias spellings dedupe to one canonical provider
    /// downstream). Keep in sync with `parse_known` — a test pins this.
    pub const BUILTINS: &[&str] = &["openai-codex", "codex", "anthropic"];

    /// Resolve a provider name: builtins plus configured generic providers
    /// (`known`). Returns None for unknown names — call sites that route
    /// model-id prefixes must keep those as part of the model id. The only
    /// constructor used for real resolution; `from_display` covers the
    /// display-only echo path.
    pub fn parse_known(value: &str, known: &std::collections::BTreeSet<String>) -> Option<Self> {
        let lowered = value.trim().to_ascii_lowercase();
        match lowered.as_str() {
            "openai-codex" | "codex" => Some(Self::OpenAiCodex),
            "anthropic" => Some(Self::Anthropic),
            _ => known
                .iter()
                .any(|k| k.eq_ignore_ascii_case(&lowered))
                .then_some(Self::Generic(lowered)),
        }
    }

    /// Display-only parse (daemon echo of an already-resolved provider):
    /// unknown names still become `Generic` so the name survives round-trips.
    // Remote UI connect (TUI) is the only runtime caller.
    pub fn from_display(value: &str) -> Self {
        Self::parse_known(value, &std::collections::BTreeSet::new())
            .unwrap_or_else(|| Self::Generic(value.trim().to_ascii_lowercase()))
    }

    pub fn name(&self) -> &str {
        match self {
            Self::OpenAiCodex => "openai-codex",
            Self::Anthropic => "anthropic",
            Self::Generic(name) => name,
        }
    }
}

#[derive(Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

/// Provider-reported usage for one LLM call, threaded from the stream readers
/// through the agent loop to the status bar. `completion_tokens` is the
/// output-token count (billed at the output rate); `cached_tokens` is the
/// provider-reported cache-hit subset (billed at a fraction of full input
/// price); None when the provider does not report cache detail.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: Option<u64>,
}

/// Normalized terminal condition for a model turn (chat-completions
/// `finish_reason`; the responses API's `response.completed` / `.incomplete`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
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
pub struct StreamUsage {
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(rename = "prompt_tokens_details")]
    pub prompt_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize, Default)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: u64,
}

#[derive(Serialize, Clone)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDef,
}

#[derive(Serialize, Clone)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Deserialize)]
pub struct StreamChunk {
    #[serde(default)]
    pub choices: Vec<StreamChoice>,
    #[serde(default)]
    pub usage: Option<StreamUsage>,
}

#[derive(Deserialize)]
pub struct StreamChoice {
    #[serde(default)]
    pub delta: StreamDelta,
    /// Terminal condition for this choice, sent on the final chunk only
    /// (e.g. "stop", "length", "tool_calls").
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
pub struct StreamDelta {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<StreamToolCall>>,
    /// Reasoning deltas arrive under provider-specific keys (OpenRouter
    /// `reasoning`, DeepSeek-style `reasoning_content`) and some providers
    /// send non-string shapes; `Value` keeps a stray shape from failing the
    /// whole chunk parse.
    #[serde(default)]
    pub reasoning: Option<Value>,
    #[serde(default)]
    pub reasoning_content: Option<Value>,
}

#[derive(Deserialize)]
pub struct StreamToolCall {
    pub index: usize,
    pub id: Option<String>,
    pub function: Option<StreamFunctionCall>,
}

#[derive(Deserialize)]
pub struct StreamFunctionCall {
    pub name: Option<String>,
    pub arguments: Option<String>,
}
