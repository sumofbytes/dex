use std::io::{self, IsTerminal, Write};

use crate::protocol::{ApprovalDecision, StreamEvent};
use crate::runtime::console::{AGENT_COLOR, RESET};
use crate::runtime::format_runtime::agent_lifecycle;

use super::http::{ChatOptions, DaemonClient};

fn prompt_for_approval(name: &str, input: &str) -> ApprovalDecision {
    use crate::render::format::{approval_details, approval_summary, approval_title};
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
        StreamEvent::ToolCall { name, args, .. } => {
            // The daemon ships the same short-arg preview the local path
            // prints (`read src/main.rs`) — show it, not just the tool name.
            let arg = args.as_str().unwrap_or_default();
            if arg.is_empty() {
                eprintln!("\n  > {name}...");
            } else {
                eprintln!("\n  > {name} {arg}...");
            }
        }
        StreamEvent::ToolResult {
            name,
            summary,
            success,
            preview,
            duration,
            ..
        } => {
            let icon = if success { "✓" } else { "✗" };
            let timing = if duration > 0.0 {
                format!(" ({})", crate::render::format::format_duration(duration))
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
    if let Some((command, excluded)) = crate::tools::parse_shell_escape(prompt.trim()) {
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

#[cfg(test)]
mod tests;
