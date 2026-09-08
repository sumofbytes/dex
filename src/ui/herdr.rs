//! Best-effort lifecycle reporting to a Herdr pane (herdr.dev).
//!
//! When dex runs inside a Herdr pane (`HERDR_ENV=1` with `HERDR_PANE_ID` and
//! `HERDR_BIN_PATH` set by Herdr), report state transitions through the herdr
//! CLI so the sidebar shows live `working` / `blocked` / `idle` state without
//! screen scraping. Outside Herdr every call is a cheap no-op, and a missing
//! or stopped herdr server just makes the spawned CLI exit non-zero.

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

const SOURCE: &str = "custom:dex";
const AGENT: &str = "dex";

/// Herdr lifecycle states. dex maps them from TUI state: a running turn is
/// `working`, a pending tool approval is `blocked`, anything else is `idle`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Idle,
    Working,
    Blocked,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Blocked => "blocked",
        }
    }
}

fn target() -> Option<(String, String)> {
    if std::env::var("HERDR_ENV").as_deref() != Ok("1") {
        return None;
    }
    // HERDR_BIN_PATH is the portable way to call the CLI (it points at the
    // server's own binary); see herdr.dev/docs/integrations/.
    let bin = std::env::var("HERDR_BIN_PATH").ok()?;
    let pane = std::env::var("HERDR_PANE_ID").ok()?;
    (!pane.is_empty() && !bin.is_empty()).then_some((pane, bin))
}

/// Herdr ignores stale per-source sequence numbers, so the sequence must
/// increase across dex restarts within one pane: seed from the epoch once
/// per process, then count up.
static SEQ: AtomicU64 = AtomicU64::new(0);

fn next_seq() -> u64 {
    static BASE: OnceLock<u64> = OnceLock::new();
    let base = BASE.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    });
    base + 1 + SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Tracks the last reported state so reports only fire on transitions, and
/// reaps the spawned `herdr` CLI processes (state changes are rare, but a
/// long session still accumulates a handful).
pub(super) struct Reporter {
    last: Option<(State, Option<String>)>,
    children: Vec<Child>,
}

impl Reporter {
    pub(super) fn new() -> Self {
        Self {
            last: None,
            children: Vec::new(),
        }
    }

    /// Called from the TUI loop (a few times a second); reports only on a
    /// state or message change.
    pub(super) fn sync(&mut self, busy: bool, approval: Option<&str>) {
        self.reap();
        let state = if approval.is_some() {
            State::Blocked
        } else if busy {
            State::Working
        } else {
            State::Idle
        };
        let message = approval.map(|tool| format!("awaiting {tool} approval"));
        if self.last.as_ref() == Some(&(state, message.clone())) {
            return;
        }
        self.last = Some((state, message.clone()));
        let Some((pane, bin)) = target() else {
            return;
        };
        let mut cmd = Command::new(bin);
        cmd.args([
            "pane",
            "report-agent",
            &pane,
            "--source",
            SOURCE,
            "--agent",
            AGENT,
            "--state",
            state.as_str(),
            "--seq",
            &next_seq().to_string(),
        ]);
        if let Some(message) = &message {
            cmd.args(["--message", message]);
        }
        self.spawn(cmd);
    }

    /// Drop the agent row when dex leaves the pane.
    pub(super) fn release(&mut self) {
        self.reap();
        self.last = None;
        let Some((pane, bin)) = target() else {
            return;
        };
        let mut cmd = Command::new(bin);
        // A seq is required: herdr rejects stale/unguarded reports from a
        // source that has already reported with one.
        cmd.args([
            "pane",
            "release-agent",
            &pane,
            "--source",
            SOURCE,
            "--agent",
            AGENT,
            "--seq",
            &next_seq().to_string(),
        ]);
        self.spawn(cmd);
    }

    fn spawn(&mut self, mut cmd: Command) {
        // Fire-and-forget: the child talks to the Herdr socket on its own;
        // stdio is detached so it can never write into our TTY.
        if let Ok(child) = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            self.children.push(child);
        }
    }

    fn reap(&mut self) {
        self.children
            .retain_mut(|child| child.try_wait().ok().flatten().is_none());
    }

    #[cfg(test)]
    fn wait_all(&mut self) {
        for child in self.children.iter_mut() {
            let _ = child.wait();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::session::{EnvGuard, TEST_SESSIONS_ENV_LOCK};
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// Shim "herdr" binary: appends its argv to $HERDR_TEST_LOG.
    fn install_shim(dir: &Path) -> PathBuf {
        let shim = dir.join("herdr");
        std::fs::write(
            &shim,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HERDR_TEST_LOG\"\n",
        )
        .expect("write shim");
        std::fs::set_permissions(&shim, Permissions::from_mode(0o755)).expect("chmod shim");
        shim
    }

    fn reported_lines(log: &Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn clear_and_set(dir: &Path, shim: bool) -> EnvGuard {
        let guard = EnvGuard(vec![
            ("HERDR_ENV", std::env::var_os("HERDR_ENV")),
            ("HERDR_PANE_ID", std::env::var_os("HERDR_PANE_ID")),
            ("HERDR_BIN_PATH", std::env::var_os("HERDR_BIN_PATH")),
            ("HERDR_TEST_LOG", std::env::var_os("HERDR_TEST_LOG")),
        ]);
        match shim {
            true => {
                std::env::set_var("HERDR_ENV", "1");
                std::env::set_var("HERDR_PANE_ID", "w1:p1");
                std::env::set_var("HERDR_BIN_PATH", install_shim(dir));
            }
            false => {
                std::env::remove_var("HERDR_ENV");
                std::env::remove_var("HERDR_PANE_ID");
                std::env::remove_var("HERDR_BIN_PATH");
            }
        }
        std::env::set_var("HERDR_TEST_LOG", dir.join("reports.log"));
        guard
    }

    fn seq_of(line: &str) -> u64 {
        line.split_whitespace()
            .filter_map(|t| t.parse::<u64>().ok())
            .next_back()
            .expect("seq in report line")
    }

    #[test]
    fn noop_outside_herdr() {
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dex-herdr-off-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let _env = clear_and_set(&dir, false);
        let mut reporter = Reporter::new();
        reporter.sync(true, None);
        reporter.sync(false, Some("bash"));
        reporter.release();
        reporter.wait_all();
        assert!(reported_lines(&dir.join("reports.log")).is_empty());
    }

    #[test]
    fn reports_state_transitions_and_releases() {
        let _lock = TEST_SESSIONS_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dex-herdr-on-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmpdir");
        let _env = clear_and_set(&dir, true);
        let log = dir.join("reports.log");

        let mut reporter = Reporter::new();
        // TUI start reports the baseline idle state, then a turn submits.
        reporter.sync(false, None);
        reporter.sync(true, None);
        // Tool approval pops: blocked, with the tool named.
        reporter.sync(true, Some("bash"));
        // Approval resolved, still busy: back to working.
        reporter.sync(true, None);
        // Turn finishes, then repeats: idle, deduped.
        reporter.sync(false, None);
        reporter.sync(false, None);
        reporter.release();
        reporter.wait_all();

        let lines = reported_lines(&log);
        // Spawned children append out of exec order; herdr orders by --seq.
        let mut reports: Vec<(u64, &String)> = lines
            .iter()
            .filter(|l| l.contains("report-agent"))
            .map(|l| (seq_of(l), l))
            .collect();
        reports.sort_by_key(|(seq, _)| *seq);
        let states: Vec<&str> = reports
            .iter()
            .filter_map(|(_, l)| l.split("--state ").nth(1))
            .map(|s| s.split_whitespace().next().expect("state value"))
            .collect();
        assert_eq!(
            states,
            vec!["idle", "working", "blocked", "working", "idle"]
        );
        let blocked = reports
            .iter()
            .map(|(_, l)| *l)
            .find(|l| l.contains("--state blocked"))
            .expect("blocked report");
        assert!(
            blocked.contains("--message awaiting bash approval"),
            "{blocked}"
        );
        let released = lines
            .iter()
            .find(|l| l.contains("release-agent"))
            .expect("release");
        assert!(
            released.contains("w1:p1")
                && released.contains(SOURCE)
                && released.contains("--agent dex")
                && released.contains("--seq"),
            "{released}"
        );
        // Sequence numbers strictly increase across reports.
        let mut prev = 0;
        for (seq, line) in &reports {
            assert!(seq > &prev, "seq must increase: {line}");
            prev = *seq;
        }
    }
}
