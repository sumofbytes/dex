//! Program status for the host terminal (OSC 7501, mitchellh.com/writing/program-status-osc7501).
//!
//! `ESC ] 7501 ; state=…[:kind=…]:app=dex[:msg=<base64>] ST` on the TUI's own
//! tty, only on a state or message change. Skipped on terminals that print
//! unknown OSCs (`TERM=linux`/`dumb`); wrapped in tmux's DCS passthrough under
//! tmux (needs `allow-passthrough on`, otherwise tmux drops it silently).

use super::herdr::State;
use super::selection::b64;
use std::io::Write;

/// Byte cap on the decoded `msg` payload.
const MSG_MAX: usize = 190;

fn status_osc(state: State, tool: Option<&str>) -> String {
    let Some(tool) = tool else {
        return format!("\x1b]7501;state={}:app=dex\x1b\\", state.as_str());
    };
    let mut msg: String = super::herdr::approval_message(tool)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    if msg.len() > MSG_MAX {
        let mut end = MSG_MAX;
        while !msg.is_char_boundary(end) {
            end -= 1;
        }
        msg.truncate(end);
    }
    format!(
        "\x1b]7501;state={}:kind=permission:app=dex:msg={}\x1b\\",
        state.as_str(),
        b64(msg.as_bytes())
    )
}

/// `ESC ] 7501 ; state=clear ST`: drop dex's root record when it leaves.
const CLEAR: &str = "\x1b]7501;state=clear\x1b\\";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sink {
    Off,
    Direct,
    Tmux,
}

impl Sink {
    fn detect(term: Option<&str>, tmux: bool) -> Self {
        match term {
            None | Some("" | "dumb" | "linux") => Sink::Off,
            _ if tmux => Sink::Tmux,
            _ => Sink::Direct,
        }
    }

    fn wrap(self, seq: &str) -> Option<String> {
        match self {
            Sink::Off => None,
            Sink::Direct => Some(seq.to_string()),
            Sink::Tmux => Some(format!(
                "\x1bPtmux;{}\x1b\\",
                seq.replace('\x1b', "\x1b\x1b")
            )),
        }
    }
}

pub(super) struct Reporter {
    sink: Sink,
    last: Option<(State, Option<String>)>,
}

impl Reporter {
    pub(super) fn new() -> Self {
        let term = std::env::var("TERM").ok();
        let tmux = std::env::var_os("TMUX").is_some_and(|v| !v.is_empty());
        Self {
            sink: Sink::detect(term.as_deref(), tmux),
            last: None,
        }
    }

    /// Called from the TUI loop with the terminal's own writer, between
    /// frames; writes only when the status changed, and retries next tick
    /// if the write failed.
    pub(super) fn sync(&mut self, out: &mut impl Write, state: State, tool: Option<&str>) {
        if self
            .last
            .as_ref()
            .is_some_and(|(s, t)| *s == state && t.as_deref() == tool)
        {
            return;
        }
        let Some(seq) = self.sink.wrap(&status_osc(state, tool)) else {
            return;
        };
        if out
            .write_all(seq.as_bytes())
            .and_then(|()| out.flush())
            .is_ok()
        {
            self.last = Some((state, tool.map(str::to_string)));
        }
    }

    pub(super) fn release(&mut self) {
        if self.last.take().is_some() {
            if let Some(seq) = self.sink.wrap(CLEAR) {
                let mut out = std::io::stdout();
                let _ = out.write_all(seq.as_bytes()).and_then(|()| out.flush());
            }
        }
    }
}

/// Also clears on a panic unwinding out of the TUI loop, before
/// `TerminalCleanup` restores the screen.
impl Drop for Reporter {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequences() {
        assert_eq!(
            status_osc(State::Working, None),
            "\x1b]7501;state=working:app=dex\x1b\\"
        );
        let blocked = status_osc(State::Blocked, Some("bash\n\x1b"));
        assert_eq!(
            blocked,
            format!(
                "\x1b]7501;state=blocked:kind=permission:app=dex:msg={}\x1b\\",
                b64(b"awaiting bash approval")
            )
        );
    }

    #[test]
    fn msg_capped_in_bytes_on_char_boundary() {
        let tool = "é".repeat(200);
        let seq = status_osc(State::Blocked, Some(&tool));
        let encoded = seq.split("msg=").nth(1).unwrap().trim_end_matches("\x1b\\");
        assert!(
            encoded.len() <= MSG_MAX.div_ceil(3) * 4,
            "{}",
            encoded.len()
        );
    }

    #[test]
    fn sink_detection_and_tmux_wrap() {
        assert_eq!(Sink::detect(Some("linux"), false), Sink::Off);
        assert_eq!(Sink::detect(None, true), Sink::Off);
        assert_eq!(Sink::detect(Some("xterm-ghostty"), false), Sink::Direct);
        assert_eq!(Sink::detect(Some("tmux-256color"), true), Sink::Tmux);
        assert_eq!(
            Sink::Tmux.wrap(CLEAR).unwrap(),
            "\x1bPtmux;\x1b\x1b]7501;state=clear\x1b\x1b\\\x1b\\"
        );
    }

    #[test]
    fn dedupes_and_retries_failed_writes() {
        struct Failing;
        impl Write for Failing {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut r = Reporter {
            sink: Sink::Direct,
            last: None,
        };
        r.sync(&mut Failing, State::Working, None);
        assert!(r.last.is_none());
        let mut buf = Vec::new();
        r.sync(&mut buf, State::Working, None);
        r.sync(&mut buf, State::Working, None);
        assert_eq!(buf, b"\x1b]7501;state=working:app=dex\x1b\\");
        // Don't emit a Clear into the test's stdout.
        r.last = None;
    }
}
