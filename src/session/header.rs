use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::protocol::ChatMessage;

pub(crate) const SESSION_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct SessionHeader {
    #[serde(rename = "type")]
    pub(crate) entry_type: String,
    pub(crate) version: u32,
    pub(crate) id: String,
    pub(crate) timestamp: String,
    pub(crate) cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
}

impl SessionHeader {
    pub(crate) fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
    pub(crate) fn cwd(&self) -> &str {
        &self.cwd
    }
    pub(crate) fn timestamp(&self) -> &str {
        &self.timestamp
    }
}

/// Borrowed view of a message for append-time serialization: the journal
/// line is built from references, so appending never clones the message.
/// Reads parse entries back through `serde_json::Value`, not this struct.
#[derive(Serialize)]
pub(crate) struct SessionMessageEntry<'a> {
    #[serde(rename = "type")]
    pub(crate) entry_type: &'a str,
    pub(crate) id: &'a str,
    pub(crate) timestamp: &'a str,
    #[serde(flatten)]
    pub(crate) message: &'a ChatMessage,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub(crate) struct SessionInfoEntry {
    #[serde(rename = "type")]
    pub(crate) entry_type: String,
    pub(crate) id: String,
    pub(crate) timestamp: String,
    pub(crate) name: String,
}

#[derive(Serialize)]
pub(crate) struct SessionClearEntry {
    #[serde(rename = "type")]
    pub(crate) entry_type: String,
    pub(crate) id: String,
    pub(crate) timestamp: String,
}

#[derive(Serialize)]
pub(crate) struct SessionEventEntry {
    #[serde(rename = "type")]
    pub(crate) entry_type: String,
    pub(crate) id: String,
    pub(crate) timestamp: String,
    /// Complexity-router tier that chose this turn's model (`turn_start`
    /// only, routing enabled). Absent otherwise, so unrouted journals
    /// stay byte-identical to before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tier: Option<String>,
}

/// Durable record of one side effect: intent (before execution) and outcome
/// (after). Written around every tool execution so a restart can reconcile
/// what happened vs. what completed (P8 journal).
#[derive(Serialize)]
pub(crate) struct SessionEffectEntry {
    #[serde(rename = "type")]
    pub(crate) entry_type: String,
    pub(crate) id: String,
    pub(crate) timestamp: String,
    pub(crate) tool_call_id: String,
    pub(crate) name: String,
    pub(crate) input_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ok: Option<bool>,
}

#[derive(Serialize)]
pub(crate) struct SessionStateEntry {
    #[serde(rename = "type")]
    pub(crate) entry_type: String,
    pub(crate) id: String,
    pub(crate) timestamp: String,
    pub(crate) key: String,
    pub(crate) value: String,
}

// ---------------------------------------------------------------------------
// Append-only journal caches (perf doc §§11–12)
// ---------------------------------------------------------------------------
//
// Production writers only ever append — `clear_messages` writes a `clear`
// marker the loader folds, and no delete/reset endpoint exists — so a
// snapshot stays exact as long as every mutation flows through
// `append_line` (session journal) / `append_event` (events journal) below,
// which refresh the snapshot's identity after each write. The one exception
// is `rewrite_messages` (post-compaction): it replaces the file atomically
// (temp + rename) and publishes the fresh snapshot fused with its identity
// under one lock. Identity is `(mtime, len)`: any out-of-band rewrite (a
// test's `fs::write`, a hand edit) misses and re-parses from disk, and a
// missing file evicts. Paths are unique per session and entries are
// FIFO-capped, so a long-lived daemon can't accumulate dead sessions.

/// `(mtime, len)` identity for a journal snapshot. Shared by the history
/// and events caches (`events.rs`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId {
    pub(crate) mtime: SystemTime,
    pub(crate) len: u64,
}

pub(crate) fn file_id(path: &Path) -> Option<FileId> {
    fs::metadata(path).ok().and_then(|m| {
        m.modified().ok().map(|mtime| FileId {
            mtime,
            len: m.len(),
        })
    })
}

/// FIFO-capped process-global cache keyed by journal path. Shared by the
/// history and events caches (`events.rs`).
pub(crate) struct PathCache<V> {
    pub(crate) map: HashMap<PathBuf, V>,
    pub(crate) order: VecDeque<PathBuf>,
}

impl<V> PathCache<V> {
    const CAP: usize = 32;

    pub(crate) fn insert(&mut self, path: &Path, value: V) {
        if !self.map.contains_key(path) {
            self.order.push_back(path.to_path_buf());
            while self.order.len() > Self::CAP {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
            // Eviction above skips keys removed out of band; compact the
            // queue if it still overflows with stale keys.
            if self.order.len() > Self::CAP * 2 {
                self.order.retain(|p| self.map.contains_key(p));
            }
        }
        self.map.insert(path.to_path_buf(), value);
    }

    pub(crate) fn get(&self, path: &Path) -> Option<&V> {
        self.map.get(path)
    }

    pub(crate) fn get_mut(&mut self, path: &Path) -> Option<&mut V> {
        self.map.get_mut(path)
    }

    pub(crate) fn evict(&mut self, path: &Path) {
        self.map.remove(path);
    }
}
