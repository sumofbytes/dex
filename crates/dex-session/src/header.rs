use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use dex_ai::ChatMessage;

pub const SESSION_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub entry_type: String,
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl SessionHeader {
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn cwd(&self) -> &str {
        &self.cwd
    }
    pub fn timestamp(&self) -> &str {
        &self.timestamp
    }
}

/// Borrowed view of a message for append-time serialization: the journal
/// line is built from references, so appending never clones the message.
/// Reads parse entries back through `serde_json::Value`, not this struct.
#[derive(Serialize)]
pub struct SessionMessageEntry<'a> {
    #[serde(rename = "type")]
    pub entry_type: &'a str,
    pub id: &'a str,
    pub timestamp: &'a str,
    #[serde(flatten)]
    pub message: &'a ChatMessage,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SessionInfoEntry {
    #[serde(rename = "type")]
    pub entry_type: String,
    pub id: String,
    pub timestamp: String,
    pub name: String,
}

#[derive(Serialize)]
pub struct SessionClearEntry {
    #[serde(rename = "type")]
    pub entry_type: String,
    pub id: String,
    pub timestamp: String,
}

#[derive(Serialize)]
pub struct SessionEventEntry {
    #[serde(rename = "type")]
    pub entry_type: String,
    pub id: String,
    pub timestamp: String,
    /// Agent mode (`plan`/`manual`/`auto`) that governed the turn
    /// (`turn_start` only). Absent for legacy journals and subagent
    /// turns, so a reattach can restore the client's last selector.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

/// Durable record of one side effect: intent (before execution) and outcome
/// (after). Written around every tool execution so a restart can reconcile
/// what happened vs. what completed (P8 journal). Nothing wires it into the
/// turn loop yet — test-only until a producer lands.
#[derive(Serialize)]
#[cfg(test)]
pub struct SessionEffectEntry {
    #[serde(rename = "type")]
    pub entry_type: String,
    pub id: String,
    pub timestamp: String,
    pub tool_call_id: String,
    pub name: String,
    pub input_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
}

#[derive(Serialize)]
pub struct SessionStateEntry {
    #[serde(rename = "type")]
    pub entry_type: String,
    pub id: String,
    pub timestamp: String,
    pub key: String,
    pub value: String,
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
pub struct FileId {
    pub mtime: SystemTime,
    pub len: u64,
}

pub fn file_id(path: &Path) -> Option<FileId> {
    fs::metadata(path).ok().and_then(|m| {
        m.modified().ok().map(|mtime| FileId {
            mtime,
            len: m.len(),
        })
    })
}

/// FIFO-capped process-global cache keyed by journal path. Shared by the
/// history and events caches (`events.rs`).
pub struct PathCache<V> {
    pub map: HashMap<PathBuf, V>,
    pub order: VecDeque<PathBuf>,
}

impl<V> PathCache<V> {
    const CAP: usize = 32;

    pub fn insert(&mut self, path: &Path, value: V) {
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

    pub fn get(&self, path: &Path) -> Option<&V> {
        self.map.get(path)
    }

    pub fn get_mut(&mut self, path: &Path) -> Option<&mut V> {
        self.map.get_mut(path)
    }

    pub fn evict(&mut self, path: &Path) {
        self.map.remove(path);
    }
}
