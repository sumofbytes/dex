//! Background shell-task registry (spec Rev 3, step 0).
//!
//! Pure registry bookkeeping lives here alongside [`pure`]; the async drain,
//! tool executor, journal/wake wiring, and TUI land in later steps. No tokio
//! awaits here — locks stay short at the call sites.

pub mod pure;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Instant;

use pure::{cap_running, next_id, retain_last8, MAX_FINISHED_RETAINED};

/// Terminal or running status of one background task.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub enum TaskStatus {
    Running,
    Exited(i32),
    Killed,
    Failed(String),
}

impl TaskStatus {
    /// One-word status for `list` rows and `TaskFinished` events.
    #[allow(dead_code)]
    pub fn word(&self) -> String {
        match self {
            Self::Running => "running".to_string(),
            Self::Exited(0) => "exit 0".to_string(),
            Self::Exited(code) => format!("exit {code}"),
            Self::Killed => "killed".to_string(),
            Self::Failed(_) => "failed".to_string(),
        }
    }

    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// One background task: tail buffer + virtual-stream accounting.
/// The async drain owns the `Child`; the registry keeps only `pid`
/// (plus the drain `JoinHandle` at the call site, stored beside this).
#[derive(Debug)]
#[allow(dead_code)]
pub struct BgTask {
    pub id: String,
    pub command: String,
    pub cwd: PathBuf,
    pub status: TaskStatus,
    pub started_at: Instant,
    pub ended_at: Option<Instant>,
    pub buf: VecDeque<u8>,
    pub total_written: u64,
    pub dropped_prefix: u64,
    pub pid: Option<u32>,
}

impl BgTask {
    pub fn new(id: String, command: String, cwd: PathBuf) -> Self {
        Self {
            id,
            command,
            cwd,
            status: TaskStatus::Running,
            started_at: Instant::now(),
            ended_at: None,
            buf: VecDeque::new(),
            total_written: 0,
            dropped_prefix: 0,
            pid: None,
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self.status, TaskStatus::Running)
    }
}

/// Completion notice queued for the next turn / idle wake.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct TaskNotice {
    pub id: String,
    pub command: String,
    pub status: String,
    pub tail: String,
}

#[allow(dead_code)]
impl TaskNotice {
    /// The lifecycle line drained into `agent-notifications` and journaled
    /// beside the typed `TaskFinished` event.
    pub fn text(&self) -> String {
        let mut text = format!(
            "[task {}] finished, {}: {}",
            self.id, self.status, self.command
        );
        if !self.tail.trim().is_empty() {
            text.push_str(&format!(" (tail: {:?})", self.tail));
        }
        text
    }
}

/// Per-session registry: monotonic ids, running cap, finished retention.
#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct TaskRegistry {
    counter: u64,
    tasks: HashMap<String, BgTask>,
    finished_order: VecDeque<String>,
}

#[allow(dead_code)]
impl TaskRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn running_count(&self) -> usize {
        self.tasks.values().filter(|t| t.is_running()).count()
    }

    /// Allocate the next session-scoped id without inserting.
    pub fn peek_next_id(&self) -> String {
        next_id(self.counter)
    }

    /// Insert a new running task; `Err` past the running cap.
    pub fn spawn(&mut self, command: String, cwd: PathBuf) -> Result<String, String> {
        if !cap_running(self.running_count()) {
            return Err(format!(
                "too many running background tasks (max {})",
                pure::MAX_RUNNING
            ));
        }
        let id = next_id(self.counter);
        self.counter += 1;
        self.tasks
            .insert(id.clone(), BgTask::new(id.clone(), command, cwd));
        Ok(id)
    }

    /// Mark terminal and enforce last-8 retention (drop-oldest id evicted).
    pub fn finish(&mut self, id: &str, status: TaskStatus) -> Option<String> {
        let task = self.tasks.get_mut(id)?;
        task.status = status;
        task.ended_at = Some(Instant::now());
        task.pid = None;
        self.finished_order.push_back(id.to_string());
        retain_last8(&mut self.finished_order);
        while self.finished_order.len() > MAX_FINISHED_RETAINED {
            self.finished_order.pop_front();
        }
        // Evict finished tasks beyond retention (oldest first).
        let live: std::collections::HashSet<String> = self.finished_order.iter().cloned().collect();
        let evict: Vec<String> = self
            .tasks
            .iter()
            .filter(|(_, t)| t.status.is_terminal() && !live.contains(t.id.as_str()))
            .map(|(id, _)| id.clone())
            .collect();
        // `finish` just pushed, so at most the pre-existing overflow evicts.
        let mut evicted = None;
        for id in evict {
            self.tasks.remove(&id);
            evicted = Some(id);
        }
        evicted
    }

    pub fn get(&self, id: &str) -> Option<&BgTask> {
        self.tasks.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut BgTask> {
        self.tasks.get_mut(id)
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Highest id ever allocated (ids are dense `task-1..=counter` minus
    /// evicted finished) — lets `list` probe without an iterator.
    pub fn task_count_hint(&self) -> u64 {
        self.counter
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_enforces_running_cap() {
        let mut reg = TaskRegistry::new();
        for _ in 0..pure::MAX_RUNNING {
            reg.spawn("sleep 60".to_string(), PathBuf::from("/tmp"))
                .unwrap();
        }
        assert!(reg
            .spawn("one-more".to_string(), PathBuf::from("/tmp"))
            .is_err());
    }

    #[test]
    fn finish_retains_last8() {
        let mut reg = TaskRegistry::new();
        let mut ids = Vec::new();
        for i in 0..8 {
            ids.push(
                reg.spawn(format!("cmd {i}"), PathBuf::from("/tmp"))
                    .unwrap(),
            );
        }
        for id in ids.iter().take(7) {
            reg.finish(id, TaskStatus::Exited(0));
        }
        // 1 still running + 7 finished; spawn 2 more, finish everything:
        // 9 finished total → retain 8, evict oldest finished (ids[0]).
        for i in 8..10 {
            ids.push(
                reg.spawn(format!("cmd {i}"), PathBuf::from("/tmp"))
                    .unwrap(),
            );
        }
        for id in ids.iter().skip(7) {
            reg.finish(id, TaskStatus::Exited(0));
        }
        assert!(reg.get(&ids[0]).is_none());
        assert!(reg.get(&ids[9]).is_some());
    }

    #[test]
    fn status_words() {
        assert_eq!(TaskStatus::Running.word(), "running");
        assert_eq!(TaskStatus::Exited(0).word(), "exit 0");
        assert_eq!(TaskStatus::Exited(3).word(), "exit 3");
        assert_eq!(TaskStatus::Killed.word(), "killed");
    }
}

#[cfg(test)]
mod proptests {
    use super::*;

    use proptest::prelude::*;

    proptest! {
        /// P5 retention/cap over spawn/finish op sequences.
        #[test]
        fn registry_caps_hold(
            ops in proptest::collection::vec(0u8..10, 0..=60),
        ) {            let mut reg = TaskRegistry::new();
            let mut live: Vec<String> = Vec::new();
            let mut seen: std::collections::HashSet<String> = Default::default();
            for op in ops {
                if op % 3 == 0 && !live.is_empty() {
                    let idx = (op as usize) % live.len();
                    let id = live.remove(idx);
                    reg.finish(&id, TaskStatus::Exited(0));
                } else {
                    match reg.spawn("cmd".to_string(), PathBuf::from("/tmp")) {
                        Ok(id) => {
                            prop_assert!(seen.insert(id.clone()));
                            live.push(id);
                        }
                        Err(_) => {
                            prop_assert!(reg.running_count() >= pure::MAX_RUNNING);
                        }
                    }
                }
                prop_assert!(reg.running_count() <= pure::MAX_RUNNING);
                let finished = reg.len().saturating_sub(reg.running_count());
                prop_assert!(finished <= pure::MAX_FINISHED_RETAINED);
            }
            // Ids strictly increase: counter order matches numeric suffix.
            let mut nums: Vec<u64> = seen
                .iter()
                .filter_map(|id| id.strip_prefix("task-")?.parse().ok())
                .collect();
            nums.sort_unstable();
            for pair in nums.windows(2) {
                prop_assert!(pair[0] < pair[1]);
            }
        }

        /// P8 model equivalence: `output(cursor)` over the registry always
        /// equals the model's byte stream tail; `wait`+cursor shares the
        /// same read path by construction (both call `bg_read`).
        #[test]
        fn output_matches_model_stream(
            chunks in proptest::collection::vec(
                proptest::collection::vec(any::<u8>(), 0..=512), 0..=20
            ),
            cursor in proptest::option::of(0u64..3000),
            cap in 64usize..4096,
        ) {
            use std::collections::VecDeque;
            let mut reg = TaskRegistry::new();
            let id = reg.spawn("cmd".to_string(), PathBuf::from("/tmp")).unwrap();
            let mut model: Vec<u8> = Vec::new();
            for chunk in &chunks {
                let task = reg.get_mut(&id).unwrap();
                pure::push_bytes(&mut task.buf, &mut task.total_written, &mut task.dropped_prefix, chunk, cap);
                model.extend_from_slice(chunk);
            }
            let task = reg.get(&id).unwrap();
            let slice = pure::slice_range(task.total_written, task.dropped_prefix, task.buf.len(), cursor);
            let buf_start = task.total_written.saturating_sub(task.buf.len() as u64);
            let from = slice.start.saturating_sub(buf_start) as usize;
            let to = slice.end.saturating_sub(buf_start) as usize;
            let got: Vec<u8> = task.buf.iter().skip(from).take(to.saturating_sub(from)).copied().collect();
            // Model: virtual stream is `model`; visible window is its tail.
            let total = model.len() as u64;
            let ws = total.saturating_sub(task.buf.len() as u64).max(task.dropped_prefix.min(total));
            let exp_start = match cursor {
                None => ws,
                Some(c) if c > total => total,
                Some(c) => c.max(ws),
            };
            let exp: Vec<u8> = model.get(exp_start as usize..).unwrap_or(&[]).to_vec();
            prop_assert_eq!(slice.start, exp_start);
            prop_assert_eq!(slice.end, total);
            prop_assert_eq!(got, exp);
            // Retention accounting holds after every sequence.
            prop_assert_eq!(task.dropped_prefix + task.buf.len() as u64, task.total_written);
            let _ = VecDeque::<u8>::new();
        }
    }
}
