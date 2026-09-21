//! Tests, split out of the module body so it stays implementation.

use super::{
    delta_thought, driver_err, is_dropped_connection, is_mid_stream, is_transport_error,
    read_stream, stream_err, stream_idle_timeout_for, SinkLine, SseDriver, StreamDelta,
    StreamPrinter, Usage,
};
use crate::protocol::{StopReason, StreamUsage};
use crate::runtime::console::CancellationToken;
use std::time::Duration;
use tokio::sync::mpsc;

/// The compaction summarizer (and any in-process caller) must be able to
/// pass a sink and have ALL streamed output routed into the channel —
/// never printed raw to stdout, which inside the TUI process is the
/// alternate screen (ghost text until resize). With sink=None the raw
/// print is intentional (plain one-shot CLI streaming); callers running
/// beside a TUI must always pass a sink.
#[tokio::test]
async fn stream_printer_with_sink_routes_lines_to_channel_not_stdout() {
    let (tx, mut rx) = mpsc::channel(32);
    let mut printer = StreamPrinter::new(Some(tx));
    printer
        .feed_line_async("Key facts: internal summary line")
        .await;
    printer.feed_line_async("```rust").await;
    printer.feed_line_async("fn main() {}").await;
    printer.feed_line_async("```").await;
    printer.finish_async().await;

    let lines: Vec<SinkLine> = {
        let mut out = Vec::new();
        while let Ok(v) = rx.try_recv() {
            out.push(v);
        }
        out
    };
    assert!(lines
        .iter()
        .any(|l| matches!(l, SinkLine::Assistant(s) if s.contains("Key facts"))));
    assert!(lines
        .iter()
        .any(|l| matches!(l, SinkLine::Assistant(s) if s.contains("fn main"))));
}

#[test]
fn reasoning_deltas_extract_from_provider_specific_fields() {
    let deepseek: StreamDelta = serde_json::from_str(r#"{"reasoning_content":"step 1"}"#).unwrap();
    assert_eq!(delta_thought(&deepseek), Some("step 1"));
    let openrouter: StreamDelta = serde_json::from_str(r#"{"reasoning":"step 2"}"#).unwrap();
    assert_eq!(delta_thought(&openrouter), Some("step 2"));
    // Non-string shapes must not kill the chunk parse or yield text.
    let array: StreamDelta = serde_json::from_str(r#"{"reasoning":[{"a":1}]}"#).unwrap();
    assert_eq!(delta_thought(&array), None);
    let plain: StreamDelta = serde_json::from_str(r#"{"content":"hi"}"#).unwrap();
    assert_eq!(delta_thought(&plain), None);
}

/// Chat-completions nests cached tokens under `prompt_tokens_details`; a
/// provider that omits the detail object must parse as None, not fail.
#[test]
fn stream_usage_parses_cached_tokens_with_and_without_detail() {
    let usage: StreamUsage = serde_json::from_str(
            r#"{"prompt_tokens":100,"completion_tokens":17,"prompt_tokens_details":{"cached_tokens":42}}"#,
        )
        .unwrap();
    assert_eq!(usage.prompt_tokens, 100);
    assert_eq!(usage.completion_tokens, 17);
    assert_eq!(usage.prompt_details.map(|d| d.cached_tokens), Some(42));

    let usage: StreamUsage = serde_json::from_str(r#"{"prompt_tokens":100}"#).unwrap();
    assert!(usage.prompt_details.is_none());
    // Providers that omit completion_tokens parse as 0, not fail.
    assert_eq!(usage.completion_tokens, 0);
}

// ---- SSE parser tests: in-memory SSE lines fed through the shared
// async driver (`run_sse_lines` → `feed_raw_async`), so all wire parsers
// are exercised end to end without a server. ----

async fn read_chat_lines(
    lines: &[&str],
    sink: Option<tokio::sync::mpsc::Sender<SinkLine>>,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
) -> Result<super::Turn, Box<dyn std::error::Error + Send + Sync>> {
    super::run_sse_lines(lines, sink, cancel, super::ChatCompletionsParser::default()).await
}

async fn read_responses_lines(
    lines: &[&str],
    sink: Option<tokio::sync::mpsc::Sender<SinkLine>>,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
) -> Result<super::Turn, Box<dyn std::error::Error + Send + Sync>> {
    super::run_sse_lines(lines, sink, cancel, super::ResponsesParser::default()).await
}

async fn read_anthropic_lines(
    lines: &[&str],
    sink: Option<tokio::sync::mpsc::Sender<SinkLine>>,
    cancel: &(dyn crate::agent::state::CancellationSource + Send + Sync),
) -> Result<super::Turn, Box<dyn std::error::Error + Send + Sync>> {
    super::run_sse_lines(lines, sink, cancel, super::AnthropicParser::default()).await
}

/// Anthropic Messages: text and tool_use blocks reassemble per index,
/// usage merges message_start (prompt side) with message_delta
/// (cumulative output), and `message_stop` terminates the stream.
#[tokio::test]
async fn anthropic_stream_reassembles_blocks_and_terminates_on_message_stop() {
    let (tx, _rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        r#"data: {"type":"message_start","message":{"usage":{"input_tokens":25,"cache_read_input_tokens":5,"cache_creation_input_tokens":2}}}"#,
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello "}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}"#,
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"th\":\"a.rs\"}"}}"#,
        r#"data: {"type":"content_block_stop","index":1}"#,
        r#"data: {"type":"ping"}"#,
        r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":9}}"#,
        r#"data: {"type":"message_stop"}"#,
        // Server keeps the connection open after message_stop — Done
        // must stop reading before this arrives.
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":99}}"#,
    ];
    let turn = read_anthropic_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(turn.message.content.as_deref(), Some("hello "));
    let calls = turn.message.tool_calls.unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "toolu_1");
    assert_eq!(calls[0].function.name, "read");
    assert_eq!(calls[0].function.arguments, r#"{"path":"a.rs"}"#);
    assert_eq!(turn.stop_reason, Some(StopReason::ToolUse));
    // Prompt side counts cache reads/writes too; output is the
    // message_delta cumulative value, not the trailing one.
    assert_eq!(
        turn.usage,
        Some(Usage {
            prompt_tokens: 32,
            completion_tokens: 9,
            cached_tokens: Some(5)
        })
    );
}

/// Thinking deltas land as Thinking sink lines; signed thinking blocks
/// are captured for replay, unsigned ones are not.
#[tokio::test]
async fn anthropic_stream_captures_replayable_thinking() {
    let (tx, mut rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"step "}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig1"}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"one"}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
        r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":""}}"#,
        r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"unsigned"}}"#,
        r#"data: {"type":"content_block_stop","index":1}"#,
        r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"redacted_thinking","data":"encrypted-blob"}}"#,
        r#"data: {"type":"content_block_stop","index":2}"#,
    ];
    let turn = read_anthropic_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap();
    let items = turn.message.reasoning_items.unwrap();
    assert_eq!(items.len(), 2, "unsigned thinking must not replay");
    assert_eq!(items[0]["type"], "thinking");
    assert_eq!(items[0]["thinking"], "step one");
    assert_eq!(items[0]["signature"], "sig1");
    assert_eq!(items[1]["type"], "redacted_thinking");
    assert_eq!(items[1]["data"], "encrypted-blob");
    let lines: Vec<SinkLine> = {
        let mut out = Vec::new();
        while let Ok(v) = rx.try_recv() {
            out.push(v);
        }
        out
    };
    assert!(matches!(&lines[0], SinkLine::Thinking(s) if s == "step "));
    assert!(matches!(&lines[1], SinkLine::Thinking(s) if s == "one"));
}

/// Anthropic stop reasons map onto the normalized set; unknown reasons
/// stay unset.
#[tokio::test]
async fn anthropic_stream_maps_stop_reasons() {
    for (reason, expected) in [
        ("end_turn", StopReason::Stop),
        ("stop_sequence", StopReason::Stop),
        ("max_tokens", StopReason::Length),
        ("model_context_window", StopReason::Length),
        ("tool_use", StopReason::ToolUse),
        ("refusal", StopReason::ContentFilter),
        ("pause_turn", StopReason::Stop),
    ] {
        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[&format!(
            r#"data: {{"type":"message_delta","delta":{{"stop_reason":"{reason}"}}}}"#
        )];
        let turn = read_anthropic_lines(lines, Some(tx), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(turn.stop_reason, Some(expected), "stop_reason={reason}");
    }
    let (tx, _rx) = mpsc::channel(32);
    let lines: &[&str] = &[r#"data: {"type":"message_delta","delta":{"stop_reason":"junk"}}"#];
    assert_eq!(
        read_anthropic_lines(lines, Some(tx), &CancellationToken::new())
            .await
            .unwrap()
            .stop_reason,
        None
    );
}

/// A terminal `error` event on a 200 body is a real failure: retryable
/// before any output flowed, a mid-stream marker after it.
#[tokio::test]
async fn anthropic_error_event_fails_turn_by_output_flow() {
    let (tx, _rx) = mpsc::channel(32);
    let lines: &[&str] =
        &[r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#];
    let err = read_anthropic_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(err, "overloaded_error: Overloaded");
    assert!(!is_mid_stream(
        &*read_anthropic_lines(lines, None, &CancellationToken::new())
            .await
            .unwrap_err()
    ));

    // After text has streamed, the failure must carry the marker so a
    // retry cannot duplicate the partial transcript.
    let (tx, _rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}"#,
        r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
    ];
    let err = read_anthropic_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(is_mid_stream(&*err));
    assert_eq!(err.to_string(), "overloaded_error: Overloaded");
}

/// Garbage, empty, and keep-alive lines are skipped; a stream with no
/// output yields an empty assistant message (no spurious tool calls).
#[tokio::test]
async fn anthropic_stream_tolerates_garbage() {
    let (tx, _rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        "data: not json at all",
        "data: ",
        r#"data: {"type":"ping"}"#,
        r#"data: {"type":"message_stop"}"#,
    ];
    let turn = read_anthropic_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(turn.message.content, None);
    assert!(turn.message.tool_calls.is_none());
    assert_eq!(turn.usage, None);
    // Ghost tool blocks (no id) are dropped at finish, like responses.
    let (tx, _rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","name":"ghost","input":{}}}"#,
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        r#"data: {"type":"content_block_stop","index":0}"#,
    ];
    let turn = read_anthropic_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap();
    assert!(turn.message.tool_calls.is_none());
}

/// A reasoning output item with encrypted_content is captured onto the
/// message for stateless replay; summary-only items are not.
#[tokio::test]
async fn responses_stream_captures_replayable_reasoning_items() {
    let (tx, _rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"r1","summary":[],"encrypted_content":"blob1"}}"#,
        r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"r2","summary":[{"type":"summary_text","text":"visible"}]}}"#,
        r#"data: {"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"read","arguments":"{}"}}"#,
        "data: [DONE]",
    ];
    let msg = read_responses_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap()
        .message;
    let items = msg.reasoning_items.expect("replayable items captured");
    assert_eq!(items.len(), 1, "summary-only item must be skipped");
    assert_eq!(items[0]["encrypted_content"], "blob1");
    assert!(msg.reasoning_content.is_none());
}

/// Null AND empty-string `encrypted_content` are both unreplayable:
/// an empty blob would corrupt the thread if sent back.
#[tokio::test]
async fn responses_stream_skips_null_and_empty_encrypted_content() {
    let (tx, _rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"r1","summary":[],"encrypted_content":""}}"#,
        r#"data: {"type":"response.output_item.done","output_index":1,"item":{"type":"reasoning","id":"r2","summary":[],"encrypted_content":null}}"#,
        "data: [DONE]",
    ];
    let msg = read_responses_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap()
        .message;
    assert!(
        msg.reasoning_items.is_none(),
        "empty/null blobs must not be captured: {:?}",
        msg.reasoning_items
    );
}

/// DeepSeek-style chat-completions reasoning: `reasoning_content` deltas
/// accumulate onto the message for replay; other reasoning keys are
/// UI-only and must not leak into the replay field.
#[tokio::test]
async fn chat_stream_captures_reasoning_content_for_replay() {
    let (tx, _rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        r#"data: {"choices":[{"delta":{"reasoning_content":"step 1"}}]}"#,
        r#"data: {"choices":[{"delta":{"reasoning":"openrouter-style"}}]}"#,
        r#"data: {"choices":[{"delta":{"reasoning_content":" step 2"}}]}"#,
        r#"data: {"choices":[{"delta":{"content":"done"}}]}"#,
        "data: [DONE]",
    ];
    let msg = read_chat_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap()
        .message;
    assert_eq!(msg.reasoning_content.as_deref(), Some("step 1 step 2"));
    assert!(msg.reasoning_items.is_none());
}

/// Chat-completions stream: content accumulates across chunks and splits
/// into complete sink lines; fragmented tool-call deltas merge into one
/// call; the usage chunk (with cache detail) surfaces as `Some(Usage)`.
#[tokio::test]
async fn chat_stream_assembles_content_tools_and_usage() {
    let (tx, mut rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        r#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#,
        r#"data: {"choices":[{"delta":{"content":"hello "}}]}"#,
        r#"data: {"choices":[{"delta":{"content":"world\n"}}]}"#,
        r#"data: {"choices":[{"delta":{"content":"second line"}}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
        r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.rs\"}"}}]}}]}"#,
        r#"data: {"choices":[],"usage":{"prompt_tokens":123,"completion_tokens":45,"prompt_tokens_details":{"cached_tokens":7}}}"#,
        "data: [DONE]",
    ];
    let turn = read_chat_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap();
    let usage = turn.usage;
    let msg = turn.message;
    assert_eq!(msg.content.as_deref(), Some("hello world\nsecond line"));
    let calls = msg.tool_calls.unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id, "c1");
    assert_eq!(calls[0].function.name, "read");
    assert_eq!(calls[0].function.arguments, r#"{"path":"a.rs"}"#);
    assert_eq!(
        usage,
        Some(Usage {
            prompt_tokens: 123,
            completion_tokens: 45,
            cached_tokens: Some(7)
        })
    );
    let lines: Vec<SinkLine> = {
        let mut out = Vec::new();
        while let Ok(v) = rx.try_recv() {
            out.push(v);
        }
        out
    };
    assert!(matches!(&lines[0], SinkLine::Assistant(s) if s == "hello world"));
    assert!(matches!(&lines[1], SinkLine::Assistant(s) if s == "second line"));
}

/// SSE allows `data:{...}` without the space: a chat-completions
/// endpoint that omits it must still stream (the Responses/Anthropic
/// parsers already accept both spellings).
#[tokio::test]
async fn chat_stream_accepts_data_without_space() {
    let (tx, _rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        r#"data:{"choices":[{"delta":{"content":"hi"}}]}"#,
        "data:[DONE]",
    ];
    let msg = read_chat_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap()
        .message;
    assert_eq!(msg.content.as_deref(), Some("hi"));
}

/// A code fence opened mid-stream is buffered and flushed as one block;
/// prose before/after streams line by line.
#[tokio::test]
async fn chat_stream_buffers_code_fences() {
    let (tx, mut rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        r#"data: {"choices":[{"delta":{"content":"```rust\n"}}]}"#,
        r#"data: {"choices":[{"delta":{"content":"fn main() {}\n"}}]}"#,
        r#"data: {"choices":[{"delta":{"content":"```\n"}}]}"#,
        r#"data: {"choices":[{"delta":{"content":"trailing prose"}}]}"#,
        "data: [DONE]",
    ];
    let msg = read_chat_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap()
        .message;
    assert_eq!(
        msg.content.as_deref(),
        Some("```rust\nfn main() {}\n```\ntrailing prose")
    );
    let lines: Vec<SinkLine> = {
        let mut out = Vec::new();
        while let Ok(v) = rx.try_recv() {
            out.push(v);
        }
        out
    };
    // The fence body keeps its streamed trailing newline and the close
    // adds one more — current sink contract; consumers re-parse the block.
    assert!(
        matches!(&lines[0], SinkLine::Assistant(s)
              if s == "```rust:\nfn main() {}\n\n```"),
        "{lines:?}"
    );
    assert!(matches!(&lines[1], SinkLine::Assistant(s) if s == "trailing prose"));
}

/// `finish_reason` on the final chunk maps onto the normalized
/// [`StopReason`] (length truncation, tool-call handoff, clean stop).
#[tokio::test]
async fn chat_stream_maps_finish_reason_to_stop_reason() {
    for (finish, expected) in [
        ("stop", StopReason::Stop),
        ("length", StopReason::Length),
        ("tool_calls", StopReason::ToolUse),
        ("function_call", StopReason::ToolUse),
        ("content_filter", StopReason::ContentFilter),
    ] {
        let (tx, _rx) = mpsc::channel(32);
        let lines: &[&str] = &[
            r#"data: {"choices":[{"delta":{"content":"hi"}}]}"#,
            &format!(r#"data: {{"choices":[{{"delta":{{}},"finish_reason":"{finish}"}}]}}"#),
            "data: [DONE]",
        ];
        let turn = read_chat_lines(lines, Some(tx), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(turn.stop_reason, Some(expected), "finish_reason={finish}");
        assert_eq!(turn.message.content.as_deref(), Some("hi"));
    }
    // Unrecognized reasons stay unset instead of being guessed.
    let (tx, _rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        r#"data: {"choices":[{"delta":{},"finish_reason":"junk"}]}"#,
        "data: [DONE]",
    ];
    let turn = read_chat_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(turn.stop_reason, None);
}

/// A pre-cancelled token unwinds before reading anything.
#[tokio::test]
async fn chat_stream_returns_cancelled_error_when_token_set() {
    let token = CancellationToken::new();
    token.cancel();
    let (tx, _rx) = mpsc::channel(32);
    let lines: &[&str] = &[r#"data: {"choices":[{"delta":{"content":"x"}}]}"#];
    let err = read_chat_lines(lines, Some(tx), &token).await.unwrap_err();
    assert_eq!(err.to_string(), "cancelled");
}

/// Responses API: the function-call state machine must survive deltas
/// that arrive BEFORE their item is added (buffered then flushed), and
/// completed calls without an id must be dropped, not executed.
#[tokio::test]
async fn responses_stream_reassembles_tool_calls_with_early_deltas() {
    let (tx, _rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        r#"data: {"type":"response.output_text.delta","delta":"hello\n"}"#,
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"item_1","call_id":"call_1","name":"read","arguments":""}}"#,
        r#"data: {"type":"response.function_call_arguments.delta","item_id":"item_2","delta":"EARLY"}"#,
        r#"data: {"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"item_2","call_id":"call_2","name":"bash","arguments":""}}"#,
        r#"data: {"type":"response.function_call_arguments.delta","item_id":"item_2","delta":"LATER"}"#,
        r#"data: {"type":"response.output_item.added","output_index":3,"item":{"type":"function_call","name":"ghost"}}"#,
        r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":50,"output_tokens":9,"input_tokens_details":{"cached_tokens":5}}}}"#,
        "data: [DONE]",
    ];
    let turn = read_responses_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap();
    let usage = turn.usage;
    let msg = turn.message;
    assert_eq!(msg.content.as_deref(), Some("hello\n"));
    let calls = msg.tool_calls.unwrap();
    assert_eq!(
        calls.len(),
        2,
        "ghost call (no id) and padding must be filtered: {calls:?}"
    );
    assert_eq!(calls[0].id, "call_1");
    assert_eq!(calls[0].function.name, "read");
    assert_eq!(calls[1].id, "call_2");
    assert_eq!(calls[1].function.arguments, "EARLYLATER");
    assert_eq!(
        usage,
        Some(Usage {
            prompt_tokens: 50,
            completion_tokens: 9,
            cached_tokens: Some(5)
        })
    );
}

/// Garbage, empty, and keep-alive data lines are skipped; reasoning
/// deltas (both provider keys) land as Thinking sink lines; a stream
/// with no output yields an empty assistant message.
#[tokio::test]
async fn responses_stream_tolerates_garbage_and_extracts_reasoning() {
    let (tx, mut rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        "data: not json at all",
        "data: ",
        r#"data: {"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#,
        r#"data: {"type":"response.reasoning_text.delta","delta":" more"}"#,
        "data: [DONE]",
    ];
    let turn = read_responses_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(turn.message.content, None);
    assert!(turn.message.tool_calls.is_none());
    assert_eq!(turn.usage, None);
    let lines: Vec<SinkLine> = {
        let mut out = Vec::new();
        while let Ok(v) = rx.try_recv() {
            out.push(v);
        }
        out
    };
    assert!(matches!(&lines[0], SinkLine::Thinking(s) if s == "thinking"));
    assert!(matches!(&lines[1], SinkLine::Thinking(s) if s == " more"));
}

/// `response.completed` normalizes to a clean stop; `response.incomplete`
/// normalizes by reason (max_output_tokens → Length, content_filter →
/// ContentFilter); an unrecognized reason stays unset.
#[tokio::test]
async fn responses_stream_maps_terminal_events_to_stop_reasons() {
    let (tx, _rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        r#"data: {"type":"response.output_text.delta","delta":"partial"}"#,
        r#"data: {"type":"response.incomplete","response":{"usage":{"input_tokens":10,"output_tokens":99},"incomplete_details":{"reason":"max_output_tokens"}}}"#,
        "data: [DONE]",
    ];
    let turn = read_responses_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(turn.stop_reason, Some(StopReason::Length));
    assert_eq!(
        turn.usage,
        Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 99,
            cached_tokens: None
        })
    );

    let (tx, _rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":4}}}"#,
        "data: [DONE]",
    ];
    let turn = read_responses_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(turn.stop_reason, Some(StopReason::Stop));

    // Incomplete for a content filter is a partial reply too.
    let (tx, _rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        r#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}"#,
        "data: [DONE]",
    ];
    let turn = read_responses_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(turn.stop_reason, Some(StopReason::ContentFilter));

    // An unrecognized incomplete reason must not claim a clean stop.
    let (tx, _rx) = mpsc::channel(32);
    let lines: &[&str] = &[
        r#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"junk"}}}"#,
        "data: [DONE]",
    ];
    let turn = read_responses_lines(lines, Some(tx), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(turn.stop_reason, None);
}

/// The mid-stream marker applies only once output has flowed; before
/// that a failure stays retryable. Display passes the message through so
/// `"cancelled"` matching keeps working.
#[test]
fn stream_error_marking_follows_output_flow() {
    assert!(!is_mid_stream(&*stream_err("boom", false)));
    assert!(is_mid_stream(&*stream_err("boom", true)));
    assert_eq!(stream_err("cancelled", true).to_string(), "cancelled");
}

/// A mid-stream failure on a headless run must close the open thinking
/// line (the terminal would otherwise stay dimmed); double-close is a
/// no-op, and retryability still follows output flow.
#[test]
fn driver_err_closes_open_thinking_line() {
    let mut driver = SseDriver::new(None);
    driver.thinking_open = true;
    driver.output_flowed = true;
    let err = driver_err(&mut driver, "boom");
    assert!(!driver.thinking_open);
    assert!(is_mid_stream(&*err));
    // Idempotent: ending an already-closed line writes nothing.
    driver.end_thinking();
    assert!(!driver.thinking_open);
    // Before any output, the same failure stays retryable.
    let mut fresh = SseDriver::new(None);
    fresh.thinking_open = true;
    assert!(!is_mid_stream(&*driver_err(&mut fresh, "boom")));
}

/// The async SSE driver must yield complete lines to the parser even
/// when a provider splits one SSE line across `response.chunk()`
/// boundaries, and must flush a final unterminated line at EOF (how the
/// Responses API ends its body). The in-memory `run_sse_lines` tests above
/// never exercise this: an in-memory body arrives as one giant chunk.
/// A localhost server writes the body in three deliberately straddled
/// writes; gaps between them keep each write a separate chunk.
#[tokio::test]
async fn run_sse_splits_lines_straddling_chunk_boundaries() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut sock, _) = listener.accept().await.unwrap();
        sock.set_nodelay(true).unwrap();
        // Drain the request head before responding.
        let mut buf = [0u8; 4096];
        let mut seen = Vec::new();
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&buf[..n]);
            if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let c1 = r#"data: {"type":"response.output_text.delta","delta":"hel"#;
        let c2 = r#"lo"}
data: {"type":"response.output_text.delta","delta":" wor"#;
        // Final line: no trailing newline — flushed at EOF.
        let c3 = r#"ld"}
data: {"type":"response.output_text.delta","delta":"!"}"#;
        let mut body = String::new();
        body.push_str(c1);
        body.push_str(c2);
        body.push_str(c3);
        sock.write_all(
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        for chunk in [c1, c2, c3] {
            sock.write_all(chunk.as_bytes()).await.unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });
    let response = crate::runtime::http::shared_streaming_client()
        .get(format!("http://{addr}/v1/responses"))
        .send()
        .await
        .unwrap();
    let turn = super::read_responses_stream(
        response,
        None,
        &CancellationToken::new(),
        Some(Duration::from_secs(30)),
    )
    .await
    .unwrap();
    server.await.unwrap();
    assert_eq!(turn.message.content.as_deref(), Some("hello world!"));
}

#[test]
fn idle_timeout_env_parses() {
    let _env = crate::session::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("DEX_STREAM_IDLE_TIMEOUT_SECS").ok();
    std::env::set_var("DEX_STREAM_IDLE_TIMEOUT_SECS", "7");
    assert_eq!(
        stream_idle_timeout_for("dex-test-no-such-model", false),
        Some(Duration::from_secs(7))
    );
    // 0 disables the watchdog entirely (no timer armed, not an
    // immediate timeout and not an infinite deadline).
    std::env::set_var("DEX_STREAM_IDLE_TIMEOUT_SECS", "0");
    assert_eq!(
        stream_idle_timeout_for("dex-test-no-such-model", false),
        None
    );
    // Garbage falls back to the default.
    std::env::set_var("DEX_STREAM_IDLE_TIMEOUT_SECS", "junk");
    assert_eq!(
        stream_idle_timeout_for("dex-test-no-such-model", false),
        Some(Duration::from_secs(super::DEFAULT_STREAM_IDLE_TIMEOUT_SECS))
    );
    match prev {
        Some(v) => std::env::set_var("DEX_STREAM_IDLE_TIMEOUT_SECS", v),
        None => std::env::remove_var("DEX_STREAM_IDLE_TIMEOUT_SECS"),
    }
}

#[test]
fn idle_timeout_default_is_per_model() {
    // No env override (lock held so a parallel env-mutating test can't
    // leak in); an unknown model is never reasoning-capable, so the
    // default must not depend on whatever catalog cache the machine has.
    let _env = crate::session::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("DEX_STREAM_IDLE_TIMEOUT_SECS").ok();
    std::env::remove_var("DEX_STREAM_IDLE_TIMEOUT_SECS");
    assert_eq!(
        stream_idle_timeout_for("dex-test-no-such-model", false),
        Some(Duration::from_secs(super::DEFAULT_STREAM_IDLE_TIMEOUT_SECS))
    );
    // Thinking enabled for the call earns the patient budget without any
    // catalog entry.
    assert_eq!(
        stream_idle_timeout_for("dex-test-no-such-model", true),
        Some(Duration::from_secs(
            super::REASONING_STREAM_IDLE_TIMEOUT_SECS
        ))
    );
    match prev {
        Some(v) => std::env::set_var("DEX_STREAM_IDLE_TIMEOUT_SECS", v),
        None => std::env::remove_var("DEX_STREAM_IDLE_TIMEOUT_SECS"),
    }
}

#[test]
fn dropped_connection_matching() {
    assert!(is_dropped_connection(
        "error sending request: connection closed before message completed"
    ));
    assert!(is_dropped_connection(
        "connection reset by peer (os error 104)"
    ));
    assert!(!is_dropped_connection("API error: boom"));
    assert!(!is_dropped_connection(
        "stream idle for over 90s; the provider stalled"
    ));
}

/// A server that sends headers then drops the socket must surface a
/// marked transport error pre-output (not a bare string): the retry gate
/// matches on the `StreamTransportError` marker, so new transport
/// wordings and `reqwest::Error` kinds need no matcher updates.
#[tokio::test]
async fn pre_output_drop_preserves_typed_transport_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut sock, _) = listener.accept().await.unwrap();
        // Drain the request head, advertise a body, then die: the client
        // sees a mid-body EOF on its first `chunk()`.
        let mut buf = [0u8; 4096];
        let mut seen = Vec::new();
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            if n == 0 {
                return;
            }
            seen.extend_from_slice(&buf[..n]);
            if seen.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let _ = sock
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 100\r\n\r\n",
                )
                .await;
        // Socket drops here with the body unsent.
    });
    let response = crate::runtime::http::shared_streaming_client()
        .get(format!("http://{addr}/v1/chat/completions"))
        .send()
        .await
        .unwrap();
    let err = read_stream(
        response,
        None,
        &CancellationToken::new(),
        Some(Duration::from_secs(30)),
    )
    .await
    .unwrap_err();
    // Provenance, not wording or kind: the same mid-body FIN reads as
    // `is_decode` on this reqwest version (`is_body` on others), so the
    // gate matches the marker `run_sse` attached, not the taxonomy.
    assert!(is_transport_error(&*err));
    assert!(!is_mid_stream(&*err));
    // The marker preserves the transport cause for logs and notices.
    let mut source = std::error::Error::source(&*err);
    let mut found_cause = false;
    while let Some(e) = source {
        if e.downcast_ref::<reqwest::Error>().is_some() {
            found_cause = true;
            break;
        }
        source = std::error::Error::source(e);
    }
    assert!(found_cause, "marker keeps the reqwest cause in-chain");
}

/// A provider that sends response headers and then never sends a byte
/// must not park the turn forever: the idle watchdog fails the stream
/// with a diagnosable error instead of an opaque hang.
#[tokio::test]
async fn stalled_stream_fails_after_idle_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut socket = socket;
        // Valid headers with a body that never arrives: chunk() pends.
        use tokio::io::AsyncWriteExt as _;
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n")
            .await;
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let client = crate::runtime::http::shared_streaming_client();
    let response = client
        .get(format!("http://{addr}/v1"))
        .send()
        .await
        .unwrap();
    let result = read_stream(
        response,
        None,
        &CancellationToken::new(),
        // Explicit budget, not the env var: this test holds no env lock
        // (a guard can't span `.await`) and must not race env-mutating
        // tests. Env parsing is covered by `idle_timeout_env_parses`.
        Some(Duration::from_secs(1)),
    )
    .await;
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("stream idle"),
        "watchdog must name the stall: {err}"
    );
}
