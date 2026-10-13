//! Program status for the host terminal (OSC 7501, mitchellh.com/writing/program-status-osc7501).
//!
//! `ESC ] 7501 ; state=…[:kind=…]:app=dex[:msg=<base64>] ST` on the TUI's own
//! tty. Terminals ignore unknown OSCs, so it is emitted unconditionally and
//! only on a state or message change.

use super::selection::b64;
use crossterm::Command;
use std::fmt;

#[derive(Clone, PartialEq, Eq, Debug)]
enum Status {
    Idle,
    Working,
    /// Waiting on a tool approval for the named tool.
    Blocked(String),
}

impl Command for Status {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        match self {
            Status::Idle => write!(f, "\x1b]7501;state=idle:app=dex\x1b\\"),
            Status::Working => write!(f, "\x1b]7501;state=working:app=dex\x1b\\"),
            Status::Blocked(tool) => {
                let msg: String = format!("awaiting {tool} approval")
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(190)
                    .collect();
                write!(
                    f,
                    "\x1b]7501;state=blocked:kind=permission:app=dex:msg={}\x1b\\",
                    b64(msg.as_bytes())
                )
            }
        }
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// `ESC ] 7501 ; state=clear ST`: drop dex's root record when it leaves.
struct Clear;

impl Command for Clear {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b]7501;state=clear\x1b\\")
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) struct Reporter {
    last: Option<Status>,
}

impl Reporter {
    pub(super) fn new() -> Self {
        Self { last: None }
    }

    /// Called from the TUI loop; writes only when the status changed.
    pub(super) fn sync(&mut self, busy: bool, approval: Option<&str>) {
        let status = match approval {
            Some(tool) => Status::Blocked(tool.to_string()),
            None if busy => Status::Working,
            None => Status::Idle,
        };
        if self.last.as_ref() == Some(&status) {
            return;
        }
        let _ = crossterm::execute!(std::io::stdout(), status.clone());
        self.last = Some(status);
    }

    pub(super) fn release(&mut self) {
        if self.last.take().is_some() {
            let _ = crossterm::execute!(std::io::stdout(), Clear);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ansi(c: &impl Command) -> String {
        let mut s = String::new();
        c.write_ansi(&mut s).unwrap();
        s
    }

    #[test]
    fn sequences() {
        assert_eq!(
            ansi(&Status::Working),
            "\x1b]7501;state=working:app=dex\x1b\\"
        );
        assert_eq!(ansi(&Clear), "\x1b]7501;state=clear\x1b\\");
        let blocked = ansi(&Status::Blocked("bash\n\x1b".into()));
        assert!(blocked.starts_with("\x1b]7501;state=blocked:kind=permission:app=dex:msg="));
        assert!(blocked.contains(&b64(b"awaiting bash approval")));
    }
}
