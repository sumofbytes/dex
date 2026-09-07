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
        StreamEvent::ApprovalRequired { name, input, .. } => {
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
    }
    None
}

/// One-shot mode: send a single prompt and print the response.
pub(crate) fn one_shot(
    client: &DaemonClient,
    prompt: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let session = client.create_session(&cwd, None)?;

    eprintln!("session: {}", session.session_id);

    client.chat(
        &session.session_id,
        prompt,
        ChatOptions::default(),
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
    println!("Type your prompt and press Enter. Ctrl+C to quit.\n");

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
