use serde_json::Value;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::agent::compaction::{compact_history, effective_tokens, KEEP_RECENT_MESSAGES};
use crate::agent::state::{cache_fingerprint, wait_cancelled, CancellationSource, ToolState};
use crate::core::console::{
    with_console, Console, SpinnerGuard, RESET, TOOL_INPUT_COLOR, TOOL_MUTATION_LOCK,
    TOOL_OUTPUT_COLOR,
};
use crate::core::format::{
    model_tool_result, short_arg, terminal_preview, tool_preview, tool_preview_body,
    tool_result_summary,
};
use crate::core::types::{ChatMessage, LlmToolCall, Role, SinkLine, StopReason, Usage};
use crate::llm::client::ModelClient;
use crate::llm::config::LlmConfig;
use crate::llm::stream::Turn;
use crate::session::Session;
use crate::tools::{execute_outcome, ToolOutcome};

pub(crate) fn tool_calls_conflict(calls: &[LlmToolCall]) -> bool {
    let mut paths = std::collections::HashSet::new();
    calls.iter().any(|call| {
        let Ok(value) = serde_json::from_str::<Value>(&call.function.arguments) else {
            return false;
        };
        let Some(path) = value.get("path").and_then(Value::as_str) else {
            return false;
        };
        !paths.insert(path.to_string())
    })
}

pub(crate) fn persist_pending(
    session: &mut Option<&mut Session>,
    messages: &[ChatMessage],
    cursor: &mut usize,
) -> std::io::Result<()> {
    if let Some(session) = session.as_deref_mut() {
        for message in messages.get(*cursor..).unwrap_or_default() {
            session.append_message(message.clone())?;
        }
        *cursor = messages.len();
    }
    Ok(())
}

/// Shared per-call accounting: live context usage in `state`, the sink
/// event the TUI accumulates spend from (carrying the daemon-priced USD
/// cost, so the remote client never re-prices locally), and the
/// session-cumulative USD cost. Also used for compaction summarizer calls,
/// which are billed too.
async fn record_usage(config: &LlmConfig, state: &mut ToolState, console: &Console, u: Usage) {
    state.last_usage = Some(u.prompt_tokens);
    state.last_cached = u.cached_tokens;
    let cost =
        crate::llm::config::usage_cost(&config.model, &config.provider, &config.base_url, &u)
            .unwrap_or_else(|| {
                #[allow(clippy::cast_precision_loss)]
                let rate_per_1k = std::env::var("DEX_COST_PER_1K")
                    .ok()
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.002);
                #[allow(clippy::cast_precision_loss)]
                {
                    (u.prompt_tokens + u.completion_tokens) as f64 * rate_per_1k / 1000.0
                }
            });
    console
        .emit_async(SinkLine::Usage {
            tokens: u.prompt_tokens,
            cached: u.cached_tokens,
            cost,
            output: u.completion_tokens,
        })
        .await;
    state.total_cost += cost;
}

async fn execute_tool_call(
    call: &LlmToolCall,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> (String, String, ToolOutcome) {
    let name = call.function.name.clone();
    let raw_args = call.function.arguments.clone();
    let value: Value = match serde_json::from_str(&raw_args) {
        Ok(value) => value,
        Err(error) => {
            return (
                name,
                raw_args,
                ToolOutcome {
                    text: format!("Error: invalid tool arguments: {}", error),
                    ok: false,
                    diff: None,
                },
            )
        }
    };
    let Some(args) = value.as_object().cloned() else {
        return (
            name,
            raw_args,
            ToolOutcome {
                text: "Error: tool arguments must be a JSON object".into(),
                ok: false,
                diff: None,
            },
        );
    };
    let input = serde_json::to_string(&args).unwrap_or_default();
    let outcome = execute_outcome(&name, &args, cancel).await;
    (name, input, outcome)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn process_turn(
    config: &LlmConfig,
    messages: &mut Vec<ChatMessage>,
    state: &mut ToolState,
    mut steering_rx: Option<&mut mpsc::Receiver<String>>,
    steering_accepted_tx: Option<&mpsc::Sender<String>>,
    mut session: Option<&mut Session>,
    client: &(impl ModelClient + 'static),
    cancel: &(impl CancellationSource + Clone + 'static),
    console: &Console,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let _working = SpinnerGuard::start(console, "Working");
    let mut last_tools: Vec<String> = Vec::new();
    let mut last_usage: Option<u64> = state.last_usage;
    let cancellation = cancel;
    let mut persisted_cursor = messages.len();

    for _iteration in 0..1_000_000 {
        persist_pending(&mut session, messages, &mut persisted_cursor)?;
        if cancellation.is_cancelled() {
            let _ = cancellation.take_cancelled();
            return Err("cancelled by user".into());
        }
        if let Some(rx) = steering_rx.as_mut() {
            let mut drained: Vec<String> = Vec::new();
            while let Ok(steering) = rx.try_recv() {
                drained.push(steering);
            }
            for steering in drained {
                if let Some(accepted) = &steering_accepted_tx {
                    let _ = accepted.send(steering.clone()).await;
                }
                messages.push(ChatMessage::user_named(steering, "steering"));
                persist_pending(&mut session, messages, &mut persisted_cursor)?;
            }
        }

        // Proactive compaction BEFORE model call
        // Ephemeral MCP status line: priced in the budget below but never
        // stored in `messages` (call-time preamble, not transcript).
        // Sync snapshot, never initializes the manager: no MCP tools are
        // in the schema before bootstrap either, so the budget stays exact
        // and a budget probe never spawns the background refresh.
        let ephemerals = [crate::mcp::ephemeral_line()];
        let mut compaction_attempts = 0;
        while compaction_attempts < 3 {
            let eff = effective_tokens(messages, &ephemerals, true);
            let need_by_tokens = eff > config.compaction_threshold();
            let need_by_count = messages.len() > 1 + KEEP_RECENT_MESSAGES;
            if !need_by_tokens && !need_by_count {
                break;
            }
            match compact_history(config, messages, cancel).await {
                Ok((true, compacted)) => {
                    compaction_attempts += 1;
                    // Summarizer calls are billed like any other; account
                    // them so the status-bar spend includes compaction.
                    if let Some(u) = compacted {
                        record_usage(config, state, console, u).await;
                    }
                    if let Some(session) = session.as_deref_mut() {
                        session.clear_messages()?;
                        for message in messages.iter().skip(1) {
                            session.append_message(message.clone())?;
                        }
                    }
                    persisted_cursor = messages.len();
                    continue;
                }
                Ok((false, _)) => break,
                Err(e) if e.contains("cancelled") => return Err(e.into()),
                Err(e) => return Err(e.into()),
            }
        }

        // Async LLM call with prompt cancel: `select!(cancelled, complete)`
        // wakes within ~10ms. No message `to_vec` clone beyond what the call
        // needs and no parked thread (S6 resource win).
        let cancel_ref: &(dyn CancellationSource + Send + Sync) = cancel;
        let turn: Turn = tokio::select! {
            _ = wait_cancelled(cancel_ref) => {
                return Err("cancelled by user".into());
            }
            r = client.complete(messages, true, console.sink().cloned(), cancel_ref) => match r {
                Ok(result) => result,
                Err(e) => {
                    let msg = e.to_string();
                    if msg == "interrupted" || msg == "cancelled" {
                        return Err("cancelled by user".into());
                    }
                    return Err(msg.into());
                }
            },
        };
        if let Some(u) = turn.usage {
            last_usage = Some(u.prompt_tokens);
            record_usage(config, state, console, u).await;
        }
        // The provider cut the reply off mid-generation (output-token limit
        // or a content filter): whatever landed is likely incomplete. Say so
        // instead of silently keeping a truncated reply as if it were complete.
        match console.sink() {
            Some(_) => {
                match turn.stop_reason {
                    Some(StopReason::Length) => {
                        console
                            .emit_async(SinkLine::System(
                                "model output hit the output-token limit and may be truncated"
                                    .to_string(),
                            ))
                            .await;
                    }
                    Some(StopReason::ContentFilter) => {
                        console.emit_async(SinkLine::System(
                          "model output was cut off by a content filter and may be incomplete"
                              .to_string(),
                      )).await;
                    }
                    _ => {}
                }
            }
            None => with_console(false, || match turn.stop_reason {
                Some(StopReason::Length) => {
                    eprintln!("[dex] model output hit the output-token limit and may be truncated")
                }
                Some(StopReason::ContentFilter) => {
                    eprintln!(
                        "[dex] model output was cut off by a content filter and may be incomplete"
                    )
                }
                _ => {}
            }),
        }
        let message = turn.message;

        if let Some(calls) = message.tool_calls.clone() {
            messages.push(ChatMessage {
                role: Role::Assistant,
                content: message.content,
                tool_calls: Some(calls.clone()),
                tool_call_id: None,
                name: None,
                reasoning_items: message.reasoning_items.clone(),
                reasoning_content: message.reasoning_content.clone(),
            });

            let serialize_batch = tool_calls_conflict(&calls);
            let results: Vec<_> = if serialize_batch {
                let _guard = TOOL_MUTATION_LOCK.lock().await;
                let mut out = Vec::new();
                for call in &calls {
                    let started = Instant::now();
                    let (name, input, outcome) =
                        execute_tool_call(call, cancel as &(dyn CancellationSource + Send + Sync))
                            .await;
                    out.push((name, input, outcome, started.elapsed()));
                }
                out
            } else {
                // JoinSet tasks (async tools): N threads → N tasks, input-ordered
                // via indexed results + sort (S3). Panics propagate as tool errors.
                let mut set = tokio::task::JoinSet::new();
                for (idx, call) in calls.iter().enumerate() {
                    let call = call.clone();
                    let cancel = cancel.clone();
                    set.spawn(async move {
                        let started = Instant::now();
                        let (name, input, outcome) = execute_tool_call(&call, &cancel).await;
                        (idx, name, input, outcome, started.elapsed())
                    });
                }
                let mut indexed: Vec<(usize, String, String, ToolOutcome, Duration)> = Vec::new();
                while let Some(joined) = set.join_next().await {
                    match joined {
                        Ok(v) => indexed.push(v),
                        Err(_) => indexed.push((
                            usize::MAX,
                            String::new(),
                            String::new(),
                            ToolOutcome {
                                text: "Error: tool worker panicked".into(),
                                ok: false,
                                diff: None,
                            },
                            Duration::ZERO,
                        )),
                    }
                }
                indexed.sort_by_key(|(idx, _, _, _, _)| *idx);
                indexed
                    .into_iter()
                    .map(|(_, n, i, o, d)| (n, i, o, d))
                    .collect::<Vec<(String, String, ToolOutcome, Duration)>>()
            };

            // Cancel landed during tool IO: the per-tool "cancelled"
            // errors above are shutdown noise, not model input. Suppress
            // the fan-out and unwind — the turn was going to abort at the
            // next loop-top check anyway, and skipping the persist keeps
            // a transcript the model never saw out of the session.
            if cancellation.is_cancelled() {
                let _ = cancellation.take_cancelled();
                return Err("cancelled by user".into());
            }

            // Hoisted: one getcwd per iteration, not per tool result.
            let turn_cwd = std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            for (call, (name, input, outcome, elapsed)) in calls.iter().zip(results) {
                let cache_key = format!(
                    "{}:{}:{}{}",
                    turn_cwd,
                    name,
                    input,
                    cache_fingerprint(&name, &input)
                );
                let succeeded = outcome.ok;
                // Captured before `outcome.text` is moved below: the
                // pre-mutation unified diff for write/edit results.
                let diff = outcome.diff.clone();
                if succeeded {
                    if last_tools.len() >= 6 {
                        last_tools.remove(0);
                    }
                    last_tools.push(cache_key.clone());
                }
                let repeated_count = last_tools.iter().filter(|k| **k == cache_key).count();
                if console.sink().is_some() {
                    console
                        .emit_async(SinkLine::ToolInput(format!(
                            "{} {}",
                            call.function.name,
                            short_arg(&name, &input)
                        )))
                        .await;
                } else {
                    with_console(console.sink().is_some(), || {
                        eprintln!(
                            "{}[tool input] {} {}{}",
                            TOOL_INPUT_COLOR,
                            call.function.name,
                            terminal_preview(&input),
                            RESET
                        );
                    });
                }

                let cacheable = matches!(
                    name.as_str(),
                    "read" | "grep" | "ffgrep" | "find" | "fffind" | "ls"
                );
                let mut cache_hit = false;
                let mut ok = succeeded;
                let result = if repeated_count >= 3 {
                    ok = false;
                    "Error: repeated identical tool call; choose a different action or finish."
                        .to_string()
                } else if cacheable && succeeded {
                    if let Some(cached) = state.cache.get(&cache_key) {
                        cache_hit = true;
                        cached.clone()
                    } else {
                        state.insert(cache_key.clone(), outcome.text.clone());
                        outcome.text
                    }
                } else {
                    if matches!(name.as_str(), "write" | "edit") {
                        state.clear();
                    }
                    outcome.text
                };
                if console.sink().is_some() {
                    let mut summary = tool_result_summary(&name, &input, &result, ok);
                    if cache_hit {
                        summary = format!("cached · {summary}");
                    }
                    let counts_only = matches!(
                        name.as_str(),
                        "read" | "grep" | "ffgrep" | "find" | "fffind" | "ls" | "chain"
                    );
                    let skip_first = !counts_only || !ok;
                    let preview = tool_preview(&name, ok, diff.as_deref(), &result, skip_first);
                    console
                        .emit_async(SinkLine::ToolOutput {
                            name: name.clone(),
                            summary,
                            success: ok,
                            preview,
                            duration: elapsed.as_secs_f64(),
                        })
                        .await;
                } else {
                    with_console(console.sink().is_some(), || {
                        let body = tool_preview_body(&name, ok, diff.as_deref(), &result);
                        eprintln!(
                            "{}[tool output] {}:\n{}{}",
                            TOOL_OUTPUT_COLOR, name, body, RESET
                        );
                    });
                }
                messages.push(ChatMessage::tool_result(
                    call.id.clone(),
                    model_tool_result(&result),
                ));
                persist_pending(&mut session, messages, &mut persisted_cursor)?;
            }
            // Write-through persist (best-effort, tiny JSON): awaited so a
            // process exit right after the turn can't lose it — a detached
            // spawn would be dropped on shutdown before it ever ran.
            if state.dirty {
                let to_save = ToolState {
                    cache: state.cache.clone(),
                    dirty: true,
                    last_usage: state.last_usage,
                    last_cached: state.last_cached,
                    total_usage: state.total_usage,
                    total_output: state.total_output,
                    total_cost: state.total_cost,
                    verify_dirty: state.verify_dirty,
                };
                to_save.save_async().await;
                state.dirty = false;
            }
        } else {
            let text = message.content.unwrap_or_default();
            messages.push(ChatMessage {
                role: Role::Assistant,
                content: Some(text.clone()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
                reasoning_items: message.reasoning_items.clone(),
                reasoning_content: message.reasoning_content.clone(),
            });
            if let Some(rx) = steering_rx.as_mut() {
                let mut steering: Vec<String> = Vec::new();
                while let Ok(s) = rx.try_recv() {
                    steering.push(s);
                }
                if !steering.is_empty() {
                    for content in steering {
                        if let Some(accepted) = &steering_accepted_tx {
                            let _ = accepted.send(content.clone()).await;
                        }
                        messages.push(ChatMessage::user_named(content, "steering"));
                    }
                    state.last_usage = last_usage;
                    continue;
                }
            }
            state.last_usage = last_usage;
            persist_pending(&mut session, messages, &mut persisted_cursor)?;
            return Ok(text);
        }
    }
    Err("turn did not complete after many tool iterations; partial progress preserved.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::state::{CancellationSource, ToolState};
    use crate::core::types::{ApiProtocol, ChatMessage, PermissionMode, Provider};
    use crate::llm::client::ModelClient;

    #[derive(Clone)]
    struct MockModel;

    impl ModelClient for MockModel {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _with_tools: bool,
            _sink: Option<mpsc::Sender<SinkLine>>,
            _cancel: &(dyn CancellationSource + Send + Sync),
        ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
            Ok(Turn {
                message: ChatMessage::assistant("hello from mock"),
                usage: Some(Usage {
                    prompt_tokens: 1,
                    completion_tokens: 0,
                    cached_tokens: None,
                }),
                stop_reason: None,
            })
        }
    }

    #[derive(Clone)]
    struct NeverCancel;

    impl CancellationSource for NeverCancel {
        fn is_cancelled(&self) -> bool {
            false
        }
        fn take_cancelled(&self) -> bool {
            false
        }
    }

    fn test_config() -> LlmConfig {
        LlmConfig {
            provider: Provider::OpenCode,
            api_key: String::new(),
            base_url: String::new(),
            model: "mock".into(),
            available_models: vec!["mock".into()],
            endpoints: Default::default(),
            api: ApiProtocol::Responses,
            account_id: None,
            thinking_effort: None,
            context_window: 128_000,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
            permission: PermissionMode::Trusted,
            verify_command: None,
            extra_headers: Default::default(),
            client: reqwest::Client::new(),
            provider_entries: Default::default(),
            provider_headers: Default::default(),
            api_pinned: false,
        }
    }

    #[tokio::test]
    async fn process_turn_completes_with_injected_client() {
        let config = test_config();
        let mut messages = vec![ChatMessage::system("sys")];
        let mut state = ToolState::default();
        let result = process_turn(
            &config,
            &mut messages,
            &mut state,
            None,
            None,
            None,
            &MockModel,
            &NeverCancel,
            &crate::core::console::Console::none(),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "hello from mock");
        assert!(messages
            .iter()
            .any(|m| m.content.as_deref() == Some("hello from mock")));
    }

    #[derive(Clone)]
    struct ToolThenAnswer {
        round: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ToolThenAnswer {
        fn new() -> Self {
            Self {
                round: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    impl ModelClient for ToolThenAnswer {
        async fn complete(
            &self,
            _messages: &[ChatMessage],
            _with_tools: bool,
            _sink: Option<mpsc::Sender<SinkLine>>,
            _cancel: &(dyn CancellationSource + Send + Sync),
        ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
            let round = self.round.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let message = if round == 0 {
                ChatMessage::assistant_calls(
                    None,
                    vec![crate::core::types::LlmToolCall {
                        id: "call-1".into(),
                        call_type: "function".into(),
                        function: crate::core::types::FunctionCall {
                            name: "bash".into(),
                            arguments: r#"{"command":"echo line-one; echo line-two; echo line-three; echo line-four"}"#.into(),
                        },
                    }],
                )
            } else {
                ChatMessage::assistant("done")
            };
            Ok(Turn {
                message,
                usage: Some(Usage {
                    prompt_tokens: 1,
                    completion_tokens: 0,
                    cached_tokens: None,
                }),
                stop_reason: None,
            })
        }
    }

    #[tokio::test]
    async fn tool_result_streams_summary_preview_and_success() {
        let config = test_config();
        let mut messages = vec![ChatMessage::system("sys")];
        let mut state = ToolState::default();
        let (sink_tx, mut sink_rx) = mpsc::channel(32);
        let (approval_tx, _approval_rx) = mpsc::channel(16);
        let _ = process_turn(
            &config,
            &mut messages,
            &mut state,
            None,
            None,
            None,
            &ToolThenAnswer::new(),
            &NeverCancel,
            &crate::core::console::Console::daemon(sink_tx, approval_tx),
        )
        .await;
        let mut events = Vec::new();
        while let Ok(e) = sink_rx.try_recv() {
            events.push(e);
        }
        assert!(events.iter().any(|e| matches!(e, SinkLine::ToolInput(_))));
        assert!(events
            .iter()
            .any(|e| matches!(e, SinkLine::ToolOutput { .. })));
    }

    #[tokio::test]
    async fn conflict_serialize_still_serializes_same_path_edits() {
        // TDD Phase 2: same-path edits must take the serialize path.
        let calls = vec![
            crate::core::types::LlmToolCall {
                id: "a".into(),
                call_type: "function".into(),
                function: crate::core::types::FunctionCall {
                    name: "edit".into(),
                    arguments: r#"{"path":"same.rs"}"#.into(),
                },
            },
            crate::core::types::LlmToolCall {
                id: "b".into(),
                call_type: "function".into(),
                function: crate::core::types::FunctionCall {
                    name: "edit".into(),
                    arguments: r#"{"path":"same.rs"}"#.into(),
                },
            },
        ];
        assert!(tool_calls_conflict(&calls));
        let different = vec![
            crate::core::types::LlmToolCall {
                id: "a".into(),
                call_type: "function".into(),
                function: crate::core::types::FunctionCall {
                    name: "read".into(),
                    arguments: r#"{"path":"a.rs"}"#.into(),
                },
            },
            crate::core::types::LlmToolCall {
                id: "b".into(),
                call_type: "function".into(),
                function: crate::core::types::FunctionCall {
                    name: "read".into(),
                    arguments: r#"{"path":"b.rs"}"#.into(),
                },
            },
        ];
        assert!(!tool_calls_conflict(&different));
    }

    #[tokio::test]
    async fn cancel_during_llm_call_unwinds_promptly() {
        // TDD Phase 2: select!(cancelled, complete) — no 50ms poll quantum.
        #[derive(Clone)]
        struct Hanging;
        impl ModelClient for Hanging {
            async fn complete(
                &self,
                _m: &[ChatMessage],
                _w: bool,
                _s: Option<mpsc::Sender<SinkLine>>,
                _c: &(dyn CancellationSource + Send + Sync),
            ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                unreachable!()
            }
        }
        let config = test_config();
        let mut messages = vec![ChatMessage::system("sys")];
        let mut state = ToolState::default();
        let cancel = crate::core::console::CancellationToken::new();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            cancel2.cancel();
        });
        let start = std::time::Instant::now();
        let err = process_turn(
            &config,
            &mut messages,
            &mut state,
            None,
            None,
            None,
            &Hanging,
            &cancel,
            &crate::core::console::Console::none(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "cancelled by user");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "cancel must preempt hanging LLM without 50ms quanta pile-up"
        );
    }

    #[tokio::test]
    async fn cancel_during_tool_io_suppresses_result_fanout() {
        // Cancel lands while the bash tool runs: the per-tool "cancelled"
        // error is shutdown noise, not model input — the turn unwinds
        // without persisting a tool result the model never saw.
        #[derive(Clone)]
        struct SleepOnce {
            round: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }
        impl ModelClient for SleepOnce {
            async fn complete(
                &self,
                _m: &[ChatMessage],
                _w: bool,
                _s: Option<mpsc::Sender<SinkLine>>,
                _c: &(dyn CancellationSource + Send + Sync),
            ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
                let round = self.round.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let message = if round == 0 {
                    ChatMessage::assistant_calls(
                        None,
                        vec![crate::core::types::LlmToolCall {
                            id: "call-1".into(),
                            call_type: "function".into(),
                            function: crate::core::types::FunctionCall {
                                name: "bash".into(),
                                arguments: r#"{"command":"sleep 30"}"#.into(),
                            },
                        }],
                    )
                } else {
                    ChatMessage::assistant("done")
                };
                Ok(Turn {
                    message,
                    usage: Some(Usage {
                        prompt_tokens: 1,
                        completion_tokens: 0,
                        cached_tokens: None,
                    }),
                    stop_reason: None,
                })
            }
        }
        let config = test_config();
        let mut messages = vec![ChatMessage::system("sys")];
        let mut state = ToolState::default();
        let cancel = crate::core::console::CancellationToken::new();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            cancel2.cancel();
        });
        let start = std::time::Instant::now();
        let err = process_turn(
            &config,
            &mut messages,
            &mut state,
            None,
            None,
            None,
            &SleepOnce {
                round: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            },
            &cancel,
            &crate::core::console::Console::none(),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "cancelled by user");
        assert_eq!(messages.len(), 2, "only system + assistant call persist");
        assert!(
            !messages.iter().any(|m| m.role == Role::Tool),
            "cancelled tool errors must not reach the transcript"
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "cancel must preempt tool IO without waiting out the command"
        );
    }
}
