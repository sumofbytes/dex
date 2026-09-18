use crate::runtime::console::DIM;
use crate::runtime::console::RESET;
use std::io::{self, IsTerminal};

/// Host part of a daemon URL with any userinfo/port/path stripped; a bracketed
/// IPv6 literal keeps its brackets.
fn url_host(daemon_url: &str) -> &str {
    let authority = daemon_url
        .split_once("://")
        .map_or(daemon_url, |(_, rest)| rest);
    // Authority = [userinfo@]host[:port]; stop at the first path/query char.
    let authority = authority.split(['/', '?', '#']).next().unwrap_or("");
    let authority = match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    };
    // Bracketed IPv6 literals: everything through `]` is the host, the rest
    // (if any) is the port. Otherwise a trailing `:digits` is a port.
    if let Some(close) = authority.find(']') {
        &authority[..=close]
    } else {
        match authority.rsplit_once(':') {
            Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
            _ => authority,
        }
    }
}

/// How the engine is reached: loopback daemons are "local", everything else
/// is reported by host. Used by the status bar instead of a startup banner.
pub(crate) fn connection_label(daemon_url: &str) -> String {
    let host = url_host(daemon_url);
    if is_loopback(host) {
        format!("[L] {host}")
    } else {
        format!("[R] {host}")
    }
}

fn is_loopback(host: &str) -> bool {
    if matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") {
        return true;
    }
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let octets: Vec<_> = bare.split('.').collect();
    octets.len() == 4 && octets[0] == "127" && octets[1..].iter().all(|o| o.parse::<u8>().is_ok())
}

/// True when both strings name the same directory. Best-effort: a path that no
/// longer exists (deleted checkout) falls back to a literal comparison.
pub(crate) fn same_workspace(left: &str, right: &str) -> bool {
    let canon = |p: &str| std::fs::canonicalize(p).unwrap_or_else(|_| std::path::PathBuf::from(p));
    canon(left) == canon(right)
}

/// Quote a value for the shell only when it needs it, so the common case stays
/// copy-pasteable (`--reattach sess-1`, not `'sess-1'`).
pub(crate) fn shell_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '/' | '.' | ':' | '@' | '_' | '-' | '+' | '=' | ',' | '~')
        });
    if safe {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// Command that brings the user back to this session.
///
/// Locality comes from how the client was invoked, never from the URL host: a
/// loopback URL is still remote when it is an SSH port-forward or a daemon in
/// another container, and a bare local `dex --reattach` against it would 404.
///
/// A locally owned daemon resolves its workspace from its own cwd, so when the
/// session came from a different directory the command has to `cd` back first —
/// the id carries the workspace *name*, never its path.
pub(crate) fn resume_command(
    daemon_url: &str,
    session_id: &str,
    session_cwd: &str,
    daemon_is_local: bool,
) -> String {
    let session_id = shell_quote(session_id);
    if !daemon_is_local {
        return format!(
            "dex connect {} --reattach {session_id}",
            shell_quote(daemon_url)
        );
    }
    let client_cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    if same_workspace(session_cwd, &client_cwd) {
        format!("dex --reattach {session_id}")
    } else {
        format!(
            "cd {} && dex --reattach {session_id}",
            shell_quote(session_cwd)
        )
    }
}

/// Pure formatter for the resume hint: label on one line, command on the next
/// so the command is triple-click copyable. Label-only dim keeps the command
/// bright; redirected output stays plain so each line stays greppable.
pub(crate) fn format_resume_hint(command: &str, styled: bool) -> String {
    if styled {
        format!("\n{DIM}To resume this session:{RESET}\n{command}")
    } else {
        format!("\nTo resume this session:\n{command}")
    }
}

/// Show the resume command after the alternate screen is gone so it lands in
/// the shell's scrollback next to the prompt. Must be the last write to the
/// terminal: any escape sequence after it (a stray `CSI ?1049l`) restores the
/// cursor onto this line and the shell's next prompt overwrites it. The one
/// exception is the SGR pair around the label, which neither moves the cursor
/// nor ends the line — and no styling after the command keeps the shell's
/// next prompt in its own colors.
pub(crate) fn print_resume_hint(
    daemon_url: &str,
    session_id: &str,
    session_cwd: &str,
    daemon_is_local: bool,
) {
    let command = resume_command(daemon_url, session_id, session_cwd, daemon_is_local);
    // Dim only on a terminal: redirected stderr should stay greppable.
    eprintln!(
        "{}",
        format_resume_hint(&command, io::stderr().is_terminal())
    );
}
