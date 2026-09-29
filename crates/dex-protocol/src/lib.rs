//! Stable HTTP and SSE types shared by dex clients and daemons.
//!
//! This crate intentionally contains serializable data only. It does not
//! depend on dex's daemon, provider, agent, or terminal runtime.

use serde::{Deserialize, Serialize};

/// Default maximum number of event-journal rows returned by one page.
pub const EVENTS_PAGE_LIMIT: usize = 1000;

/// Request to create a new session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub cwd: String,
    pub name: Option<String>,
}

/// Response after creating a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionResponse {
    pub session_id: String,
    pub path: String,
}

/// Summary of a session for listing. Constructed via serde from the
/// daemon's `GET /api/sessions` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub path: String,
    pub name: Option<String>,
    pub cwd: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub message_count: usize,
    /// Child-agent runs recorded under this session (§16); `0` from older
    /// daemons that don't report them.
    #[serde(default)]
    pub child_agents: usize,
    /// Child runs whose last turn marker is an unterminated `turn_start`
    /// (crashed or daemon-restart-killed children).
    #[serde(default)]
    pub interrupted_children: usize,
}

/// Request to run a shell command directly (`!`/`!!` prefix in the TUI),
/// bypassing the agent loop. Runs the daemon's `bash` tool in the session
/// workspace with no approval step — the `!` itself is the approval.
/// `exclude_from_context` (`!!`): the run is still saved to session
/// history and shown in the transcript, but never sent to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellRequest {
    pub command: String,
    #[serde(default)]
    pub exclude_from_context: bool,
}

/// Response from running a shell command directly. `output` is the combined
/// stdout/stderr (already clamped for display); `code` is the process exit
/// code (`None` when killed or timed out).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellResponse {
    pub output: String,
    pub success: bool,
    pub code: Option<i32>,
}

/// Request to enqueue a steering message into an active turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SteerRequest {
    pub content: String,
}

/// Request to enqueue a follow-up message (runs as a chained turn after the
/// current one completes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FollowupRequest {
    pub content: String,
}

/// Request to recall a queued steering/follow-up message that the daemon has
/// not injected yet (client-side "edit a waiting message").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallRequest {
    pub content: String,
    /// `true` recalls a queued follow-up; `false` (default) a queued steer.
    #[serde(default)]
    pub followup: bool,
}

/// Request to submit a chat prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub prompt: String,
    #[serde(default)]
    pub skill_dirs: Vec<String>,
    /// Optional per-request overrides; when absent the daemon uses its own
    /// environment. These let a co-located client forward its
    /// CLI flags through to the turn.
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission: Option<String>,
    /// Agent mode (`plan`/`manual`/`auto`); preferred over `permission` when
    /// present — the daemon derives the permission from it and, for `plan`,
    /// appends the plan directive. Absent (older clients) leaves behaviour
    /// unchanged.
    #[serde(default)]
    pub mode: Option<String>,
    /// Extra HTTP headers for the provider request (client `--header`
    /// flags). Merged over the daemon's own configured headers.
    #[serde(default)]
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    pub plan: Option<String>,
    /// Custom base system prompt text (client `--system-prompt` /
    /// `--system-prompt-file`, already resolved client-side). Replaces the
    /// built-in base; project/extensions/skills still append. Older clients
    /// omit it and the daemon falls back to its own env/file layers.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Per-turn reasoning effort override from a remote `/thinking` choice.
    /// `None` (older clients omit it) uses the daemon default; `Some("")`
    /// is an explicit clear (unset, ignoring env); otherwise the level.
    #[serde(default)]
    pub thinking_effort: Option<String>,
}

/// Request to run one registered extension slash command on the daemon
/// (`/<name> [args]` in a remote TUI). The daemon resolves `name` against
/// its own manager — the client never needs the extension id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionRunRequest {
    pub name: String,
    #[serde(default)]
    pub arg: String,
}

/// Response to approve/deny a tool execution. `request_id` must match the
/// `ApprovalRequired` stream event the decision resolves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub request_id: String,
    pub decision: ApprovalDecision,
}

/// One labeled option inside a [`Question`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

/// One structured question the model asks via the `ask_user` tool. Bounds
/// (option count, label lengths) are enforced in the schema AND clamped
/// server-side by the executor — schema constraints are advisory for some
/// models.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    /// Checkbox vs radio behavior on the picker surfaces.
    #[serde(default)]
    pub multi_select: bool,
    /// 0-based index into `options`, used by the headless empty-line
    /// shortcut. `None` re-prompts instead.
    #[serde(default)]
    pub default: Option<usize>,
}

/// One answer slot in a [`QuestionResponse`]; `answers[i]` corresponds to
/// `questions[i]` of the `QuestionRequired` event (unanswered slots are
/// [`QuestionAnswer::Dismiss`]). `Choice`/`Multi` index into
/// `Question.options` only — the implicit "Other" row is UI-only and never
/// shifts those indices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum QuestionAnswer {
    #[serde(rename = "choice")]
    Choice(usize),
    #[serde(rename = "multi")]
    Multi(Vec<usize>),
    #[serde(rename = "text")]
    Text(String),
    #[serde(rename = "dismiss")]
    Dismiss,
}

impl QuestionAnswer {
    /// Audit spelling for the answer kind ("choice"/"multi"/"text"/
    /// "dismiss"). Single source so audit rows never drift from the wire,
    /// mirroring [`ApprovalDecision::as_str`]; the option labels for
    /// `choice`/`multi` are joined by the caller, which owns the question.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Choice(_) => "choice",
            Self::Multi(_) => "multi",
            Self::Text(_) => "text",
            Self::Dismiss => "dismiss",
        }
    }
}

/// POST body answering a `QuestionRequired` stream event, mirroring
/// [`ApprovalResponse`]. One response resolves the whole batch: partial
/// answers never cross the wire; UIs buffer per-question state and submit
/// once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionResponse {
    pub request_id: String,
    pub answers: Vec<QuestionAnswer>,
}

// Single enum for the wire AND the agent loop. Serde spellings are the wire contract ("allow_once" /
// "allow_session" / "deny"); `as_str` is the audit spelling ("once" /
// "session" / "deny") — single source so audit rows never drift from the
// wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    AllowSession,
    Deny,
}

impl ApprovalDecision {
    /// Audit/wire spelling for an approval decision ("once"/"session"/
    /// "deny"). Single source so audit rows never drift from the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllowOnce => "once",
            Self::AllowSession => "session",
            Self::Deny => "deny",
        }
    }
}

/// SSE event types streamed during a chat turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum StreamEvent {
    /// Incremental assistant text.
    #[serde(rename = "assistant_text")]
    AssistantText(String),

    /// Incremental model reasoning ("thinking") delta.
    #[serde(rename = "thinking")]
    Thinking(String),

    /// A tool call was initiated.
    #[serde(rename = "tool_call")]
    ToolCall {
        name: String,
        args: serde_json::Value,
        /// The LLM's tool-call id, pairing this event with its
        /// `tool_result`. Optional with a serde default so older
        /// daemons/journals parse unchanged (they pair by tail order).
        #[serde(default)]
        id: String,
    },

    /// A tool call completed.
    #[serde(rename = "tool_result")]
    ToolResult {
        name: String,
        summary: String,
        success: bool,
        /// A few informational output lines to show under the summary.
        #[serde(default)]
        preview: Vec<String>,
        /// Wall-clock seconds the tool took; 0 when unknown.
        #[serde(default)]
        duration: f64,
        /// Pairs with the `tool_call` that started this call; empty when
        /// the starter predates ids (old journals, session rebuild).
        #[serde(default)]
        id: String,
    },

    /// The agent needs user approval for a tool. `agent` is set (V1b, plan
    /// §12) when the requester is a background child agent: the prompt
    /// renders labeled ("explorer wants to run bash: …") and stays
    /// answerable after the parent turn ends. Optional with a serde default
    /// so older clients/datasets parse unchanged.
    #[serde(rename = "approval_required")]
    ApprovalRequired {
        request_id: String,
        name: String,
        input: String,
        #[serde(default)]
        agent: Option<String>,
    },

    /// The model asked a structured question via the `ask_user` tool:
    /// 1–4 questions, each with 2–4 labeled options plus an implicit
    /// UI-only free-text row. `agent` is set when the requester is a
    /// background child agent (same labeling as `ApprovalRequired`).
    /// Optional with a serde default so older clients/datasets parse
    /// unchanged.
    #[serde(rename = "question_required")]
    QuestionRequired {
        request_id: String,
        questions: Vec<Question>,
        #[serde(default)]
        agent: Option<String>,
    },

    /// The turn completed successfully. `usage` is the daemon-reported prompt
    /// token count for the conversation, when known; `cached` the cached-token
    /// subset the provider reported for the last call, when known.
    #[serde(rename = "turn_complete")]
    TurnComplete {
        response: String,
        #[serde(default)]
        usage: Option<u64>,
        #[serde(default)]
        cached: Option<u64>,
    },

    /// The turn failed.
    #[serde(rename = "turn_failed")]
    TurnFailed { error: String },

    /// Prompt and completion tokens reported by the provider after each LLM
    /// call within a turn, letting the client render live context usage in
    /// its status bar. `cached` is the provider-reported cached-token
    /// subset; `cost` is the daemon-priced USD cost of this call (catalog
    /// or `DEX_COST_PER_1K` fallback), accumulated client-side so both
    /// processes always agree on spend; `output` is the completion-token
    /// count for the same call, accumulated for the status bar's output
    /// figure; `gen_ms` is the wall-clock duration the daemon measured for
    /// the call, the denominator for the footer's output tokens/s rate.
    /// Older daemons omit `gen_ms`; the client then hides the rate.
    #[serde(rename = "usage")]
    Usage {
        tokens: u64,
        #[serde(default)]
        cached: Option<u64>,
        #[serde(default)]
        cost: f64,
        #[serde(default)]
        output: u64,
        #[serde(default)]
        gen_ms: Option<u64>,
    },

    /// A system message (e.g. compaction notice).
    #[serde(rename = "system")]
    System(String),

    /// An error occurred.
    #[serde(rename = "error")]
    Error(String),

    /// Plan update for remote UI sync. New fields carry the full task
    /// contract (goal, constraints, steps, acceptance) so a remote TUI does
    /// not drop constraints/acceptance when the daemon syncs the plan back.
    #[serde(rename = "plan")]
    Plan {
        goal: Option<String>,
        steps: Vec<(String, bool)>,
        #[serde(default)]
        constraints: Vec<String>,
        #[serde(default)]
        acceptance: Vec<(String, bool)>,
    },

    /// Steering message was accepted by the agent loop (mid-turn injection).
    /// The client uses this to clear its `pending_steering` badge and render
    /// the user's steer as a transcript block.
    #[serde(rename = "steering_accepted")]
    SteeringAccepted { content: String },

    /// Follow-up message was accepted and queued for the next chained turn.
    #[serde(rename = "followup_accepted")]
    FollowupAccepted { content: String },

    /// V1b typed child-agent lifecycle (plan §15): first-class variants so a
    /// consumer reads typed fields instead of parsing the V1a `System`
    /// prefix. An explicit wire bump — the shipped client's
    /// `SseFramer::ingest` and `lenient_array` skip unknown types but
    /// still advance the seq cursor, so replay never stalls (a direct
    /// `StreamEnvelope` parse still errors; leniency lives in those
    /// callers). The V1a `System` lines stay journaled beside them for
    /// old clients.
    #[serde(rename = "agent_spawned")]
    AgentSpawned { agent_id: String, name: String },

    /// The child's state advanced: which tool it is running now (§15).
    #[serde(rename = "agent_progress")]
    AgentProgress {
        agent_id: String,
        state: String,
        #[serde(default)]
        current_tool: Option<String>,
    },

    /// A child reached a terminal state (§15): status is the same word the
    /// V1a lifecycle line uses ("completed"/"failed"/"cancelled"/"timed out").
    #[serde(rename = "agent_completed")]
    AgentCompleted { agent_id: String, status: String },

    /// One live child-agent transcript line (plan §20 child view): the
    /// child's own console output, wrapped in the same `StreamEvent` shapes
    /// the parent turn streams (`AssistantText`/`Thinking`/`ToolCall`/
    /// `ToolResult`/`System`/`Error`), so a client renders the child
    /// transcript with the identical block semantics as the parent's. The
    /// event rides the same journal + SSE path as the other typed variants;
    /// older clients skip the unknown type without stalling replay.
    #[serde(rename = "agent_line")]
    AgentLine {
        agent_id: String,
        /// The child's definition name (view title); repeated so a client
        /// keying the log by id never needs the spawn event first.
        name: String,
        event: Box<StreamEvent>,
    },
}

/// One numbered SSE event (P10). `seq` is the daemon-assigned, per-session
/// monotonic cursor; the event journal persists every payload so a
/// reconnecting client can replay `GET /api/sessions/{id}/events?since=seq`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEnvelope {
    pub seq: u64,
    #[serde(flatten)]
    pub event: StreamEvent,
}

/// Response from `POST /api/sessions/{id}/reattach`: the session is made
/// usable again and the client gets the cursor to replay from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReattachResponse {
    pub session_id: String,
    pub seq: u64,
}

/// Response from `GET /api/sessions/{id}/events?since=...`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsResponse {
    pub events: Vec<StreamEnvelope>,
    pub next_seq: u64,
}

/// A single skill entry advertised by the daemon (discovered from its
/// workspace). The body is fetched on demand via `POST /api/sessions/{id}/skill`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
}

/// Request to load a skill into the current session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadSkillRequest {
    pub name: String,
    #[serde(default)]
    pub skill_dirs: Vec<String>,
}

/// Response after loading a skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadSkillResponse {
    pub name: String,
    pub description: String,
    pub content: String,
}

/// Runtime info about the daemon, returned by `GET /api/config`. The client
/// TUI uses it for the status footer and slash-command suggestions; the
/// daemon resolves provider/model/permission from its own environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub api: String,
    pub available_models: Vec<String>,
    pub context_window: u64,
    pub permission: String,
    pub cwd: String,
    pub git_branch: Option<String>,
    pub git_dirty: bool,
    /// Effective reasoning effort on the daemon (stored `/thinking` choice >
    /// env > file). Carried so the client's `/thinking` display matches what
    /// turns actually use instead of reporting "unset".
    #[serde(default)]
    pub thinking_effort: Option<String>,
    /// Mismatch warning when the effort isn't advertised for the model.
    /// Surfaced as a transcript line; never `eprintln!`d from the daemon,
    /// which shares the TUI's terminal and would corrupt it.
    #[serde(default)]
    pub thinking_warning: Option<String>,
}

/// Lightweight git status for the footer, returned by `GET /api/git`.
/// Split out of `DaemonInfo` so the TUI can poll for branch/dirty changes
/// without re-resolving the full provider config (`LlmConfig::from_env`)
/// on every poll.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitInfo {
    pub git_branch: Option<String>,
    #[serde(default)]
    pub git_dirty: bool,
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_line_round_trips_with_nested_event() {
        let event = StreamEvent::AgentLine {
            agent_id: "a1".into(),
            name: "explorer".into(),
            event: Box::new(StreamEvent::ToolResult {
                name: "bash".into(),
                summary: "ls".into(),
                success: true,
                preview: vec!["file.rs".into()],
                duration: 0.1,
                id: "t1".into(),
            }),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"agent_line\""));
        let back: StreamEvent = serde_json::from_str(&json).unwrap();
        match back {
            StreamEvent::AgentLine {
                agent_id,
                name,
                event,
            } => {
                assert_eq!(agent_id, "a1");
                assert_eq!(name, "explorer");
                assert!(matches!(*event, StreamEvent::ToolResult { .. }));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn git_info_round_trips_and_defaults_dirty() {
        let info = GitInfo {
            git_branch: Some("main".into()),
            git_dirty: true,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: GitInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.git_branch.as_deref(), Some("main"));
        assert!(back.git_dirty);
        // Older daemons omit `git_dirty`; the footer treats it as clean.
        let back: GitInfo = serde_json::from_str(r#"{"git_branch":"feat"}"#).unwrap();
        assert_eq!(back.git_branch.as_deref(), Some("feat"));
        assert!(!back.git_dirty);
    }

    #[test]
    fn daemon_info_defaults_thinking_fields_for_old_daemons() {
        // New client against an old daemon: missing thinking fields default
        // to None instead of failing the `/api/config` parse.
        let back: DaemonInfo = serde_json::from_str(
            r#"{"provider":"opencode","model":"m","api":"openai-responses","available_models":[],"context_window":128000,"permission":"ask-writes","cwd":"/tmp","git_branch":"main","git_dirty":false}"#,
        )
        .unwrap();
        assert!(back.thinking_effort.is_none());
        assert!(back.thinking_warning.is_none());
    }

    #[test]
    fn shell_request_response_round_trip() {
        let req = ShellRequest {
            command: "ls -la".into(),
            exclude_from_context: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: ShellRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.command, "ls -la");
        assert!(!back.exclude_from_context);
        // Old clients omit the flag; it defaults to feeding the next turn.
        let back: ShellRequest = serde_json::from_str(r#"{"command":"x"}"#).unwrap();
        assert!(!back.exclude_from_context);
        let resp = ShellResponse {
            output: "ok".into(),
            success: true,
            code: Some(0),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: ShellResponse = serde_json::from_str(&json).unwrap();
        assert!(back.success);
        assert_eq!(back.code, Some(0));
    }

    #[test]
    fn stream_event_round_trips_through_json() {
        let events = vec![
            StreamEvent::AssistantText("hello".into()),
            StreamEvent::Thinking("step".into()),
            StreamEvent::ToolCall {
                name: "read".into(),
                args: serde_json::json!({"path":"a.rs"}),
                id: "call-1".into(),
            },
            StreamEvent::ToolResult {
                name: "read".into(),
                summary: "ok".into(),
                success: true,
                preview: vec!["line".into()],
                duration: 0.1,
                id: "call-1".into(),
            },
            StreamEvent::TurnComplete {
                response: "done".into(),
                usage: Some(42),
                cached: None,
            },
            StreamEvent::TurnFailed {
                error: "oops".into(),
            },
            StreamEvent::System("sys".into()),
            StreamEvent::Error("err".into()),
            StreamEvent::Usage {
                tokens: 10,
                cached: Some(2),
                cost: 0.0003,
                output: 4,
                gen_ms: Some(1234),
            },
            StreamEvent::SteeringAccepted {
                content: "steer".into(),
            },
            StreamEvent::FollowupAccepted {
                content: "follow".into(),
            },
        ];
        for ev in events {
            let json = serde_json::to_string(&ev).unwrap();
            let back: StreamEvent = serde_json::from_str(&json).unwrap();
            // re-serializing should be stable
            assert_eq!(serde_json::to_string(&back).unwrap(), json);
        }
    }

    #[test]
    fn tool_events_without_ids_parse_as_legacy() {
        // Journals written before ids existed omit the field; they must
        // still parse (pairing falls back to tail order), and re-serializing
        // must not corrupt the event.
        let call: StreamEvent =
            serde_json::from_str(r#"{"type":"tool_call","data":{"name":"read","args":"a.rs"}}"#)
                .unwrap();
        assert!(matches!(call, StreamEvent::ToolCall { ref id, .. } if id.is_empty()));
        let result: StreamEvent = serde_json::from_str(
            r#"{"type":"tool_result","data":{"name":"read","summary":"ok","success":true}}"#,
        )
        .unwrap();
        assert!(matches!(result, StreamEvent::ToolResult { ref id, .. } if id.is_empty()));
    }

    #[test]
    fn stream_envelope_preserves_seq() {
        let env = StreamEnvelope {
            seq: 99,
            event: StreamEvent::System("hi".into()),
        };
        let json = serde_json::to_string(&env).unwrap();
        let back: StreamEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 99);
    }

    #[test]
    fn approval_decision_snake_case() {
        assert_eq!(
            serde_json::to_string(&ApprovalDecision::AllowOnce).unwrap(),
            "\"allow_once\""
        );
        assert_eq!(
            serde_json::to_string(&ApprovalDecision::AllowSession).unwrap(),
            "\"allow_session\""
        );
        assert_eq!(
            serde_json::to_string(&ApprovalDecision::Deny).unwrap(),
            "\"deny\""
        );
    }

    #[test]
    fn question_required_round_trips_and_defaults_agent() {
        let event = StreamEvent::QuestionRequired {
            request_id: "q-1".into(),
            questions: vec![Question {
                question: "Which database?".into(),
                header: "Database".into(),
                options: vec![
                    QuestionOption {
                        label: "postgres".into(),
                        description: "default".into(),
                    },
                    QuestionOption {
                        label: "sqlite".into(),
                        description: "embedded".into(),
                    },
                ],
                multi_select: false,
                default: Some(0),
            }],
            agent: Some("explorer".into()),
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"question_required\""));
        assert!(json.contains("multi_select"));
        let back: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::to_string(&back).unwrap(), json);
        // Old-client parse path: a journal/event without `agent` still parses.
        let bare: StreamEvent = serde_json::from_str(
            r#"{"type":"question_required","data":{"request_id":"q-1","questions":[]}}"#,
        )
        .unwrap();
        match bare {
            StreamEvent::QuestionRequired {
                agent, questions, ..
            } => {
                assert_eq!(agent, None);
                assert!(questions.is_empty());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn question_answer_round_trips_all_variants() {
        for answer in [
            QuestionAnswer::Choice(1),
            QuestionAnswer::Multi(vec![0, 2]),
            QuestionAnswer::Text("sqlite, actually".into()),
            QuestionAnswer::Dismiss,
        ] {
            let json = serde_json::to_string(&answer).unwrap();
            let back: QuestionAnswer = serde_json::from_str(&json).unwrap();
            assert_eq!(back, answer);
            assert_eq!(serde_json::to_string(&back).unwrap(), json);
        }
        assert_eq!(
            serde_json::to_string(&QuestionAnswer::Choice(0)).unwrap(),
            r#"{"type":"choice","data":0}"#
        );
        assert_eq!(
            serde_json::to_string(&QuestionAnswer::Dismiss).unwrap(),
            r#"{"type":"dismiss"}"#
        );
        // Audit spellings share one source with the wire kinds.
        assert_eq!(QuestionAnswer::Choice(3).as_str(), "choice");
        assert_eq!(QuestionAnswer::Multi(vec![]).as_str(), "multi");
        assert_eq!(QuestionAnswer::Text("t".into()).as_str(), "text");
        assert_eq!(QuestionAnswer::Dismiss.as_str(), "dismiss");
    }

    #[test]
    fn question_response_round_trips() {
        let req = QuestionResponse {
            request_id: "q-1".into(),
            answers: vec![QuestionAnswer::Choice(1), QuestionAnswer::Dismiss],
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: QuestionResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.request_id, "q-1");
        assert_eq!(back.answers.len(), 2);
    }

    #[test]
    fn chat_request_defaults_missing_fields() {
        let req: ChatRequest = serde_json::from_str(r#"{"prompt":"hi"}"#).unwrap();
        assert!(req.skill_dirs.is_empty());
        assert!(req.base_url.is_none());
        assert!(req.permission.is_none());
        assert!(req.headers.is_none());
        assert!(req.thinking_effort.is_none());
    }

    #[test]
    fn plan_event_carries_no_budget() {
        let ev = StreamEvent::Plan {
            goal: Some("g".into()),
            steps: vec![("s".into(), false)],
            constraints: vec!["c".into()],
            acceptance: vec![],
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: StreamEvent = serde_json::from_str(&json).unwrap();
        match back {
            StreamEvent::Plan { goal, .. } => assert_eq!(goal.as_deref(), Some("g")),
            _ => panic!("wrong variant"),
        }
    }
}
