//! Invocation tracing (spec §35): one record per extension-event dispatch,
//! appended to the hook engine's audit surface and printed by
//! `dex runtime trace`. Bounded ring — a long daemon run cannot grow it
//! without limit; the newest entries are the ones being debugged.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// Ring capacity: enough to cover several turns of hook activity.
const TRACE_CAP: usize = 1000;

#[derive(Clone, Debug)]
pub struct TraceRecord {
    /// Wall-clock time of the dispatch start.
    pub at: SystemTime,
    /// Extension that served the call.
    pub component: String,
    /// Event (enveloped slot) that was dispatched.
    pub event: String,
    pub duration_ms: u128,
    /// `ok` or `error` — the fail-open contract keeps this informative only.
    pub status: &'static str,
    /// Truncated error detail, when the call failed.
    pub detail: Option<String>,
}

fn ring() -> &'static Mutex<VecDeque<TraceRecord>> {
    static RING: OnceLock<Mutex<VecDeque<TraceRecord>>> = OnceLock::new();
    RING.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Append one invocation record (newest last; oldest evicted at cap).
pub fn record(
    component: &str,
    event: &str,
    duration: std::time::Duration,
    status: &'static str,
    detail: Option<String>,
) {
    let mut ring = ring().lock().unwrap_or_else(|e| e.into_inner());
    if ring.len() == TRACE_CAP {
        ring.pop_front();
    }
    ring.push_back(TraceRecord {
        at: SystemTime::now(),
        component: component.to_string(),
        event: event.to_string(),
        duration_ms: duration.as_millis(),
        status,
        detail: detail.map(|d| d.chars().take(200).collect()),
    });
}

/// Newest-last snapshot of the ring.
pub fn snapshot() -> Vec<TraceRecord> {
    ring()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_newest_and_caps() {
        for i in 0..(TRACE_CAP + 50) {
            record(
                "ext",
                "tool.before",
                std::time::Duration::from_millis(i as u64),
                "ok",
                None,
            );
        }
        let snap = snapshot();
        // Concurrent tests also record into the shared ring, so assert the
        // ring contract (bounded, newest last) rather than exact eviction
        // boundaries: our 1050 sequential records must all have been capped.
        assert!(snap.len() <= TRACE_CAP);
        let ours: Vec<u128> = snap
            .iter()
            .filter(|r| r.event == "tool.before")
            .map(|r| r.duration_ms)
            .collect();
        // Our records survive newest-first: the last few of the loop are
        // present and strictly increasing.
        assert!(ours.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(ours.last().copied(), Some((TRACE_CAP + 49) as u128));
        // Error detail is truncated to the record cap.
        record(
            "ext",
            "tool.after",
            std::time::Duration::from_millis(1),
            "error",
            Some("x".repeat(500)),
        );
        let ours = snapshot()
            .into_iter()
            .rev()
            .find(|r| r.event == "tool.after" && r.status == "error")
            .expect("our error record still in the ring");
        assert_eq!(ours.detail.as_deref().map(str::len), Some(200));
    }
}
