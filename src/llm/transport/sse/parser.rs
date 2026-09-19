use crate::llm::protocol::merge_chat_tool_call;
use crate::llm::protocol::response_call_index;
use crate::llm::protocol::response_tool_call;
use crate::protocol::LlmToolCall;
use crate::protocol::StopReason;
use crate::protocol::StreamChunk;
use crate::protocol::StreamDelta;
use crate::protocol::Usage;
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;

/// Reasoning delta under the provider-specific chat-completions key, as a
/// string; non-string shapes (some providers send arrays) yield None.
pub(crate) fn delta_thought(delta: &StreamDelta) -> Option<&str> {
    delta
        .reasoning
        .as_ref()
        .and_then(Value::as_str)
        .or_else(|| delta.reasoning_content.as_ref().and_then(Value::as_str))
}

/// Mid-stream events a parser emits per SSE line; the driver owns what
/// happens to them (printing, accumulation, usage threading, lifecycle).
pub(crate) enum StreamEvent {
    /// Reasoning delta; UI-only, routed to the sink, never persisted.
    Thinking(String),
    /// Content delta; accumulated and printed live by the driver.
    Text(String),
    /// Provider-reported token usage (the last one wins).
    Usage(Usage),
    /// Terminal condition reported by the provider.
    Stop(StopReason),
    /// Protocol-level end of stream (chat-completions `[DONE]`, the
    /// Anthropic `message_stop`); the driver stops reading. Protocols that
    /// end at EOF (responses API) never emit it.
    Done,
    /// Provider-reported failure (e.g. Anthropic's terminal `error` event
    /// on a 200 body). Honors the same output-flowed rule as transport
    /// failures: before any output it stays retryable, after it becomes a
    /// [`MidStreamError`].
    Fail(String),
}

/// Parser-owned parts of the final assistant message; the driver owns
/// `content` and assembles the message itself.
pub(crate) struct ParsedMessage {
    pub(crate) tool_calls: Vec<LlmToolCall>,
    pub(crate) reasoning_items: Option<Vec<Value>>,
    pub(crate) reasoning_content: Option<String>,
}

/// One protocol parser, fed raw SSE lines. All protocol-specific state
/// (tool-call merging, reasoning replay) lives in the implementation.
pub(crate) trait StreamParser {
    fn feed(&mut self, line: &str) -> Vec<StreamEvent>;
    fn finish(self) -> ParsedMessage;
}

fn stop_reason_from_finish(finish: &str) -> Option<StopReason> {
    match finish {
        "stop" => Some(StopReason::Stop),
        "length" => Some(StopReason::Length),
        "tool_calls" | "function_call" => Some(StopReason::ToolUse),
        "content_filter" => Some(StopReason::ContentFilter),
        // Unrecognized provider reason — leave unset rather than guess.
        _ => None,
    }
}

/// `POST /chat/completions` stream: JSON chunks under `data:` (the space
/// after the colon is optional per SSE, like the other parsers accept),
/// terminated by `[DONE]`. Tool-call deltas merge by `index`.
#[derive(Default)]
pub(crate) struct ChatCompletionsParser {
    pub(crate) tool_calls: Vec<LlmToolCall>,
    /// DeepSeek-style reasoning text, replayed on the assistant message.
    pub(crate) reasoning: String,
}

impl StreamParser for ChatCompletionsParser {
    fn feed(&mut self, line: &str) -> Vec<StreamEvent> {
        let Some(data) = line.strip_prefix("data:") else {
            return Vec::new();
        };
        let data = data.trim();
        if data == "[DONE]" {
            return vec![StreamEvent::Done];
        }
        // Providers interleave non-chunk payloads (keep-alives, error
        // notices); skipping one unshapely line beats aborting a
        // multi-minute generation.
        let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) else {
            return Vec::new();
        };
        let mut events = Vec::new();
        if let Some(usage) = &chunk.usage {
            events.push(StreamEvent::Usage(Usage {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                cached_tokens: usage.prompt_details.as_ref().map(|d| d.cached_tokens),
            }));
        }
        for choice in chunk.choices {
            if let Some(text) = delta_thought(&choice.delta) {
                events.push(StreamEvent::Thinking(text.to_string()));
            }
            // DeepSeek-style reasoning text: captured for replay on the
            // assistant message so the provider gets its own reasoning
            // thread back next request (self-gating: only set when the
            // provider streamed this exact field).
            if let Some(text) = choice
                .delta
                .reasoning_content
                .as_ref()
                .and_then(Value::as_str)
            {
                self.reasoning.push_str(text);
            }
            if let Some(text) = choice.delta.content {
                events.push(StreamEvent::Text(text));
            }
            for delta in choice.delta.tool_calls.unwrap_or_default() {
                merge_chat_tool_call(&mut self.tool_calls, delta);
            }
            if let Some(reason) = &choice.finish_reason {
                if let Some(stop) = stop_reason_from_finish(reason) {
                    events.push(StreamEvent::Stop(stop));
                }
            }
        }
        events
    }

    fn finish(self) -> ParsedMessage {
        ParsedMessage {
            tool_calls: self.tool_calls,
            reasoning_items: None,
            reasoning_content: (!self.reasoning.is_empty()).then_some(self.reasoning),
        }
    }
}

/// `POST /responses` stream: typed JSON events under `data:`; the body ends
/// at EOF. Tool calls arrive as items (added/done) plus argument deltas.
#[derive(Default)]
pub(crate) struct ResponsesParser {
    pub(crate) tool_calls: Vec<LlmToolCall>,
    pub(crate) response_items: HashMap<String, usize>,
    pub(crate) pending_arguments: HashMap<String, String>,
    pub(crate) reasoning_items: Vec<Value>,
}

impl ResponsesParser {
    /// Usage + terminal condition from a completed/incomplete response event.
    fn response_terminal(&self, response: &Value, events: &mut Vec<StreamEvent>) {
        if let Some(usage) = response.get("usage") {
            if let Some(input) = usage.get("input_tokens").and_then(Value::as_u64) {
                events.push(StreamEvent::Usage(Usage {
                    prompt_tokens: input,
                    completion_tokens: usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    cached_tokens: usage
                        .pointer("/input_tokens_details/cached_tokens")
                        .and_then(Value::as_u64),
                }));
            }
        }
        // `response.completed` (no incomplete details) is a clean stop;
        // `response.incomplete` reports why the reply was cut off. An
        // unrecognized reason stays unset rather than claiming a clean stop.
        let stop = match response
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
        {
            Some("max_output_tokens") => Some(StopReason::Length),
            Some("content_filter") => Some(StopReason::ContentFilter),
            Some(_) => None,
            None => Some(StopReason::Stop),
        };
        if let Some(stop) = stop {
            events.push(StreamEvent::Stop(stop));
        }
    }

    /// Function-call state machine shared verbatim by `output_item.added`
    /// and `output_item.done`: index the item, merge its seed state, and
    /// flush any arguments that arrived before the item was added.
    fn handle_function_call_item(&mut self, event: &Value) {
        if event.pointer("/item/type").and_then(Value::as_str) != Some("function_call") {
            return;
        }
        let item = event.get("item").unwrap_or(&Value::Null);
        let index = response_call_index(
            &self.tool_calls,
            event
                .get("output_index")
                .and_then(Value::as_u64)
                .unwrap_or(self.tool_calls.len() as u64) as usize,
            item,
        );
        response_tool_call(&mut self.tool_calls, index, item);
        if let Some(item_id) = item.get("id").and_then(Value::as_str) {
            self.response_items.insert(item_id.to_string(), index);
            if let Some(arguments) = self.pending_arguments.remove(item_id) {
                if let Some(call) = self.tool_calls.get_mut(index) {
                    call.function.arguments.push_str(&arguments);
                }
            }
        }
    }
}

impl StreamParser for ResponsesParser {
    fn feed(&mut self, line: &str) -> Vec<StreamEvent> {
        let Some(data) = line.strip_prefix("data:") else {
            return Vec::new();
        };
        let data = data.trim();
        if data == "[DONE]" || data.is_empty() {
            return Vec::new();
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut events = Vec::new();
        match event_type {
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    events.push(StreamEvent::Thinking(delta.to_string()));
                }
            }
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    events.push(StreamEvent::Text(delta.to_string()));
                }
            }
            "response.output_item.added" => self.handle_function_call_item(&event),
            "response.function_call_arguments.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    let key = event
                        .get("item_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| {
                            format!(
                                "output:{}",
                                event
                                    .get("output_index")
                                    .and_then(Value::as_u64)
                                    .unwrap_or(0)
                            )
                        });
                    if let Some(index) = event
                        .get("item_id")
                        .and_then(Value::as_str)
                        .and_then(|id| self.response_items.get(id).copied())
                    {
                        if let Some(call) = self.tool_calls.get_mut(index) {
                            call.function.arguments.push_str(delta);
                        }
                    } else {
                        self.pending_arguments
                            .entry(key)
                            .or_default()
                            .push_str(delta);
                    }
                }
            }
            "response.output_item.done" => {
                if event.pointer("/item/type").and_then(Value::as_str) == Some("reasoning") {
                    // Keep raw reasoning items for stateless replay next
                    // request; summary-only items (no encrypted_content)
                    // can't be replayed and would corrupt the thread.
                    let item = event.get("item").unwrap_or(&Value::Null);
                    if item.get("encrypted_content").is_some_and(|v| {
                        !v.is_null() && v.as_str().map(|s| !s.is_empty()).unwrap_or(true)
                    }) {
                        self.reasoning_items.push(item.clone());
                    }
                } else {
                    self.handle_function_call_item(&event);
                }
            }
            "response.completed" | "response.done" | "response.incomplete" => {
                if let Some(response) = event.get("response") {
                    self.response_terminal(response, &mut events);
                }
            }
            _ => {}
        }
        events
    }

    fn finish(self) -> ParsedMessage {
        // Completed calls without an id must be dropped, not executed.
        let mut tool_calls = self.tool_calls;
        tool_calls.retain(|call| !call.id.is_empty() && !call.function.name.is_empty());
        ParsedMessage {
            tool_calls,
            reasoning_items: (!self.reasoning_items.is_empty()).then_some(self.reasoning_items),
            reasoning_content: None,
        }
    }
}

fn stop_reason_from_anthropic(stop: &str) -> Option<StopReason> {
    match stop {
        "end_turn" | "stop_sequence" => Some(StopReason::Stop),
        "max_tokens" | "model_context_window" => Some(StopReason::Length),
        "tool_use" => Some(StopReason::ToolUse),
        "refusal" => Some(StopReason::ContentFilter),
        // `pause_turn` suspends a server-tool turn for continuation; dex
        // drives its own turns, so a paused stream is just a finished one.
        "pause_turn" => Some(StopReason::Stop),
        // Unrecognized provider reason — leave unset rather than guess.
        _ => None,
    }
}

/// One indexed content block under construction (text / thinking /
/// tool_use). Anthropic streams per-block deltas keyed by `index`, so each
/// block accumulates its own state until `content_block_stop`.
#[derive(Default)]
pub(crate) struct AnthropicBlock {
    pub(crate) kind: String,
    pub(crate) id: String,
    pub(crate) name: String,
    /// `input_json_delta` fragments for tool_use blocks.
    pub(crate) json: String,
    /// thinking_delta accumulation (replayed for signed thinking blocks).
    pub(crate) text: String,
    /// signature_delta accumulation (required to replay thinking).
    pub(crate) signature: String,
    /// Complete block captured at start for types with no deltas
    /// (`redacted_thinking` arrives whole).
    pub(crate) raw: Option<Value>,
}

/// `POST /v1/messages` stream (Anthropic Messages wire): typed events under
/// `data:`, body ends with `message_stop` (or EOF). Usage arrives split —
/// prompt-side counts on `message_start`, cumulative output tokens on
/// `message_delta` — so the parser accumulates and emits one complete
/// [`Usage`] per update instead of letting the last event clobber the rest.
#[derive(Default)]
pub(crate) struct AnthropicParser {
    pub(crate) blocks: HashMap<u64, AnthropicBlock>,
    pub(crate) tool_calls: Vec<LlmToolCall>,
    /// Signature-carrying thinking blocks (+ whole redacted_thinking
    /// blocks) for stateless replay on the next request.
    pub(crate) reasoning_items: Vec<Value>,
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read: Option<u64>,
    pub(crate) cache_creation: u64,
}

impl AnthropicParser {
    fn block(&mut self, event: &Value) -> Option<&mut AnthropicBlock> {
        let index = event.get("index").and_then(Value::as_u64)?;
        self.blocks.get_mut(&index)
    }

    /// Merge one usage object (`message_start` / `message_delta` shape):
    /// every field is optional and only updates when present.
    fn absorb_usage(&mut self, usage: &Value) {
        if let Some(v) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.input_tokens = v;
        }
        if let Some(v) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.output_tokens = v;
        }
        if let Some(v) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.cache_read = Some(v);
        }
        if let Some(v) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
        {
            self.cache_creation = v;
        }
    }

    /// Prompt side counts every token that occupies context: uncached
    /// input, cache reads, and cache writes; cache reads are surfaced
    /// separately for cost discounting.
    fn current_usage(&self) -> Usage {
        Usage {
            prompt_tokens: self.input_tokens + self.cache_read.unwrap_or(0) + self.cache_creation,
            completion_tokens: self.output_tokens,
            cached_tokens: self.cache_read,
        }
    }
}

impl StreamParser for AnthropicParser {
    fn feed(&mut self, line: &str) -> Vec<StreamEvent> {
        let Some(data) = line.strip_prefix("data:") else {
            return Vec::new();
        };
        let data = data.trim();
        if data.is_empty() {
            return Vec::new();
        }
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut events = Vec::new();
        match event_type {
            // Terminal failure on a 200 body (overloaded_error, …): the
            // driver turns this into a real error, retryable only before
            // any output flowed.
            "error" => {
                let kind = event
                    .pointer("/error/type")
                    .and_then(Value::as_str)
                    .unwrap_or("api_error");
                let message = event
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown provider error");
                events.push(StreamEvent::Fail(format!("{kind}: {message}")));
            }
            "message_start" => {
                if let Some(usage) = event.pointer("/message/usage") {
                    self.absorb_usage(usage);
                }
            }
            "content_block_start" => {
                let block = event.get("content_block").cloned().unwrap_or(Value::Null);
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                let kind = block
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.blocks.insert(
                    index,
                    AnthropicBlock {
                        id: block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        raw: (kind == "redacted_thinking").then_some(block),
                        kind,
                        ..Default::default()
                    },
                );
            }
            "content_block_delta" => {
                let delta = event.get("delta").cloned().unwrap_or(Value::Null);
                match delta
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "text_delta" => {
                        // Text flows to the caller via StreamEvent::Text;
                        // blocks only accumulate state that gets replayed
                        // (thinking), so nothing to keep here.
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            events.push(StreamEvent::Text(text.to_string()));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                            events.push(StreamEvent::Thinking(text.to_string()));
                            if let Some(block) = self.block(&event) {
                                block.text.push_str(text);
                            }
                        }
                    }
                    "signature_delta" => {
                        if let Some(sig) = delta.get("signature").and_then(Value::as_str) {
                            if let Some(block) = self.block(&event) {
                                block.signature.push_str(sig);
                            }
                        }
                    }
                    "input_json_delta" => {
                        if let Some(fragment) = delta.get("partial_json").and_then(Value::as_str) {
                            if let Some(block) = self.block(&event) {
                                block.json.push_str(fragment);
                            }
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                if let Some(block) = self.blocks.remove(&index) {
                    match block.kind.as_str() {
                        "tool_use" => {
                            // Completed calls without an id/name must be
                            // dropped, not executed (responses-parser rule).
                            if !block.id.is_empty() && !block.name.is_empty() {
                                self.tool_calls.push(LlmToolCall {
                                    id: block.id,
                                    call_type: "function".to_string(),
                                    function: crate::protocol::FunctionCall {
                                        name: block.name,
                                        arguments: if block.json.is_empty() {
                                            "{}".to_string()
                                        } else {
                                            block.json
                                        },
                                    },
                                });
                            }
                        }
                        // Only signature-carrying thinking blocks replay;
                        // unsigned ones would corrupt the thread.
                        "thinking" => {
                            if !block.signature.is_empty() {
                                self.reasoning_items.push(json!({
                                    "type": "thinking",
                                    "thinking": block.text,
                                    "signature": block.signature,
                                }));
                            }
                        }
                        "redacted_thinking" => {
                            if let Some(raw) = block.raw {
                                self.reasoning_items.push(raw);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "message_delta" => {
                if let Some(usage) = event.get("usage") {
                    self.absorb_usage(usage);
                    events.push(StreamEvent::Usage(self.current_usage()));
                }
                if let Some(stop) = event
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .and_then(stop_reason_from_anthropic)
                {
                    events.push(StreamEvent::Stop(stop));
                }
            }
            "message_stop" => events.push(StreamEvent::Done),
            // `ping` keep-alives and anything unrecognized are noise.
            _ => {}
        }
        events
    }

    fn finish(self) -> ParsedMessage {
        // Completed calls without an id must be dropped, not executed.
        let mut tool_calls = self.tool_calls;
        tool_calls.retain(|call| !call.id.is_empty() && !call.function.name.is_empty());
        ParsedMessage {
            tool_calls,
            reasoning_items: (!self.reasoning_items.is_empty()).then_some(self.reasoning_items),
            reasoning_content: None,
        }
    }
}
