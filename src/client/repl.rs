use std::io::{self, IsTerminal, Write};

use crate::core::console::{AGENT_COLOR, RESET};
use crate::core::format::agent_lifecycle;
use crate::protocol::{ApprovalDecision, StreamEvent};

use super::http::{ChatOptions, DaemonClient};

fn prompt_for_approval(name: &str, input: &str) -> ApprovalDecision {
    use crate::core::format::{approval_details, approval_summary, approval_title};
    let title = approval_title(name, input);
    let summary = approval_summary(name, input);
    let details = approval_details(name, input);
    eprintln!();
    eprintln!("  ┌─ Approval required ─────────────────────────────────");
    eprintln!("  │ {} — {}", title, name);
    eprintln!("  │ {}", summary);
    for line in details.iter().take(6) {
        // keep raw JSON out of sight; show the human lines
        if line == &summary {
            continue;
        }
        eprintln!("  │ {}", line);
    }
    eprintln!("  └──────────────────────────────────────────────────────");
    eprint!("  [y] allow once  [s] allow for session  [n] deny > ");
    io::stderr().flush().ok();
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).ok();
    approval_answer(&answer)
}

/// Map a raw answer line to an approval decision: `y`/`yes` allow once,
/// `s`/`session` allow for the session, anything else (blank, `n`, junk)
/// denies — fail closed.
fn approval_answer(answer: &str) -> ApprovalDecision {
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalDecision::AllowOnce,
        "s" | "session" => ApprovalDecision::AllowSession,
        _ => ApprovalDecision::Deny,
    }
}

fn handle_event(event: StreamEvent) -> Option<ApprovalDecision> {
    handle_event_with(event, &mut prompt_for_approval)
}

/// The event renderer with the approval decision injected: production uses
/// the stdin prompt; tests inject a canned decision.
fn handle_event_with(
    event: StreamEvent,
    decide: &mut dyn FnMut(&str, &str) -> ApprovalDecision,
) -> Option<ApprovalDecision> {
    match event {
        StreamEvent::AssistantText(text) => {
            print!("{text}");
            io::stdout().flush().ok();
        }
        StreamEvent::Thinking(_) => {}
        StreamEvent::ToolCall { name, .. } => {
            eprintln!("\n  > {name}...");
        }
        StreamEvent::ToolResult {
            name,
            summary,
            success,
            preview,
            duration,
        } => {
            let icon = if success { "✓" } else { "✗" };
            let timing = if duration > 0.0 {
                format!(" ({})", crate::core::format::format_duration(duration))
            } else {
                String::new()
            };
            eprintln!("  {icon} {name}: {summary}{timing}");
            for line in preview {
                eprintln!("      {line}");
            }
        }
        StreamEvent::ApprovalRequired {
            name, input, agent, ..
        } => {
            // A child agent's request is labeled (V1b); the CLI still
            // answers it like any other parked approval.
            if let Some(agent) = agent {
                eprintln!("  [{agent}] requests {name}");
            }
            return Some(decide(&name, &input));
        }
        StreamEvent::TurnFailed { error } => {
            eprintln!("\nerror: {error}");
        }
        StreamEvent::System(msg) => {
            // Child-agent lifecycle lines get their own colored marker so a
            // delegation pops out of the muted `[system]` notes, matching
            // the TUI's `◈`/`◇` glyphs. ANSI only on a real terminal; piped
            // output stays plain like every other REPL line.
            if let Some((marker, rest)) = agent_lifecycle(&msg) {
                if io::stdout().is_terminal() {
                    eprintln!("{AGENT_COLOR}{marker} [agent{rest}]{RESET}");
                } else {
                    eprintln!("[agent {rest}]");
                }
            } else {
                eprintln!("[system] {msg}");
            }
        }
        StreamEvent::Error(msg) => {
            eprintln!("[error] {msg}");
        }
        StreamEvent::TurnComplete { .. } => {}
        StreamEvent::Usage { .. } => {}
        StreamEvent::Plan { .. } => {}
        StreamEvent::SteeringAccepted { content } => {
            eprintln!("[steer] {content}");
        }
        StreamEvent::FollowupAccepted { content } => {
            eprintln!("[follow-up] {content}");
        }
        // Child-agent lifecycle (V1b): one line per transition, mirroring
        // the V1a System lines without parsing text.
        StreamEvent::AgentSpawned { agent_id, name } => {
            eprintln!("[agent {name}:{agent_id}] started");
        }
        StreamEvent::AgentProgress {
            agent_id,
            current_tool,
            ..
        } => {
            if let Some(tool) = current_tool {
                eprintln!("[agent …:{agent_id}] running {tool} …");
            }
        }
        StreamEvent::AgentCompleted { agent_id, status } => {
            eprintln!("[agent …:{agent_id}] {status}");
        }
    }
    None
}

/// One-shot mode: `!<command>` runs a shell command directly on the daemon
/// (`!` feeds the next turn, `!!` stays out of model context);
/// anything else sends a single prompt and prints the response. A bare
/// `!`/`!!` falls through to the agent. `options` carries the CLI
/// flag overrides (`--model`, `-H`, `--skill`, …) so
/// `dex connect <url> "prompt"` keeps flag parity with the TUI.
pub(crate) fn one_shot(
    client: &DaemonClient,
    prompt: &str,
    options: &ChatOptions,
    session_name: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    // `!`/`!!` shell escape: run directly on the daemon, no agent
    // turn. The daemon saves the run to the new session's history.
    if let Some((command, excluded)) = crate::protocol::parse_shell_escape(prompt.trim()) {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let session = client.create_session(&cwd, session_name)?;
        eprintln!("session: {}", session.session_id);
        let resp = client.shell(&session.session_id, &command, excluded)?;
        if resp.success {
            print!("{}", resp.output);
            if !resp.output.ends_with('\n') {
                println!();
            }
            Ok(())
        } else {
            Err(resp.output.into())
        }
    } else {
        one_shot_chat(client, prompt, options, session_name)
    }
}

/// One-shot mode: send a single prompt and print the response. `options`
/// carries the CLI flag overrides (`--model`, `-H`, `--skill`, …) so
/// `dex connect <url> "prompt"` keeps flag parity with the TUI.
fn one_shot_chat(
    client: &DaemonClient,
    prompt: &str,
    options: &ChatOptions,
    session_name: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let session = client.create_session(&cwd, session_name)?;

    eprintln!("session: {}", session.session_id);

    client.chat(
        &session.session_id,
        prompt,
        options.clone(),
        &mut handle_event,
    )?;

    println!();
    Ok(())
}

/// Interactive REPL mode.
pub(crate) fn run_repl(client: &DaemonClient) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let session = client.create_session(&cwd, None)?;

    println!("Connected to daemon. Session: {}", session.session_id);
    println!("Type your prompt and press Enter. Ctrl+C to quit.");
    println!("Prefix with ! to run shell directly (!! keeps it out of model context).\n");

    let stdin = io::stdin();

    loop {
        print!("> ");
        io::stdout().flush()?;

        let mut input = String::new();
        match stdin.read_line(&mut input) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("read error: {e}");
                break;
            }
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if input == "/quit" || input == "/exit" {
            break;
        }
        if input == "/sessions" {
            match client.list_sessions() {
                Ok(sessions) => {
                    for s in &sessions {
                        println!(
                            "  {} {} ({})",
                            s.session_id,
                            s.name.as_deref().unwrap_or("(unnamed)"),
                            s.cwd,
                        );
                    }
                }
                Err(e) => eprintln!("error listing sessions: {e}"),
            }
            continue;
        }
        if input == "/cancel" {
            if let Err(e) = client.cancel(&session.session_id) {
                eprintln!("error cancelling: {e}");
            } else {
                println!("cancel sent");
            }
            continue;
        }
        // `!`/`!!` shell escape: run directly, no agent turn. A
        // bare `!`/`!!` falls through to the agent.
        if let Some((command, excluded)) = crate::protocol::parse_shell_escape(input) {
            match client.shell(&session.session_id, &command, excluded) {
                Ok(resp) => {
                    if resp.success {
                        print!("{}", resp.output);
                        if !resp.output.ends_with('\n') {
                            println!();
                        }
                    } else {
                        eprintln!("{}", resp.output);
                    }
                }
                Err(e) => eprintln!("error: {e}"),
            }
            println!();
            continue;
        }

        if let Err(e) = client.chat(
            &session.session_id,
            input,
            ChatOptions::default(),
            &mut handle_event,
        ) {
            eprintln!("error: {e}");
        }

        println!();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
            },
            StreamEvent::ToolResult {
                name: "bash".into(),
                summary: "ran".into(),
                success: true,
                preview: vec!["out".into()],
                duration: 1.5,
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
            assert_eq!(handle_event_with(event, &mut decide), None);
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
        assert_eq!(decision, Some(ApprovalDecision::AllowSession));
        assert_eq!(
            calls.borrow().as_slice(),
            vec![("write".to_string(), r#"{"path":"x"}"#.to_string())]
        );
    }

    /// `!`/`!!` one-shots run directly on the daemon — no LLM involved, so a
    /// plain daemon spawn is enough.
    #[test]
    fn one_shot_shell_escape_runs_on_the_daemon() {
        use std::time::Duration;
        let _lock = crate::session::TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let data_dir = std::env::temp_dir().join(format!("dex-repl-shell-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data_dir);
        let saved: Vec<(&str, Option<std::ffi::OsString>)> =
            [("XDG_DATA_HOME", std::env::var_os("XDG_DATA_HOME"))]
                .into_iter()
                .collect();
        let _env = crate::session::EnvGuard(saved);
        std::env::set_var("XDG_DATA_HOME", &data_dir);

        // The real daemon router (no LLM involved).
        let daemon_base = crate::client::http::tests::spawn_daemon_sync(
            crate::daemon::server::router(std::sync::Arc::new(crate::daemon::DaemonState::new())),
        );

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
}
