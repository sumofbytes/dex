use std::io::{self, Write};

use crate::protocol::{ApprovalDecision, StreamEvent};

use super::http::{ChatOptions, DaemonClient};

fn prompt_for_approval(name: &str, input: &str) -> ApprovalDecision {
    use crate::core::format::{approval_details, approval_summary, approval_title};
    let title = approval_title(name);
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
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalDecision::AllowOnce,
        "s" | "session" => ApprovalDecision::AllowSession,
        _ => ApprovalDecision::Deny,
    }
}

fn handle_event(event: StreamEvent) -> Option<ApprovalDecision> {
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
            return Some(prompt_for_approval(&name, &input));
        }
        StreamEvent::TurnFailed { error } => {
            eprintln!("\nerror: {error}");
        }
        StreamEvent::System(msg) => {
            eprintln!("[system] {msg}");
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
