//! Pure cursor/buffer/clamp core for background tasks.
//!
//! Sync, no tokio, no `DaemonState` — everything property-testable lives
//! here. The async drain/registry wrappers in `mod.rs` stay thin.
#![allow(dead_code)] // step 0 scaffolding: async/tool/wake call sites land later

use std::collections::VecDeque;

use dex_agent_core::clamp_lines;

/// Max running tasks per session (spec Rev 3: ~8 MiB/session bound).
pub const MAX_RUNNING: usize = 8;
/// Retained finished tasks per session (drop-oldest).
pub const MAX_FINISHED_RETAINED: usize = 8;
/// `wait(timeout_secs)` ceiling, mirrors `delegate` (`schema.rs:21`).
pub const MAX_WAIT_SECS: u64 = 120;
/// Bash-class clamp applied to the *new* byte range.
pub const OUTPUT_CLAMP_LINES: usize = 400;
pub const OUTPUT_CLAMP_BYTES: usize = 32 * 1024;

/// Result of resolving a model-supplied cursor against the virtual stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slice {
    /// Byte offset into the virtual stream to start reading from.
    pub start: u64,
    /// Byte offset to stop at (always `total` on this path).
    pub end: u64,
    /// Bytes dropped between the requested cursor and `start`
    /// (0 unless the cursor predates retention).
    pub truncated: u64,
    /// True when the cursor was past the stream end and got clamped.
    pub clamped: bool,
}

/// Resolve `cursor` (byte offset into `total_written`, `None` = tail) to a
/// `[start, end)` range over the virtual stream. Never panics, never
/// underflows: out-of-range cursors clamp, they don't error.
pub fn slice_range(total: u64, dropped: u64, buf_len: usize, cursor: Option<u64>) -> Slice {
    let buf_len = buf_len as u64;
    // Invariant the registry upholds: `dropped + buf_len == total`
    // (saturating when `total` is small). Clamp defensively anyway.
    let window_start = total.saturating_sub(buf_len).max(dropped.min(total));
    let Some(c) = cursor else {
        return Slice {
            start: window_start,
            end: total,
            truncated: 0,
            clamped: false,
        };
    };
    if c > total {
        return Slice {
            start: total,
            end: total,
            truncated: 0,
            clamped: true,
        };
    }
    if c < window_start {
        return Slice {
            start: window_start,
            end: total,
            truncated: window_start.saturating_sub(c),
            clamped: false,
        };
    }
    Slice {
        start: c,
        end: total,
        truncated: 0,
        clamped: false,
    }
}

/// Append `chunk` to the tail buffer, evicting oldest bytes past `cap`.
/// Upholds `dropped + buf.len() == total`.
pub fn push_bytes(
    buf: &mut VecDeque<u8>,
    total: &mut u64,
    dropped: &mut u64,
    chunk: &[u8],
    cap: usize,
) {
    buf.extend(chunk.iter().copied());
    *total = total.saturating_add(chunk.len() as u64);
    while buf.len() > cap {
        buf.pop_front();
    }
    *dropped = total.saturating_sub(buf.len() as u64);
}

/// Lossy-decode bytes; never panics and always returns valid UTF-8.
/// A slice starting mid-character yields `U+FFFD` at the seam instead of
/// panicking — the cursor contract tolerates the marker.
pub fn decode_lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Head+tail clamp of the *new* output range, same limits as foreground bash.
pub fn clamp_new(text: &str) -> String {
    clamp_lines(text, OUTPUT_CLAMP_LINES, OUTPUT_CLAMP_BYTES)
}

/// Clamp `wait(timeout_secs)` server-side (schema bounds are advisory).
/// `0` is preserved (poll once).
pub fn clamp_timeout(secs: u64) -> u64 {
    secs.min(MAX_WAIT_SECS)
}

/// Session-scoped id (`task-1`…).
pub fn next_id(counter: u64) -> String {
    format!("task-{}", counter + 1)
}

/// True when another task may start.
pub fn cap_running(running: usize) -> bool {
    running < MAX_RUNNING
}

/// Drop-oldest finished retention.
pub fn retain_last8<T>(finished: &mut VecDeque<T>) {
    while finished.len() > MAX_FINISHED_RETAINED {
        finished.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_none_returns_tail_window() {
        let s = slice_range(100, 20, 80, None);
        assert_eq!(
            s,
            Slice {
                start: 20,
                end: 100,
                truncated: 0,
                clamped: false
            }
        );
    }

    #[test]
    fn slice_past_end_clamps() {
        let s = slice_range(10, 0, 10, Some(99));
        assert_eq!(
            s,
            Slice {
                start: 10,
                end: 10,
                truncated: 0,
                clamped: true
            }
        );
    }

    #[test]
    fn slice_predates_window_reports_truncated() {
        let s = slice_range(100, 60, 40, Some(10));
        assert_eq!(s.start, 60);
        assert_eq!(s.truncated, 50);
    }

    #[test]
    fn push_bytes_upholds_accounting() {
        let (mut buf, mut total, mut dropped) = (VecDeque::new(), 0u64, 0u64);
        push_bytes(&mut buf, &mut total, &mut dropped, b"hello", 4);
        assert_eq!(total, 5);
        assert_eq!(buf.len(), 4);
        assert_eq!(dropped, 1);
        assert_eq!(dropped + buf.len() as u64, total);
    }

    #[test]
    fn timeout_clamps() {
        assert_eq!(clamp_timeout(0), 0);
        assert_eq!(clamp_timeout(30), 30);
        assert_eq!(clamp_timeout(999), 120);
    }

    #[test]
    fn ids_are_session_scoped() {
        assert_eq!(next_id(0), "task-1");
        assert_eq!(next_id(7), "task-8");
    }
}

#[cfg(test)]
mod proptests {
    use super::*;

    use proptest::prelude::*;

    fn window_start(total: u64, buf_len: usize, dropped: u64) -> u64 {
        total.saturating_sub(buf_len as u64).max(dropped.min(total))
    }

    proptest! {
        /// P1 cursor totality: no panic/underflow, range inside the stream,
        /// clamped past-end, truncated predating-window.
        #[test]
        fn slice_range_is_total(
            total in 0u64..1_000_000,
            buf_len in 0usize..4096,
            dropped_bias in 0u64..5000,
            cursor in proptest::option::of(0u64..1_000_010),
        ) {
            let dropped = dropped_bias.min(total);
            let ws = window_start(total, buf_len, dropped);
            let s = slice_range(total, dropped, buf_len, cursor);
            prop_assert!(s.start <= s.end);
            prop_assert!(s.end == total);
            prop_assert!(s.end.saturating_sub(s.start) <= buf_len as u64);
            match cursor {
                None => {
                    prop_assert_eq!(s.start, ws);
                    prop_assert!(!s.clamped);
                    prop_assert_eq!(s.truncated, 0);
                }
                Some(c) if c > total => {
                    prop_assert!(s.clamped);
                    prop_assert_eq!(s.start, total);
                    prop_assert_eq!(s.truncated, 0);
                }
                Some(c) if c < ws => {
                    prop_assert!(!s.clamped);
                    prop_assert_eq!(s.start, ws);
                    prop_assert_eq!(s.truncated, ws - c);
                }
                Some(c) => {
                    prop_assert!(!s.clamped);
                    prop_assert_eq!(s.start, c);
                    prop_assert_eq!(s.truncated, 0);
                }
            }
        }

        /// P2 buffer accounting over chunk sequences.
        #[test]
        fn push_bytes_accounting(
            chunks in proptest::collection::vec(
                proptest::collection::vec(any::<u8>(), 0..=2048), 0..=50
            ),
            cap in 1usize..8192,
        ) {
            let (mut buf, mut total, mut dropped) = (VecDeque::new(), 0u64, 0u64);
            let mut model: Vec<u8> = Vec::new();
            for chunk in &chunks {
                push_bytes(&mut buf, &mut total, &mut dropped, chunk, cap);
                model.extend_from_slice(chunk);
                prop_assert_eq!(total, model.len() as u64);
                prop_assert!(buf.len() <= cap);
                prop_assert_eq!(dropped + buf.len() as u64, total);
                let tail_len = model.len().min(cap);
                prop_assert_eq!(
                    buf.iter().copied().collect::<Vec<_>>(),
                    model[model.len() - tail_len..].to_vec()
                );
            }
        }

        /// P3 UTF-8 split safety: arbitrary byte slices never panic,
        /// always valid UTF-8, never longer (in bytes) than lossy can emit
        /// from the input length bound.
        #[test]
        fn decode_never_panics(
            bytes in proptest::collection::vec(any::<u8>(), 0..=512),
        ) {
            let out = decode_lossy(&bytes);
            // Valid UTF-8 by construction; replacement chars keep it bounded.
            prop_assert!(out.len() <= bytes.len() + out.chars().count() * 2);
            prop_assert_eq!(decode_lossy(out.as_bytes()), out);
        }

        /// P3b unicode text split at arbitrary seams.
        #[test]
        fn decode_split_unicode(
            chars in proptest::collection::vec(proptest::char::any(), 0..=120),
            at in 0usize..200,
        ) {
            let s: String = chars.into_iter().collect();
            let bytes = s.as_bytes();
            let mid = at.min(bytes.len());
            let (a, b) = (decode_lossy(&bytes[..mid]), decode_lossy(&bytes[mid..]));
            let _ = a + &b; // must not panic; validity holds per half
        }

        /// P4 clamp bounds on the new range.
        #[test]
        fn clamp_new_bounds(
            lines in proptest::collection::vec("[^\n\r]{0,40}", 0..=60),
        ) {
            let text = lines.join("\n");
            let out = clamp_new(&text);
            prop_assert!(out.lines().count() <= OUTPUT_CLAMP_LINES + 1);
            prop_assert!(out.matches(" lines truncated ").count() <= 1);
            prop_assert!(!out.contains("[... 0 of "));
            // Note: no idempotency assert — `clamp_lines` normalizes
            // trailing newlines via `lines().join()`, so blank-line
            // inputs are not fixed points (same as foreground bash).
        }

        /// P6 timeout clamp.
        #[test]
        fn timeout_clamp_holds(secs in any::<u64>()) {
            let c = clamp_timeout(secs);
            prop_assert!(c <= MAX_WAIT_SECS);
            if secs <= MAX_WAIT_SECS {
                prop_assert_eq!(c, secs);
            }
        }
    }
}
