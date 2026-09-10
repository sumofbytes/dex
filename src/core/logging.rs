//! Runtime log filtering behind `DEX_LOG` — works in release binaries too:
//! no static max-level cap is compiled in, so `DEX_LOG=trace` is honored by
//! `cargo build --release` output without a rebuild (the filter is a runtime
//! ceiling, not a debug-assertion).
//!
//! Sink is stderr, except while a TUI owns the terminal (`redirect_to_file`),
//! where logs append to `$XDG_DATA_HOME/dex/dex.log` (or are dropped when no
//! file is available) so alt-screen output isn't garbled.

use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

/// Severity order: `Off` sorts below every message level, so a plain integer
/// compare gates everything when it is the ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Level {
    Off = 0,
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl Level {
    fn parse(raw: &str) -> Option<Level> {
        Some(match raw.trim().to_ascii_lowercase().as_str() {
            "off" => Level::Off,
            "error" => Level::Error,
            "warn" => Level::Warn,
            "info" => Level::Info,
            "debug" => Level::Debug,
            "trace" => Level::Trace,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Level::Off => "OFF",
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }
}

/// Default ceiling: only failures surface without opting in; `DEX_LOG` raises it.
static MAX_LEVEL: AtomicU8 = AtomicU8::new(Level::Warn as u8);

enum Sink {
    Stderr,
    File(File),
    /// Drop lines: used while the TUI owns the terminal and no log file
    /// could be opened — stderr would garble the alt screen.
    Null,
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

/// Read `DEX_LOG` once at startup. An unrecognized value warns and falls
/// back to the default, so a typo never silences startup diagnostics.
pub(crate) fn init() {
    let level = match env::var("DEX_LOG") {
        Ok(raw) => match Level::parse(&raw) {
            Some(level) => level,
            None => {
                eprintln!(
                    "dex: ignoring unknown DEX_LOG value '{raw}' (expected off|error|warn|info|debug|trace)"
                );
                Level::Warn
            }
        },
        Err(_) => Level::Warn,
    };
    set_level(level);
}

fn set_level(level: Level) {
    MAX_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Cheap first gate: severity clears the ceiling. The `log!` macro checks
/// this before formatting anything.
pub(crate) fn enabled(level: Level) -> bool {
    MAX_LEVEL.load(Ordering::Relaxed) >= level as u8
}

/// One line per event: `<timestamp> <LEVEL> <module>: <message>`.
pub(crate) fn log(level: Level, target: &str, args: fmt::Arguments<'_>) {
    let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f");
    let line = format!("{ts} {} {target}: {args}\n", level.as_str());
    let Ok(mut sink) = SINK.get_or_init(|| Mutex::new(Sink::Stderr)).lock() else {
        return; // a poisoned logger must not panic the process
    };
    let _ = match &mut *sink {
        Sink::Stderr => std::io::stderr().write_all(line.as_bytes()),
        Sink::File(file) => file.write_all(line.as_bytes()),
        Sink::Null => Ok(()),
    };
}

fn set_sink(sink: Sink) {
    *SINK
        .get_or_init(|| Mutex::new(Sink::Stderr))
        .lock()
        .unwrap() = sink;
}

/// Install the file sink for `path` (created as needed), or fall back to a
/// null sink when it can't be opened. Returns a notice for the caller to
/// print while the terminal is still the normal screen — only when logs are
/// actually being raised above the default (`DEX_LOG` ≥ info).
fn install_file_sink(path: PathBuf) -> Option<String> {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => {
            set_sink(Sink::File(file));
            None
        }
        Err(_) => {
            set_sink(Sink::Null);
            enabled(Level::Info).then(|| {
                format!("dex: could not open {path:?} for logs; dropping DEX_LOG output while the TUI owns the terminal")
            })
        }
    }
}

/// The TUI is about to own the terminal: move subsequent logs to
/// `$XDG_DATA_HOME/dex/dex.log`. When no file is available, fall back to a
/// null sink instead of stderr — the alt screen is running and stray log
/// lines would garble it (real errors still reach the TUI as protocol
/// events). Returns a notice for the caller to print while the terminal is
/// still the normal screen.
pub(crate) fn redirect_to_file() -> Option<String> {
    match log_file() {
        Some(path) => install_file_sink(path),
        None => {
            set_sink(Sink::Null);
            enabled(Level::Info).then(|| {
                "dex: no log location (XDG_DATA_HOME/HOME unset); dropping DEX_LOG output while the TUI owns the terminal".to_string()
            })
        }
    }
}

/// Same base resolution as sessions (`Session::session_dir`): XDG wins, then
/// the HOME default. No HOME at all → no file, and `redirect_to_file` drops
/// lines instead of garbling the TUI with stderr.
fn log_file() -> Option<PathBuf> {
    let base = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("dex/dex.log"))
}

/// `crate::log!(Debug, "fmt {x}")` — filtered at runtime by `DEX_LOG`.
#[macro_export]
macro_rules! log {
    ($level:ident, $($arg:tt)+) => {
        if $crate::core::logging::enabled($crate::core::logging::Level::$level) {
            $crate::core::logging::log(
                $crate::core::logging::Level::$level,
                module_path!(),
                format_args!($($arg)+),
            )
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_named_levels_case_insensitively() {
        assert_eq!(Level::parse("trace"), Some(Level::Trace));
        assert_eq!(Level::parse("  DEBUG "), Some(Level::Debug));
        assert_eq!(Level::parse("Warn"), Some(Level::Warn));
        assert_eq!(Level::parse("off"), Some(Level::Off));
        assert_eq!(Level::parse("verbose"), None);
        assert_eq!(Level::parse(""), None);
    }

    #[test]
    fn ceiling_gates_by_severity() {
        let passes = |ceiling: Level, msg: Level| ceiling as u8 >= msg as u8;
        for (ceiling, msg, want) in [
            (Level::Off, Level::Error, false),
            (Level::Warn, Level::Error, true),
            (Level::Warn, Level::Warn, true),
            (Level::Warn, Level::Info, false),
            (Level::Debug, Level::Trace, false),
            (Level::Trace, Level::Trace, true),
        ] {
            assert_eq!(passes(ceiling, msg), want, "{ceiling:?} vs {msg:?}");
        }
    }

    #[test]
    fn redirected_sink_receives_formatted_lines() {
        set_level(Level::Trace);
        let path = std::env::temp_dir().join(format!("dex-log-test-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        assert!(
            install_file_sink(path.clone()).is_none(),
            "open should succeed"
        );
        log(Level::Debug, "t", format_args!("hello {}", 7));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.ends_with("DEBUG t: hello 7\n"), "{text}");
        let _ = fs::remove_file(&path);
        // Restore global state for the rest of the suite.
        set_level(Level::Warn);
        set_sink(Sink::Stderr);
    }

    #[test]
    fn unwritable_log_target_falls_back_to_null() {
        // A regular file used as a directory: create_dir_all and the open both
        // fail, so the sink must drop lines instead of writing to stderr.
        set_level(Level::Debug);
        let blocker = std::env::temp_dir().join(format!("dex-log-nodir-{}", std::process::id()));
        fs::write(&blocker, b"x").unwrap();
        let path = blocker.join("dex.log");
        assert!(
            install_file_sink(path).is_some(),
            "failure should yield a notice"
        );
        let _ = fs::remove_file(&blocker);
        // Restore global state for the rest of the suite.
        set_level(Level::Warn);
        set_sink(Sink::Stderr);
    }
}
