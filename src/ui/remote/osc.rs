use crossterm::event;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;
use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

/// How long to hold a suspicious char run while waiting for the rest of an
/// OSC color report before giving up and replaying it as real input.
/// Short poll for the next key in the burst, total window for a chunked
/// terminal reply (some ttys split `\x1b]11;rgb:…\x07` across writes).
const OSC_LOOKAHEAD: Duration = Duration::from_millis(35);
const OSC_TOTAL_TIMEOUT: Duration = Duration::from_millis(150);

/// TUI startup instant, so swallowed reports can be attributed: uptime near
/// zero means our own startup theme query (reply arrived late); a large
/// uptime means something else queried this tty mid-session.
pub(crate) static OSC_START: OnceLock<Instant> = OnceLock::new();

/// XDG cache directory (`$XDG_CACHE_HOME`, else `$HOME/.cache`); the macOS
/// layout matches the `dirs` crate. Replaces the `dirs` dep (one call site).
pub(crate) fn cache_dir() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Library/Caches"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var_os("XDG_CACHE_HOME")
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache"))
            })
    }
}

/// Best-effort diagnostic journal of OSC runs caught by `strip_osc_report`,
/// appended to `~/.cache/dex/osc.log`. The reply carries no sender identity,
/// so the log records when + how late + what; failures are ignored.
fn log_osc(kind: &str, body: &str) {
    use std::io::Write;
    let Some(dir) = cache_dir() else { return };
    let path = dir.join("dex/osc.log");
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    let up = OSC_START.get_or_init(Instant::now).elapsed().as_secs_f64();
    let body: String = body.chars().take(80).collect();
    let _ = writeln!(f, "{ts} uptime={up:.1}s {kind} body={body}");
}

/// Swallow OSC 10/11 color reports that crossterm mis-parses as keystrokes.
/// Crossterm has no OSC parsing: `\x1b]11;rgb:0505/1818/2e2e` + BEL arrives
/// as `Alt+']'`, then one plain-char key per body byte, then `Ctrl+G` (BEL)
/// or `Alt+'\'` (ST). Such reports come from the startup theme query and
/// from anything else querying this tty (the terminal itself does), at any
/// time; unfiltered they type `10;rgb:f6f6/dcdc/acac…` into the composer.
/// Returns `None` when the run was swallowed; anything that doesn't fit the
/// report shape is replayed untouched, so real input is never dropped —
/// worst case it is delayed by one look-ahead window.
pub(crate) fn strip_osc_report(
    ev: Event,
    pending: &mut VecDeque<Event>,
) -> std::io::Result<Option<Event>> {
    let lead_in = |e: &Event| {
        matches!(
            e,
            Event::Key(k) if k.code == KeyCode::Char(']') && k.modifiers == KeyModifiers::ALT
        )
    };
    if !lead_in(&ev) {
        return Ok(Some(ev));
    }
    let terminator = |k: &crossterm::event::KeyEvent| {
        (k.code == KeyCode::Char('g') && k.modifiers == KeyModifiers::CONTROL) // BEL
            || (k.code == KeyCode::Char('\\') && k.modifiers == KeyModifiers::ALT)
        // ST
    };

    // Consume the run: plain chars accumulate into `body`, until a report
    // terminator, the next report's lead-in, any other key, or a gap.
    // A chunked tty can split `\x1b]11;rgb:…\x07` across writes with a short
    // gap; if the prefix so far could still become a valid report, keep
    // waiting up to the total window instead of replaying the fragment.
    let mut run: Vec<Event> = vec![ev];
    let mut body = String::new();
    let start = Instant::now();
    loop {
        let elapsed = start.elapsed();
        if elapsed >= OSC_TOTAL_TIMEOUT {
            break;
        }
        let remaining = OSC_TOTAL_TIMEOUT - elapsed;
        let short = remaining.min(OSC_LOOKAHEAD);
        let next = match pending.pop_front() {
            Some(e) => e,
            None if event::poll(short)? => event::read()?,
            None => {
                if is_osc_prefix(&body) && !body.is_empty() {
                    continue;
                }
                break;
            }
        };
        let (char_of, ends) = match &next {
            Event::Key(k) if k.modifiers.is_empty() || k.modifiers == KeyModifiers::SHIFT => {
                match k.code {
                    KeyCode::Char(c) => (Some(c), false),
                    _ => (None, true),
                }
            }
            Event::Key(k) if terminator(k) => (None, true),
            e if lead_in(e) => (None, true),
            _ => (None, true),
        };
        if let Some(c) = char_of {
            body.push(c);
        }
        run.push(next);
        if ends {
            break;
        }
    }

    // A back-to-back next report begins with its own lead-in; reclassify it.
    let next_lead_in = if run.len() > 1 && lead_in(run.last().unwrap()) {
        run.pop()
    } else {
        None
    };

    if is_osc_report(&body) {
        log_osc("swallowed", &body);
        if let Some(lead) = next_lead_in {
            pending.push_front(lead);
        }
        return Ok(None);
    }
    // Not a report after all (or the burst was split beyond the look-ahead —
    // ponytail: 25ms; a tty that chunks reply writes slower than that would
    // leak the tail): replay everything in order. The lead-in itself is
    // returned for dispatch (Alt+']' inserts nothing) so a replayed run can
    // never re-enter this filter and loop.
    log_osc("replayed", &body);
    let lead_in_ev = run.remove(0);
    for e in run.into_iter().rev() {
        pending.push_front(e);
    }
    Ok(Some(lead_in_ev))
}

/// Body grammar of an OSC 10/11 color report: `10;rgb:` / `11;rgb:` plus at
/// least three `/`-separated hex components (16-bit or truncated).
pub(crate) fn is_osc_report(body: &str) -> bool {
    let rest = body
        .strip_prefix("10;rgb:")
        .or_else(|| body.strip_prefix("11;rgb:"))
        .unwrap_or("");
    !rest.is_empty()
        && rest.split('/').count() >= 3
        && rest.chars().all(|c| c.is_ascii_hexdigit() || c == '/')
}

fn is_osc_prefix(body: &str) -> bool {
    if body.is_empty() {
        return true;
    }
    if "10;rgb:".starts_with(body) || "11;rgb:".starts_with(body) {
        return true;
    }
    if let Some(rest) = body
        .strip_prefix("10;rgb:")
        .or_else(|| body.strip_prefix("11;rgb:"))
    {
        return rest.chars().all(|c| c.is_ascii_hexdigit() || c == '/');
    }
    matches!(body, "1" | "10" | "11" | "10;" | "11;")
}
