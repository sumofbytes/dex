//! Turn-loop tests, split out of `turn_loop.rs` (the agent state machine).
//! Exposes `TEST_TURN_ENV_LOCK` for cross-module serialization.

use super::tools::tool_calls_conflict;
use super::*;

/// Serializes tests that run `process_turn`: every turn reads
/// `DEX_MAX_TOOL_ITERATIONS` at entry, and the budget test parks the
/// var at "2" across its await — any concurrent `process_turn` would
/// exhaust early. An async mutex because the guard must span awaits
/// (clippy's `await_holding_lock` rejects the std one here).
pub(crate) static TEST_TURN_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
use crate::agent::state::{CancellationSource, ToolState};
use crate::llm::client::ModelClient;
use crate::protocol::{ApiProtocol, ChatMessage, PermissionMode, Provider, Usage};

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
        global_headers: Default::default(),
        connect_timeout_secs: 10,
        request_timeout_secs: 300,
        provider_entries: Default::default(),
        provider_headers: Default::default(),
        api_pinned: false,
    }
}

/// Session-cumulative totals accumulate across calls with saturating
/// adds; zero-prompt usage still counts its output tokens (the one-shot
/// spend summary gates on both totals).
#[tokio::test]
async fn record_usage_accumulates_session_totals() {
    let config = test_config();
    let mut state = ToolState::default();
    record_usage(
        &config,
        &mut state,
        &Console::none(),
        Usage {
            prompt_tokens: 10,
            completion_tokens: 4,
            cached_tokens: None,
        },
        None,
    )
    .await;
    record_usage(
        &config,
        &mut state,
        &Console::none(),
        Usage {
            prompt_tokens: 0,
            completion_tokens: 7,
            cached_tokens: None,
        },
        None,
    )
    .await;
    assert_eq!(state.total_usage, 10);
    assert_eq!(state.total_output, 11);
    assert_eq!(state.last_usage, Some(0));
}

/// `turn.start`/`turn.end` fire around the turn; a `turn.end` hook can
/// act through the host (`dex.tools.call` runs gated, here trusted).
#[tokio::test]
async fn turn_lifecycle_hooks_fire_around_the_turn() {
    let _lock = TEST_TURN_ENV_LOCK.lock().await;
    let _ext_lock = crate::extensions::tests::TEST_GLOBAL_MANAGER_LOCK
        .lock()
        .await;
    // Workspace-confined: the hook writes a relative path in the test
    // process's cwd (the crate root).
    let marker = std::path::PathBuf::from(format!(
        "dex-turn-end-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let marker_str = marker.display().to_string();
    let lua = format!(
            "return function(dex)\n  dex.events.on(\"turn.end\", function(ctx, ev)\n    if not ev.ok then return end\n    dex.tools.call(\"write\", {{ path = \"{}\", content = \"done\" }})\n  end)\nend\n",
            marker_str
        );
    let manifest = "manifest_version: 1\nid: turn-hook\nversion: 0.1.0\ncapabilities: []\n";
    let root = crate::extensions::tests::fixture_exts(&[("turn-hook", manifest, &lua)]);
    crate::extensions::global_manager()
        .refresh_with(std::slice::from_ref(&root))
        .await;
    let config = test_config();
    let mut messages = vec![ChatMessage::system("sys")];
    let mut state = ToolState::default();
    let result = process_turn(AgentRuntime {
        config: &config,
        messages: &mut messages,
        state: &mut state,
        steering_rx: None,
        steering_accepted_tx: None,
        session: None,
        client: &MockModel,
        cancel: &NeverCancel,
        console: &crate::runtime::console::Console::none(),
        filter: None,
        agent_ctx: None,
        tool_budget: None,
    })
    .await;
    assert!(result.is_ok(), "{result:?}");
    assert!(
        marker.is_file(),
        "turn.end hook must have written the marker"
    );
    std::fs::remove_file(&marker).ok();
    std::fs::remove_dir_all(&root).ok();
    // Drop the fixture: the process-global manager would otherwise keep
    // the turn.end hook writing a marker file on every later turn in
    // this test process.
    crate::extensions::global_manager().reset_for_tests().await;
}

/// `before_agent_start` appends to the System prompt for the turn's
/// model requests and is restored when the turn ends (per-turn scope).
#[tokio::test]
async fn before_agent_start_appends_system_prompt_and_restores() {
    let _lock = TEST_TURN_ENV_LOCK.lock().await;
    let _ext_lock = crate::extensions::tests::TEST_GLOBAL_MANAGER_LOCK
        .lock()
        .await;
    let lua = "return function(dex)\n  dex.events.on(\"before_agent_start\", function(ctx, ev)\n    return { append = \"PROMPT-MARKER\" }\n  end)\nend\n";
    let manifest = "manifest_version: 1\nid: promptmark\nversion: 0.1.0\ncapabilities: []\n";
    let root = crate::extensions::tests::fixture_exts(&[("promptmark", manifest, lua)]);
    crate::extensions::global_manager()
        .refresh_with(std::slice::from_ref(&root))
        .await;

    // Recording client: capture the first request's system message.
    #[derive(Clone)]
    struct CaptureModel(Arc<std::sync::Mutex<Option<String>>>);
    impl ModelClient for CaptureModel {
        async fn complete(
            &self,
            messages: &[ChatMessage],
            _with_tools: bool,
            _sink: Option<mpsc::Sender<SinkLine>>,
            _cancel: &(dyn CancellationSource + Send + Sync),
        ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
            let first = messages.first().map(|m| m.content.clone());
            *self.0.lock().unwrap() = first.flatten();
            Ok(Turn {
                message: ChatMessage::assistant("done"),
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
    let system_text = "base persona".to_string();
    let mut messages = vec![ChatMessage::system(&system_text)];
    let captured = Arc::new(std::sync::Mutex::new(None::<String>));
    let mut state = ToolState::default();
    let result = process_turn(AgentRuntime {
        config: &config,
        messages: &mut messages,
        state: &mut state,
        steering_rx: None,
        steering_accepted_tx: None,
        session: None,
        client: &CaptureModel(captured.clone()),
        cancel: &NeverCancel,
        console: &crate::runtime::console::Console::none(),
        filter: None,
        agent_ctx: None,
        tool_budget: None,
    })
    .await;
    assert!(result.is_ok(), "{result:?}");
    // The request the model saw carried the appendix...
    let seen = captured.lock().unwrap().clone().unwrap_or_default();
    assert!(
        seen.contains("PROMPT-MARKER") && seen.contains("base persona"),
        "system prompt must carry the appendix during the turn: {seen:?}"
    );
    // ...and the session's System message is restored afterwards.
    assert_eq!(
        messages.first().and_then(|m| m.content.clone()).as_deref(),
        Some("base persona"),
        "appendix must not persist past the turn"
    );
    std::fs::remove_dir_all(&root).ok();
    crate::extensions::global_manager().reset_for_tests().await;
}

#[tokio::test]
async fn process_turn_completes_with_injected_client() {
    let _lock = TEST_TURN_ENV_LOCK.lock().await;
    let config = test_config();
    let mut messages = vec![ChatMessage::system("sys")];
    let mut state = ToolState::default();
    let result = process_turn(AgentRuntime {
        config: &config,
        messages: &mut messages,
        state: &mut state,
        steering_rx: None,
        steering_accepted_tx: None,
        session: None,
        client: &MockModel,
        cancel: &NeverCancel,
        console: &crate::runtime::console::Console::none(),
        filter: None,
        agent_ctx: None,
        tool_budget: None,
    })
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
                    vec![crate::protocol::LlmToolCall {
                        id: "call-1".into(),
                        call_type: "function".into(),
                        function: crate::protocol::FunctionCall {
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
    let _lock = TEST_TURN_ENV_LOCK.lock().await;
    let config = test_config();
    let mut messages = vec![ChatMessage::system("sys")];
    let mut state = ToolState::default();
    let (sink_tx, mut sink_rx) = mpsc::channel(32);
    let (approval_tx, _approval_rx) = mpsc::channel(16);
    let _ = process_turn(AgentRuntime {
        config: &config,
        messages: &mut messages,
        state: &mut state,
        steering_rx: None,
        steering_accepted_tx: None,
        session: None,
        client: &ToolThenAnswer::new(),
        cancel: &NeverCancel,
        console: &crate::runtime::console::Console::daemon(sink_tx, approval_tx),
        filter: None,
        agent_ctx: None,
        tool_budget: None,
    })
    .await;
    let mut events = Vec::new();
    while let Ok(e) = sink_rx.try_recv() {
        events.push(e);
    }
    assert!(events
        .iter()
        .any(|e| matches!(e, SinkLine::ToolInput { .. })));
    assert!(events
        .iter()
        .any(|e| matches!(e, SinkLine::ToolOutput { .. })));
    // The start announce precedes the completion line, and both carry
    // the call id so the UI can pair them (the DEX-10 case: the input
    // must be visible while the tool still runs, not after).
    let input_pos = events
        .iter()
        .position(|e| matches!(e, SinkLine::ToolInput { .. }))
        .expect("tool input announced");
    let output_pos = events
        .iter()
        .position(|e| matches!(e, SinkLine::ToolOutput { .. }))
        .expect("tool output emitted");
    assert!(
        input_pos < output_pos,
        "tool input must precede its output: {events:?}"
    );
    let input_id = events.iter().find_map(|e| match e {
        SinkLine::ToolInput { id, .. } => Some(id.clone()),
        _ => None,
    });
    let output_id = events.iter().find_map(|e| match e {
        SinkLine::ToolOutput { id, .. } => Some(id.clone()),
        _ => None,
    });
    assert_eq!(input_id.as_deref(), Some("call-1"));
    assert_eq!(input_id, output_id);
}

/// Online context compaction end-to-end: a completed plan step is a
/// boundary, and once the horizon (requests per boundary × remaining
/// steps) covers the cache re-write breakeven, the history compacts and
/// the re-plan reminder lands in the transcript.
#[derive(Clone)]
struct PlanThenCompact {
    round: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    big_path: String,
}

impl ModelClient for PlanThenCompact {
    async fn complete(
        &self,
        _messages: &[ChatMessage],
        _with_tools: bool,
        _sink: Option<mpsc::Sender<SinkLine>>,
        _cancel: &(dyn CancellationSource + Send + Sync),
    ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
        let round = self.round.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let call = |name: &str, args: String, id: &str| {
            ChatMessage::assistant_calls(
                None,
                vec![crate::protocol::LlmToolCall {
                    id: id.into(),
                    call_type: "function".into(),
                    function: crate::protocol::FunctionCall {
                        name: name.into(),
                        arguments: args,
                    },
                }],
            )
        };
        let message = match round {
                // Filler rounds before and after the big read: the
                // summarizer needs a multi-message span to cut (the
                // min-messages guard), and the rounds build the
                // requests-per-boundary sample.
                r if (0..10).contains(&r) => {
                    call("ls", "{}".to_string(), &format!("call-ls-{r}"))
                }
                10 => call(
                    "read",
                    format!(r#"{{"path":"{}"}}"#, self.big_path),
                    "call-read",
                ),
                r if (11..30).contains(&r) => {
                    call("ls", "{}".to_string(), &format!("call-ls-{r}"))
                }
                30 => call(
                    "update_plan",
                    r#"{"steps":[{"id":"1","goal":"read the big file","status":"completed"},{"id":"2","goal":"second","status":"pending"},{"id":"3","goal":"third","status":"pending"}]}"#
                        .to_string(),
                    "call-plan",
                ),
                _ => ChatMessage::assistant("done"),
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
async fn online_compaction_compacts_at_an_economical_boundary() {
    // Shared env lock + panic-safe restore: the var is read by other
    // test modules in this binary (e.g. the protocol schema test), so
    // unsynchronized set/remove flakes those assertions.
    let _lock = TEST_TURN_ENV_LOCK.lock().await;
    let _env = crate::session::EnvGuard(vec![(
        crate::agent::online_compaction::ONLINE_COMPACTION_ENV,
        std::env::var_os(crate::agent::online_compaction::ONLINE_COMPACTION_ENV),
    )]);
    std::env::set_var(crate::agent::online_compaction::ONLINE_COMPACTION_ENV, "1");
    // The read tool clamps a single file to ~64 KiB (~16k tokens), so
    // shrink keep_recent below that: the archive then clears the
    // breakeven at the first demonstrated boundary (31 requests, 2
    // steps remaining → horizon 63 vs breakeven ≈ 16).
    let mut config = test_config();
    config.keep_recent_tokens = 4_000;
    // ~200 KB of read output: far beyond keep_recent_tokens, so the
    // archive is large enough for the economics to fire at the first
    // demonstrated boundary (31 requests, 2 steps remaining).
    let big_path = "target/dex-online-compact-test.txt";
    let marker = "ONLINE-COMPACT-MARKER-7f3a";
    let big = format!(
        "{}\n",
        (0..2000)
            .map(|i| format!("line {i:0>4} {}", "x".repeat(90)))
            .collect::<Vec<_>>()
            .join("\n")
    );
    std::fs::write(big_path, big).unwrap();

    let mut messages = vec![ChatMessage::system("sys")];
    let mut state = ToolState::default();
    let result = process_turn(AgentRuntime {
        config: &config,
        messages: &mut messages,
        state: &mut state,
        steering_rx: None,
        steering_accepted_tx: None,
        session: None,
        client: &PlanThenCompact {
            round: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            big_path: big_path.to_string(),
        },
        cancel: &NeverCancel,
        console: &crate::runtime::console::Console::none(),
        filter: None,
        agent_ctx: None,
        tool_budget: Some(64),
    })
    .await;
    // Remove the fixture before asserting so a failed assert doesn't
    // leak it into `target/`.
    let _ = std::fs::remove_file(big_path);

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(result.as_deref().unwrap(), "done");
    // The boundary compacted: exactly one compaction recorded, the
    // re-plan reminder is in the transcript, and the big read output is
    // gone from the context.
    assert_eq!(state.online_compaction.native_compaction_count(), 1);
    assert!(messages.iter().any(|m| {
        m.name.as_deref() == Some("compact")
            && m.content
                .as_deref()
                .is_some_and(|c| c.contains("call update_plan"))
    }));
    assert!(!messages
        .iter()
        .any(|m| m.content.as_deref().is_some_and(|c| c.contains(marker))));
    // The remaining plan steps ride along on the reminder.
    let reminder = messages
        .iter()
        .find(|m| m.name.as_deref() == Some("compact"))
        .unwrap();
    assert!(reminder.content.as_deref().unwrap().contains("- second"));
}

#[tokio::test]
async fn conflict_serialize_still_serializes_same_path_edits() {
    // TDD Phase 2: same-path edits must take the serialize path.
    let calls = vec![
        crate::protocol::LlmToolCall {
            id: "a".into(),
            call_type: "function".into(),
            function: crate::protocol::FunctionCall {
                name: "edit".into(),
                arguments: r#"{"path":"same.rs"}"#.into(),
            },
        },
        crate::protocol::LlmToolCall {
            id: "b".into(),
            call_type: "function".into(),
            function: crate::protocol::FunctionCall {
                name: "edit".into(),
                arguments: r#"{"path":"same.rs"}"#.into(),
            },
        },
    ];
    assert!(tool_calls_conflict(&calls));
    let different = vec![
        crate::protocol::LlmToolCall {
            id: "a".into(),
            call_type: "function".into(),
            function: crate::protocol::FunctionCall {
                name: "read".into(),
                arguments: r#"{"path":"a.rs"}"#.into(),
            },
        },
        crate::protocol::LlmToolCall {
            id: "b".into(),
            call_type: "function".into(),
            function: crate::protocol::FunctionCall {
                name: "read".into(),
                arguments: r#"{"path":"b.rs"}"#.into(),
            },
        },
    ];
    assert!(!tool_calls_conflict(&different));
}

#[tokio::test]
async fn parallel_batch_preserves_input_order() {
    // Distinct-path reads take the parallel fan-out; results must come
    // back input-ordered so the caller can zip them with the original
    // calls — a slow first worker must never shift attribution onto the
    // second call's result.
    use crate::tools::Policy;
    let call = |id: &str, path: &str| crate::protocol::LlmToolCall {
        id: id.into(),
        call_type: "function".into(),
        function: crate::protocol::FunctionCall {
            name: "read".into(),
            arguments: format!(r#"{{"path":"{path}"}}"#),
        },
    };
    let calls = vec![
        call("a", "definitely-not-here-a.rs"),
        call("b", "definitely-not-here-b.rs"),
    ];
    assert!(!tool_calls_conflict(&calls));
    let (sink_tx, mut sink_rx) = mpsc::channel(32);
    let (approval_tx, _approval_rx) = mpsc::channel(16);
    let console = crate::runtime::console::Console::daemon(sink_tx, approval_tx);
    let results = run_tool_batch(&calls, &NeverCancel, &Policy::trusted(), None, &console).await;
    assert_eq!(results.len(), 2);
    assert!(
        results[0].1.contains("definitely-not-here-a.rs"),
        "first result keeps first call's input"
    );
    assert!(
        results[1].1.contains("definitely-not-here-b.rs"),
        "second result keeps second call's input"
    );
    // Parallel fan-out announces every call up front, each tagged with
    // its own id so the UI can open both blocks before either finishes.
    let mut announced = Vec::new();
    while let Ok(e) = sink_rx.try_recv() {
        if let SinkLine::ToolInput { id, .. } = e {
            announced.push(id);
        }
    }
    announced.sort();
    assert_eq!(announced, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn then_run_forces_serialization() {
    // A `write`/`edit` carrying `then_run` runs a shell command, so the
    // batch must serialize even across distinct paths — otherwise N edits
    // fan out N concurrent shells.
    let call = |name: &str, args: &str| crate::protocol::LlmToolCall {
        id: name.into(),
        call_type: "function".into(),
        function: crate::protocol::FunctionCall {
            name: name.into(),
            arguments: args.into(),
        },
    };
    assert!(tool_calls_conflict(&[
        call("edit", r#"{"path":"a.rs","then_run":"cargo test"}"#),
        call("edit", r#"{"path":"b.rs"}"#),
    ]));
    // A bare `bash` likewise serializes the batch.
    assert!(tool_calls_conflict(&[call("bash", r#"{"command":"ls"}"#)]));
    // A blank `then_run` is not a command: the writes stay parallel.
    assert!(!tool_calls_conflict(&[
        call("edit", r#"{"path":"a.rs","then_run":"  "}"#),
        call("edit", r#"{"path":"b.rs"}"#),
    ]));
}

#[test]
fn overflow_wording_is_recognized() {
    for msg in [
        "API error: This model's maximum context length is 8192 tokens",
        "input length exceeds context window",
        "your prompt is too long",
        "400 Too many tokens in request",
        "context size exceeds limit",
        "prompt too long for context",
        "token limit exceeded",
        "please reduce the length of the context",
    ] {
        assert!(is_context_overflow(msg), "{msg}");
    }
    assert!(!is_context_overflow("API error: invalid api key"));
    assert!(!is_context_overflow("stream idle for over 90s"));
    // Generic length validation without a context anchor must not
    // trigger a wasteful emergency compaction.
    assert!(!is_context_overflow("reduce the length of your filename"));
}

#[test]
fn queue_recall_removes_newest_match_only() {
    let mut pending = Vec::new();
    apply_queue_msg(&mut pending, QueueMsg::Content("a".into()));
    apply_queue_msg(&mut pending, QueueMsg::Content("a".into()));
    apply_queue_msg(&mut pending, QueueMsg::Content("b".into()));
    // Newest matching item goes first; the older duplicate stays.
    apply_queue_msg(&mut pending, QueueMsg::Recall("a".into()));
    assert_eq!(pending, vec!["a".to_string(), "b".to_string()]);
    // A recall with no queued match is a no-op (already injected).
    apply_queue_msg(&mut pending, QueueMsg::Recall("zzz".into()));
    assert_eq!(pending, vec!["a".to_string(), "b".to_string()]);
    // A recall that arrives after its item (separate drain) still works.
    apply_queue_msg(&mut pending, QueueMsg::Recall("a".into()));
    assert_eq!(pending, vec!["b".to_string()]);
}

/// A model that stalls in a tool-call loop is cut off by the per-turn
/// budget instead of burning unbounded tokens; the transcript stays
/// coherent (every call has its result).
#[tokio::test]
async fn turn_budget_stops_an_endless_tool_loop() {
    #[derive(Clone)]
    struct AlwaysTool;
    impl ModelClient for AlwaysTool {
        async fn complete(
            &self,
            _m: &[ChatMessage],
            _w: bool,
            _s: Option<mpsc::Sender<SinkLine>>,
            _c: &(dyn CancellationSource + Send + Sync),
        ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
            Ok(Turn {
                message: ChatMessage::assistant_calls(
                    Some("thinking about it".to_string()),
                    vec![crate::protocol::LlmToolCall {
                        id: format!("c{}", uuid::Uuid::new_v4().simple()),
                        call_type: "function".into(),
                        function: crate::protocol::FunctionCall {
                            name: "read".into(),
                            arguments: r#"{"path":"README.md"}"#.into(),
                        },
                    }],
                ),
                usage: Some(Usage {
                    prompt_tokens: 1,
                    completion_tokens: 0,
                    cached_tokens: None,
                }),
                stop_reason: None,
            })
        }
    }
    // Serialize with every other process_turn test: while this test's
    // DEX_MAX_TOOL_ITERATIONS=2 is live across the process_turn await,
    // any concurrent process_turn would read it and exhaust early.
    let _lock = TEST_TURN_ENV_LOCK.lock().await;
    let prev = std::env::var("DEX_MAX_TOOL_ITERATIONS").ok();
    std::env::set_var("DEX_MAX_TOOL_ITERATIONS", "2");
    let config = test_config();
    let mut messages = vec![ChatMessage::system("sys")];
    let mut state = ToolState::default();
    let err = process_turn(AgentRuntime {
        config: &config,
        messages: &mut messages,
        state: &mut state,
        steering_rx: None,
        steering_accepted_tx: None,
        session: None,
        client: &AlwaysTool,
        cancel: &NeverCancel,
        console: &crate::runtime::console::Console::none(),
        filter: None,
        agent_ctx: None,
        tool_budget: None,
    })
    .await
    .unwrap_err()
    .to_string();
    match prev {
        Some(v) => std::env::set_var("DEX_MAX_TOOL_ITERATIONS", v),
        None => std::env::remove_var("DEX_MAX_TOOL_ITERATIONS"),
    }
    assert!(err.contains("turn budget exhausted"), "{err}");
    // Transcript coherence: every assistant batch has its tool result.
    let unanswered = messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .map(|m| m.tool_calls.as_ref().map(|c| c.len()).unwrap_or_default())
        .sum::<usize>();
    let answered = messages.iter().filter(|m| m.role == Role::Tool).count();
    assert_eq!(
        answered, unanswered,
        "budget stop must leave a coherent transcript"
    );
}

/// When the provider rejects the request because the history no longer
/// fits, the loop emergency-compacts and retries once instead of failing
/// the turn.
#[tokio::test]
async fn context_overflow_compacts_and_retries_once() {
    let _lock = TEST_TURN_ENV_LOCK.lock().await;
    #[derive(Clone)]
    struct OverflowThenOk {
        round: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl ModelClient for OverflowThenOk {
        async fn complete(
            &self,
            messages: &[ChatMessage],
            _w: bool,
            _s: Option<mpsc::Sender<SinkLine>>,
            _c: &(dyn CancellationSource + Send + Sync),
        ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
            let round = self.round.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if round == 0 {
                return Err("API error: maximum context length exceeded".into());
            }
            // After compaction the history must actually have shrunk.
            assert!(
                messages.len() < 20,
                "compaction must cut history before the retry"
            );
            Ok(Turn {
                message: ChatMessage::assistant("recovered"),
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
    messages.push(ChatMessage::user("goal: fix the flaky test"));
    // Enough medium-size turns that compaction has something to cut.
    for i in 0..20 {
        messages.push(ChatMessage::user(format!("u{i}: {}", "x".repeat(300))));
        messages.push(ChatMessage::assistant(format!("a{i}: {}", "y".repeat(300))));
    }
    let mut state = ToolState::default();
    let result = process_turn(AgentRuntime {
        config: &config,
        messages: &mut messages,
        state: &mut state,
        steering_rx: None,
        steering_accepted_tx: None,
        session: None,
        client: &OverflowThenOk {
            round: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        },
        cancel: &NeverCancel,
        console: &crate::runtime::console::Console::none(),
        filter: None,
        agent_ctx: None,
        tool_budget: None,
    })
    .await;
    assert!(
        result.is_ok(),
        "overflow must compact and retry: {result:?}"
    );
    // Exactly one summary entry, and the failed prompt round did not
    // leave the transcript wedged.
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.name.as_deref() == Some("summary"))
            .count(),
        1
    );
}

#[tokio::test]
async fn cancel_during_llm_call_unwinds_promptly() {
    let _lock = TEST_TURN_ENV_LOCK.lock().await;
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
    let cancel = crate::runtime::console::CancellationToken::new();
    let cancel2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancel2.cancel();
    });
    let start = std::time::Instant::now();
    let err = process_turn(AgentRuntime {
        config: &config,
        messages: &mut messages,
        state: &mut state,
        steering_rx: None,
        steering_accepted_tx: None,
        session: None,
        client: &Hanging,
        cancel: &cancel,
        console: &crate::runtime::console::Console::none(),
        filter: None,
        agent_ctx: None,
        tool_budget: None,
    })
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
    let _lock = TEST_TURN_ENV_LOCK.lock().await;
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
                    vec![crate::protocol::LlmToolCall {
                        id: "call-1".into(),
                        call_type: "function".into(),
                        function: crate::protocol::FunctionCall {
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
    let cancel = crate::runtime::console::CancellationToken::new();
    let cancel2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancel2.cancel();
    });
    let start = std::time::Instant::now();
    let err = process_turn(AgentRuntime {
        config: &config,
        messages: &mut messages,
        state: &mut state,
        steering_rx: None,
        steering_accepted_tx: None,
        session: None,
        client: &SleepOnce {
            round: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        },
        cancel: &cancel,
        console: &crate::runtime::console::Console::none(),
        filter: None,
        agent_ctx: None,
        tool_budget: None,
    })
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

#[derive(Clone)]
struct RepeatedGuardScript {
    commands: Vec<&'static str>,
    round: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl RepeatedGuardScript {
    fn new(commands: Vec<&'static str>) -> Self {
        Self {
            commands,
            round: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl ModelClient for RepeatedGuardScript {
    async fn complete(
        &self,
        _m: &[ChatMessage],
        _w: bool,
        _s: Option<mpsc::Sender<SinkLine>>,
        _c: &(dyn CancellationSource + Send + Sync),
    ) -> Result<Turn, Box<dyn std::error::Error + Send + Sync>> {
        let round = self.round.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let message = if round < self.commands.len() {
            ChatMessage::assistant_calls(
                None,
                vec![crate::protocol::LlmToolCall {
                    id: format!("call-{round}"),
                    call_type: "function".into(),
                    function: crate::protocol::FunctionCall {
                        name: "bash".into(),
                        arguments: format!(r#"{{"command":"{}"}}"#, self.commands[round]),
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
async fn repeated_tool_guard_counts_after_ring_eviction() {
    // k, a, b, c, k, d, k: the ring is full (6) when the 7th identical
    // call arrives, and the evicted front entry is itself a match.
    // Counting after the push+eviction reads 2 — under the threshold.
    // (A pre-push count would read 3 and feed the model a spurious
    // "repeated identical tool call" error on this call.)
    // Lock so the budget test's env window can't overlap this run.
    let _lock = TEST_TURN_ENV_LOCK.lock().await;
    let config = test_config();
    let mut messages = vec![ChatMessage::system("sys")];
    let mut state = ToolState::default();
    let guard_err = "Error: repeated identical tool call; choose a different action or finish.";
    let _ = process_turn(AgentRuntime {
        config: &config,
        messages: &mut messages,
        state: &mut state,
        steering_rx: None,
        steering_accepted_tx: None,
        session: None,
        client: &RepeatedGuardScript::new(vec![
            "echo repeat-probe",
            "echo other-a",
            "echo other-b",
            "echo other-c",
            "echo repeat-probe",
            "echo other-d",
            "echo repeat-probe",
        ]),
        cancel: &NeverCancel,
        console: &crate::runtime::console::Console::none(),
        filter: None,
        agent_ctx: None,
        tool_budget: None,
    })
    .await;
    let guard_hits = messages
        .iter()
        .filter(|m| m.role == Role::Tool && m.content.as_deref() == Some(guard_err))
        .count();
    assert_eq!(
        guard_hits, 0,
        "guard must not fire: ring eviction removed a match before counting"
    );
}

#[tokio::test]
async fn repeated_tool_guard_fires_on_third_consecutive_call() {
    // Positive control: three identical calls in a row must trip the
    // guard on the third one only.
    // Lock so the budget test's env window can't overlap this run.
    let _lock = TEST_TURN_ENV_LOCK.lock().await;
    let config = test_config();
    let mut messages = vec![ChatMessage::system("sys")];
    let mut state = ToolState::default();
    let guard_err = "Error: repeated identical tool call; choose a different action or finish.";
    let _ = process_turn(AgentRuntime {
        config: &config,
        messages: &mut messages,
        state: &mut state,
        steering_rx: None,
        steering_accepted_tx: None,
        session: None,
        client: &RepeatedGuardScript::new(vec!["echo probe", "echo probe", "echo probe"]),
        cancel: &NeverCancel,
        console: &crate::runtime::console::Console::none(),
        filter: None,
        agent_ctx: None,
        tool_budget: None,
    })
    .await;
    let guard_hits = messages
        .iter()
        .filter(|m| m.role == Role::Tool && m.content.as_deref() == Some(guard_err))
        .count();
    assert_eq!(
        guard_hits, 1,
        "exactly the third identical call trips: {guard_hits}"
    );
    let last_tool = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Tool)
        .and_then(|m| m.content.as_deref());
    assert_eq!(
        last_tool,
        Some(guard_err),
        "the guard error must land on the third call's result"
    );
}

/// Phase 2 exit: a child runs the same loop with its own seed, no
/// steering, and a filter — the allowlist is enforced at dispatch
/// (denied `bash` fails closed as a tool result) while the allowed
/// `read` runs, and the history is the child's own.
#[derive(Clone)]
struct FilterProbe {
    round: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl FilterProbe {
    fn new() -> Self {
        Self {
            round: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
}

impl ModelClient for FilterProbe {
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
                vec![
                    crate::protocol::LlmToolCall {
                        id: "call-1".into(),
                        call_type: "function".into(),
                        function: crate::protocol::FunctionCall {
                            name: "read".into(),
                            arguments: r#"{"path":"Cargo.toml"}"#.into(),
                        },
                    },
                    crate::protocol::LlmToolCall {
                        id: "call-2".into(),
                        call_type: "function".into(),
                        function: crate::protocol::FunctionCall {
                            name: "bash".into(),
                            arguments: r#"{"command":"echo hi"}"#.into(),
                        },
                    },
                ],
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
async fn filtered_child_run_enforces_allowlist_and_keeps_own_history() {
    let _lock = TEST_TURN_ENV_LOCK.lock().await;
    let config = test_config();
    let mut messages = vec![ChatMessage::system("child seed: explore only")];
    let mut state = ToolState::default();
    let filter = ToolFilter::new("explorer", ["read", "grep", "find"]);
    let result = process_turn(AgentRuntime {
        config: &config,
        messages: &mut messages,
        state: &mut state,
        steering_rx: None,
        steering_accepted_tx: None,
        session: None,
        client: &FilterProbe::new(),
        cancel: &NeverCancel,
        console: &crate::runtime::console::Console::none(),
        filter: Some(&filter),
        agent_ctx: None,
        tool_budget: None,
    })
    .await;
    assert_eq!(result.unwrap(), "done");
    let tool_text: String = messages
        .iter()
        .filter(|m| m.role == Role::Tool)
        .filter_map(|m| m.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        tool_text.contains("dex"),
        "allowed read must run: {tool_text}"
    );
    assert!(
        tool_text.contains("not in explorer's tool allowlist"),
        "denied bash must fail closed with the policy reason: {tool_text}"
    );
    // The child's history is its own seed plus this turn — no parent
    // transcript is ever inherited.
    assert_eq!(
        messages.first().and_then(|m| m.content.as_deref()),
        Some("child seed: explore only")
    );
    assert!(
        !messages.iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|c| c.contains("parent transcript"))),
        "child must never see the parent transcript"
    );
}
