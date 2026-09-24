use dex_ai::{ChatMessage, LlmToolCall, Role, Turn};

use crate::TokenLedger;

/// Result of applying one normalized model response to the conversation.
#[derive(Debug, Clone)]
pub struct AppliedModelTurn {
    /// Calls to execute before the next model request, when present.
    pub tool_calls: Option<Vec<LlmToolCall>>,
    /// Assistant text for a completed response; empty when tools were called.
    pub response: String,
}

/// Append one model response to history and update its matching token ledger.
///
/// This is the shared state transition used by the dex turn loop after the
/// provider-specific response has been normalized by `dex-ai`.
pub fn apply_model_turn(
    messages: &mut Vec<ChatMessage>,
    ledger: &mut TokenLedger,
    mut turn: Turn,
) -> AppliedModelTurn {
    if let Some(calls) = turn.message.tool_calls.take() {
        messages.push(ChatMessage {
            role: Role::Assistant,
            content: turn.message.content,
            tool_calls: Some(calls.clone()),
            tool_call_id: None,
            name: None,
            reasoning_items: turn.message.reasoning_items,
            reasoning_content: turn.message.reasoning_content,
        });
        ledger.push(messages.last().expect("just pushed"));
        AppliedModelTurn {
            tool_calls: Some(calls),
            response: String::new(),
        }
    } else {
        let response = turn.message.content.unwrap_or_default();
        messages.push(ChatMessage {
            role: Role::Assistant,
            content: Some(response.clone()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            reasoning_items: turn.message.reasoning_items,
            reasoning_content: turn.message.reasoning_content,
        });
        ledger.push(messages.last().expect("just pushed"));
        AppliedModelTurn {
            tool_calls: None,
            response,
        }
    }
}
