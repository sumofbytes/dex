//! Tests, split out of the module body so it stays implementation.

use super::*;
use crate::client::e2e_tests::spawn_daemon_sync;
use crate::protocol::StreamEvent;

#[test]
fn approval_answers_fail_closed() {
    use crate::protocol::ApprovalDecision as D;
    assert_eq!(approval_answer("y"), D::AllowOnce);
    assert_eq!(approval_answer("  YES \n"), D::AllowOnce);
    assert_eq!(approval_answer("s"), D::AllowSession);
    assert_eq!(approval_answer("Session"), D::AllowSession);
    // Anything else denies: blank (EOF), n, junk.
    for junk in ["", "   ", "n", "no", "maybe"] {
        assert_eq!(approval_answer(junk), D::Deny, "{junk:?}");
    }
}

#[test]
fn handle_event_prints_and_only_asks_on_approvals() {
    use std::cell::RefCell;
    use std::rc::Rc;
    let calls = Rc::new(RefCell::new(Vec::new()));
    let asked = calls.clone();
    let mut decide = move |name: &str, input: &str| {
        asked
            .borrow_mut()
            .push((name.to_string(), input.to_string()));
        ApprovalDecision::AllowSession
    };
    // Non-approval events render and never ask.
    let events = [
        StreamEvent::AssistantText("hi".into()),
        StreamEvent::Thinking("hmm".into()),
        StreamEvent::ToolCall {
            name: "bash".into(),
            args: "echo".into(),
            id: String::new(),
        },
        StreamEvent::ToolResult {
            name: "bash".into(),
            summary: "ran".into(),
            success: true,
            preview: vec!["out".into()],
            duration: 1.5,
            id: String::new(),
        },
        StreamEvent::TurnFailed {
            error: "boom".into(),
        },
        StreamEvent::System("sys".into()),
        StreamEvent::Error("err".into()),
        StreamEvent::TurnComplete {
            response: "done".into(),
            usage: None,
            cached: None,
        },
        StreamEvent::SteeringAccepted {
            content: "s".into(),
        },
        StreamEvent::FollowupAccepted {
            content: "f".into(),
        },
        StreamEvent::AgentSpawned {
            agent_id: "a-0".into(),
            name: "explorer".into(),
        },
        StreamEvent::AgentProgress {
            agent_id: "a-0".into(),
            state: "running".into(),
            current_tool: Some("read".into()),
        },
        StreamEvent::AgentCompleted {
            agent_id: "a-0".into(),
            status: "complete".into(),
        },
    ];
    for event in events {
        assert_eq!(handle_event_with(event, &mut decide), EventReply::None);
    }
    assert!(
        calls.borrow().is_empty(),
        "no decision asked for plain events"
    );

    // An approval asks, with the child label carried through.
    let decision = handle_event_with(
        StreamEvent::ApprovalRequired {
            request_id: "r1".into(),
            name: "write".into(),
            input: r#"{"path":"x"}"#.into(),
            agent: Some("explorer".into()),
        },
        &mut decide,
    );
    assert_eq!(
        decision,
        EventReply::Decision(ApprovalDecision::AllowSession)
    );
    assert_eq!(
        calls.borrow().as_slice(),
        vec![("write".to_string(), r#"{"path":"x"}"#.to_string())]
    );
}

fn sample_questions() -> Vec<crate::protocol::Question> {
    use crate::protocol::{Question, QuestionOption};
    vec![
        Question {
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
        },
        Question {
            question: "Pick extras".into(),
            header: "Extras".into(),
            options: vec![
                QuestionOption {
                    label: "auth".into(),
                    description: String::new(),
                },
                QuestionOption {
                    label: "telemetry".into(),
                    description: String::new(),
                },
            ],
            multi_select: true,
            default: None,
        },
    ]
}

/// Scripted stdin for `prompt_questions_with`; `None` entries are EOF and
/// an exhausted script is EOF too (the caller only scripts what it needs).
fn script(lines: &[Option<&str>]) -> impl FnMut() -> Option<String> {
    let owned: Vec<Option<String>> = lines
        .iter()
        .map(|l| l.as_ref().map(|s| s.to_string()))
        .collect();
    let mut it = owned.into_iter();
    move || it.next().unwrap_or(None)
}

#[test]
fn headless_questions_eof_dismisses_the_whole_batch() {
    let answers = prompt_questions_with(&sample_questions(), &mut script(&[Some("2")]));
    // The first answer was recorded, but EOF on the second dismisses
    // everything — a half-batch must never be submitted.
    assert_eq!(
        answers,
        vec![QuestionAnswer::Dismiss, QuestionAnswer::Dismiss]
    );
}

#[test]
fn headless_questions_empty_line_honors_default_and_rereprompts() {
    // Empty line on q1 picks the model-set default; empty on q2 (no
    // default) re-prompts, then "1" answers it; the last Enter submits.
    let answers = prompt_questions_with(
        &sample_questions(),
        &mut script(&[Some(""), Some(""), Some("1")]),
    );
    assert_eq!(
        answers,
        vec![QuestionAnswer::Choice(0), QuestionAnswer::Multi(vec![0]),]
    );
}

#[test]
fn headless_questions_dot_skips_and_multi_parses() {
    let answers =
        prompt_questions_with(&sample_questions(), &mut script(&[Some("."), Some("2,1")]));
    assert_eq!(
        answers,
        vec![QuestionAnswer::Dismiss, QuestionAnswer::Multi(vec![1, 0]),]
    );
    // Junk multi answers re-prompt until a valid line arrives.
    let answers = prompt_questions_with(
        &sample_questions(),
        &mut script(&[Some("1"), Some("9"), Some("auth"), Some("1,1"), Some("2")]),
    );
    assert_eq!(
        answers,
        vec![QuestionAnswer::Choice(0), QuestionAnswer::Multi(vec![1])]
    );
}

#[test]
fn headless_questions_other_row_takes_free_text() {
    let answers = prompt_questions_with(
        &sample_questions(),
        &mut script(&[Some("3"), Some("sqlite, on a stick\n"), Some("1")]),
    );
    assert_eq!(
        answers[0],
        QuestionAnswer::Text("sqlite, on a stick\n".to_string())
    );
}

#[test]
fn headless_questions_out_of_range_rereprompts() {
    let answers = prompt_questions_with(
        &sample_questions(),
        &mut script(&[Some("7"), Some("0"), Some("2"), Some("1")]),
    );
    assert_eq!(
        answers,
        vec![QuestionAnswer::Choice(1), QuestionAnswer::Multi(vec![0])]
    );
}

/// `!`/`!!` one-shots run directly on the daemon — no LLM involved, so a
/// plain daemon spawn is enough.
#[test]
fn one_shot_shell_escape_runs_on_the_daemon() {
    use std::time::Duration;
    let _lock = crate::test_env::TEST_SESSIONS_ENV_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let data_dir = std::env::temp_dir().join(format!("dex-repl-shell-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&data_dir);
    let saved: Vec<(&str, Option<std::ffi::OsString>)> =
        [("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]
            .into_iter()
            .collect();
    let _env = crate::test_env::EnvGuard(saved);
    std::env::set_var("XDG_DATA_HOME", &data_dir);

    // The real daemon router (no LLM involved).
    let daemon_base = spawn_daemon_sync(crate::daemon::server::router(std::sync::Arc::new(
        crate::daemon::DaemonState::new(),
    )));

    let client = DaemonClient::new(&daemon_base).unwrap();
    client.wait_until_ready(Duration::from_secs(10)).unwrap();
    // `!` runs and feeds the next turn; `!!` stays out of context.
    one_shot(
        &client,
        "!echo repl-ok",
        &ChatOptions::default(),
        Some("repl"),
    )
    .expect("! runs");
    one_shot(&client, "!!echo quiet", &ChatOptions::default(), None).expect("!! runs");
    // A failing command surfaces as an error.
    assert!(one_shot(&client, "!exit 7", &ChatOptions::default(), None).is_err());

    let _ = std::fs::remove_dir_all(&data_dir);
}
