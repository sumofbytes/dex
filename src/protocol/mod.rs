use serde::{Deserialize, Serialize};

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

/// Split a `!`/`!!` shell escape into `(command, exclude_from_context)`.
/// `!cmd` feeds the next turn; `!!cmd` stays out of the LLM context. Returns
/// `None` when the line isn't a shell escape — including a bare `!`/`!!`,
/// which falls through to the agent instead of erroring. Everything
/// after the prefix is the command, newlines included.
pub fn parse_shell_escape(line: &str) -> Option<(String, bool)> {
    let (rest, excluded) = match line.strip_prefix("!!") {
        Some(rest) => (rest, true),
        None => (line.strip_prefix('!')?, false),
    };
    let command = rest.trim().to_string();
    if command.is_empty() {
        None
    } else {
        Some((command, excluded))
    }
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
    /// Extra HTTP headers for the provider request (client `--header`
    /// flags). Merged over the daemon's own configured headers.
    #[serde(default)]
    pub headers: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    pub plan: Option<String>,
}

/// Response to approve/deny a tool execution. `request_id` must match the
/// `ApprovalRequired` stream event the decision resolves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub request_id: String,
    pub decision: ApprovalDecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    AllowSession,
    Deny,
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
    },

    /// The agent needs user approval for a tool.
    #[serde(rename = "approval_required")]
    ApprovalRequired {
        request_id: String,
        name: String,
        input: String,
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
    fn shell_escape_splits_command() {
        assert_eq!(
            parse_shell_escape("!ls -la"),
            Some(("ls -la".to_string(), false))
        );
        assert_eq!(
            parse_shell_escape("!  echo hi  "),
            Some(("echo hi".to_string(), false))
        );
        assert_eq!(
            parse_shell_escape("!!cargo test"),
            Some(("cargo test".to_string(), true))
        );
        assert_eq!(
            parse_shell_escape("!!  echo hi  "),
            Some(("echo hi".to_string(), true))
        );
        // Multiline scripts run whole.
        assert_eq!(
            parse_shell_escape("!echo a\necho b"),
            Some(("echo a\necho b".to_string(), false))
        );
        // Bare `!`/`!!` fall through to the agent (usage, not a run).
        assert_eq!(parse_shell_escape("!"), None);
        assert_eq!(parse_shell_escape("!   "), None);
        assert_eq!(parse_shell_escape("!!"), None);
        // Ordinary prompts and slash commands are not shell escapes.
        assert_eq!(parse_shell_escape("hello"), None);
        assert_eq!(parse_shell_escape("/model foo"), None);
        assert_eq!(parse_shell_escape(""), None);
    }

    #[test]
    fn stream_event_round_trips_through_json() {
        let events = vec![
            StreamEvent::AssistantText("hello".into()),
            StreamEvent::Thinking("step".into()),
            StreamEvent::ToolCall {
                name: "read".into(),
                args: serde_json::json!({"path":"a.rs"}),
            },
            StreamEvent::ToolResult {
                name: "read".into(),
                summary: "ok".into(),
                success: true,
                preview: vec!["line".into()],
                duration: 0.1,
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
    fn chat_request_defaults_missing_fields() {
        let req: ChatRequest = serde_json::from_str(r#"{"prompt":"hi"}"#).unwrap();
        assert!(req.skill_dirs.is_empty());
        assert!(req.base_url.is_none());
        assert!(req.permission.is_none());
        assert!(req.headers.is_none());
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
