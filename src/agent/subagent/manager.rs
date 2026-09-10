//! Phase 4 — per-session child-agent lifecycle: registry, spawn cap,
//! bounded wait, cancel, and completion notices.
//!
//! The manager owns *mechanics only*: id allocation, the spawn cap,
//! cancellation tokens, task handles, result retention, and the notice
//! queue. It never builds prompts, touches the model, or interprets
//! results — the caller supplies the child body at [`AgentManager::spawn`]
//! (Phase 5's delegate tool builds it from the parent turn; Phase 7 adds
//! tool filtering, model override, and timeout). Tests inject mock bodies,
//! which keeps every lifecycle path hermetic.
//!
//! Note on the plan (§7, §14): the sketch shows `spawn(def, seed)` with the
//! child body implied. The body arrives as an argument instead, because the
//! manager must not contain model logic and the only code that can build
//! the child future (parent-turn config, client, tools, policy) lives with
//! the caller. Same seam, one parameter wider.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::core::console::CancellationToken;

use super::context::ContextSeed;
use super::definition::AgentDefinition;
use super::instance::{AgentId, AgentInstance, AgentState};
use super::result::AgentResult;

/// Max live children per session (plan §7). The 5th concurrent spawn is
/// rejected with the running list so the caller can wait on or cancel one
/// instead of fanning out.
pub(crate) const MAX_CHILDREN: usize = 4;
/// Completion notices retained per session; once full, further completions
/// fold into the [`AgentManager::take_overflow`] counter instead of growing
/// without bound.
pub(crate) const MAX_NOTICES: usize = 32;
/// Terminal results retained per session, so `wait` (and Phase 5's
/// `delegate_output`) can fetch them after the notice is drained.
const MAX_RESULTS: usize = 64;
/// `wait` poll quantum: prompt completion delivery without busy-spinning.
const WAIT_POLL: Duration = Duration::from_millis(25);

/// Completion announcement queued for the Phase 6 drain site (parent turn
/// end, which renders these into context). Notices are informational only —
/// the retained [`AgentResult`] is the source of truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentNotice {
    pub(crate) agent_id: AgentId,
    pub(crate) name: String,
    pub(crate) status: AgentState,
}

/// Bounded-wait outcome for [`AgentManager::wait`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WaitOutcome {
    /// Terminal result; also retained for later fetch after notice drain.
    Finished(AgentResult),
    /// Still live when `timeout` elapsed; carries the last-seen state.
    Running(AgentState),
    /// Unknown id: never spawned here, or its result aged out of retention.
    Unknown,
}

/// `spawn` rejection. `AtCapacity` carries the running list so the caller
/// (model or operator) can pick a waiter instead of guessing.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SpawnError {
    AtCapacity {
        limit: usize,
        running: Vec<(AgentId, String)>,
    },
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AtCapacity { limit, running } => {
                let list = running
                    .iter()
                    .map(|(id, name)| format!("{id} ({name})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "at child capacity ({limit} running): {list}; \
                     wait for or cancel one before spawning"
                )
            }
        }
    }
}

impl std::error::Error for SpawnError {}

/// Per-session child-agent registry. Cheap to clone; all clones share one
/// `Mutex<Inner>` so concurrent completions cannot tear registry state
/// (plan §17).
#[derive(Clone)]
pub(crate) struct AgentManager {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    session: String,
    next_counter: u64,
    running: HashMap<AgentId, RunningChild>,
    results: HashMap<AgentId, AgentResult>,
    /// Insertion order of `results`, for oldest-first eviction.
    result_order: VecDeque<AgentId>,
    notices: VecDeque<AgentNotice>,
    /// Completions dropped because `notices` was full.
    overflowed: usize,
}

struct RunningChild {
    instance: AgentInstance,
    token: CancellationToken,
    handle: Option<JoinHandle<AgentResult>>,
}

impl AgentManager {
    pub(crate) fn new(session: &str) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                session: session.to_string(),
                next_counter: 0,
                running: HashMap::new(),
                results: HashMap::new(),
                result_order: VecDeque::new(),
                notices: VecDeque::new(),
                overflowed: 0,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Spawn a child agent.
    ///
    /// `run` receives the child's [`CancellationToken`] and produces its
    /// terminal [`AgentResult`]; results are filed under the allocated id,
    /// so bodies cannot misattribute. Every terminal path — completion,
    /// failure, cancel — funnels through result retention, the notice
    /// queue, and registry removal.
    ///
    /// The instance starts `Running`: the task is spawned before this
    /// returns, so `Pending` is unobservable and would only race status
    /// checks. (`Pending` stays for future queued spawns.)
    pub(crate) async fn spawn<F, Fut>(
        &self,
        def: &AgentDefinition,
        seed: ContextSeed,
        run: F,
    ) -> Result<AgentId, SpawnError>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = AgentResult> + Send + 'static,
    {
        let (id, token) = {
            let mut inner = self.lock();
            if inner.running.len() >= MAX_CHILDREN {
                let running = inner
                    .running
                    .iter()
                    .map(|(id, child)| (id.clone(), child.instance.definition.name.clone()))
                    .collect();
                return Err(SpawnError::AtCapacity {
                    limit: MAX_CHILDREN,
                    running,
                });
            }
            let id = AgentId(format!("{}-{}", inner.session, inner.next_counter));
            inner.next_counter += 1;
            let token = CancellationToken::new();
            inner.running.insert(
                id.clone(),
                RunningChild {
                    instance: AgentInstance {
                        id: id.clone(),
                        definition: def.clone(),
                        parent_id: None,
                        context: seed,
                        state: AgentState::Running,
                    },
                    token: token.clone(),
                    handle: None,
                },
            );
            (id, token)
        };

        let manager = self.clone();
        let name = def.name.clone();
        let task_id = id.clone();
        let handle = tokio::spawn(async move {
            // Cancel wins over a body that ignores its token: without this
            // select, `cancel` would only flag while the child ran forever.
            let result = tokio::select! {
                result = run(token.clone()) => result,
                () = token.cancelled() => AgentResult {
                    status: AgentState::Cancelled,
                    summary: String::new(),
                    error: Some("cancelled".to_string()),
                },
            };
            manager.finish(&task_id, &name, result.clone());
            result
        });

        // The wrapper may already have finished (and removed its entry) for
        // an immediately-ready body; only record the handle if still live.
        if let Some(child) = self.lock().running.get_mut(&id) {
            child.handle = Some(handle);
        }
        Ok(id)
    }

    /// Stamp one terminal result through retention, notices, and registry
    /// removal. Single choke point: completion, failure, cancel (and later
    /// timeout) all land here, so no terminal path can orphan an entry.
    fn finish(&self, id: &AgentId, name: &str, result: AgentResult) {
        let mut inner = self.lock();
        if inner.results.len() >= MAX_RESULTS {
            if let Some(oldest) = inner.result_order.pop_front() {
                inner.results.remove(&oldest);
            }
        }
        let status = result.status;
        inner.result_order.push_back(id.clone());
        inner.results.insert(id.clone(), result);
        if inner.notices.len() >= MAX_NOTICES {
            inner.overflowed += 1;
        } else {
            inner.notices.push_back(AgentNotice {
                agent_id: id.clone(),
                name: name.to_string(),
                status,
            });
        }
        inner.running.remove(id);
    }

    /// Bounded wait: the retained result if terminal, the last-seen state
    /// if `timeout` elapses first, `Unknown` for an id this manager never
    /// spawned (or whose result aged out of retention).
    pub(crate) async fn wait(&self, id: &AgentId, timeout: Duration) -> WaitOutcome {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let inner = self.lock();
                if let Some(result) = inner.results.get(id) {
                    return WaitOutcome::Finished(result.clone());
                }
                match inner.running.get(id) {
                    Some(child) => {
                        if tokio::time::Instant::now() >= deadline {
                            return WaitOutcome::Running(child.instance.state);
                        }
                    }
                    None => return WaitOutcome::Unknown,
                }
            }
            tokio::time::sleep(WAIT_POLL).await;
        }
    }

    /// Current lifecycle state, live or terminal. `None` for unknown ids.
    /// Cancel-surfaced states arrive here once the wrapper funnels them
    /// through `finish` — `cancel` itself only signals the token.
    pub(crate) fn status(&self, id: &AgentId) -> Option<AgentState> {
        let inner = self.lock();
        if let Some(child) = inner.running.get(id) {
            return Some(child.instance.state);
        }
        inner.results.get(id).map(|result| result.status)
    }

    /// Signal the child's token. Returns the last-seen state (`None` for
    /// unknown ids); the `Cancelled` result itself lands via `finish` once
    /// the wrapper observes the token. Signalling a finished id is a
    /// harmless no-op lookup.
    pub(crate) async fn cancel(&self, id: &AgentId) -> Option<AgentState> {
        let inner = self.lock();
        if let Some(child) = inner.running.get(id) {
            child.token.cancel();
            return Some(child.instance.state);
        }
        inner.results.get(id).map(|result| result.status)
    }

    /// Drain queued completion notices, oldest first. Results stay
    /// retained — draining never loses fetchability.
    pub(crate) async fn drain_notices(&self) -> Vec<AgentNotice> {
        self.lock().notices.drain(..).collect()
    }

    /// Completions dropped while the notice queue was full. Resets to zero;
    /// the Phase 6 drain site renders a non-zero take as "N more children
    /// finished — ask for specifics".
    pub(crate) fn take_overflow(&self) -> usize {
        std::mem::take(&mut self.lock().overflowed)
    }

    /// Live children. The daemon shutdown path (§17) joins until this
    /// reaches zero.
    pub(crate) fn active_count(&self) -> usize {
        self.lock().running.len()
    }

    /// Cancel every live child and join their tasks. After this returns,
    /// every child has funneled through `finish` (results + notices
    /// retained) and `active_count` is zero: a joined handle means its
    /// wrapper already stored and removed its entry.
    pub(crate) async fn shutdown(&self) {
        let handles: Vec<JoinHandle<AgentResult>> = {
            let mut inner = self.lock();
            for child in inner.running.values() {
                child.token.cancel();
            }
            inner
                .running
                .values_mut()
                .filter_map(|child| child.handle.take())
                .collect()
        };
        for handle in handles {
            let _ = handle.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::definition::PermissionInherit;
    use super::*;
    use std::path::PathBuf;
    use tokio::sync::Barrier;

    fn test_def(name: &str) -> AgentDefinition {
        AgentDefinition {
            name: name.to_string(),
            description: format!("{name} test agent"),
            prompt: String::new(),
            model: None,
            tools: ["read".to_string()].into_iter().collect(),
            permissions: PermissionInherit::Inherit,
            max_tool_iterations: None,
            timeout: Duration::from_secs(60),
        }
    }

    fn test_seed() -> ContextSeed {
        ContextSeed {
            task: "do the thing".to_string(),
            file_hints: vec![PathBuf::from("src/main.rs")],
            parent_summary: None,
        }
    }

    fn completed(summary: &str) -> AgentResult {
        AgentResult {
            status: AgentState::Completed,
            summary: summary.to_string(),
            error: None,
        }
    }

    fn failed(error: &str) -> AgentResult {
        AgentResult {
            status: AgentState::Failed,
            summary: String::new(),
            error: Some(error.to_string()),
        }
    }

    /// A body that only ends through its token — the shape Phase 7's real
    /// runner has. Lets tests hold children live without sleeps.
    async fn token_body(token: CancellationToken) -> AgentResult {
        token.cancelled().await;
        AgentResult {
            status: AgentState::Cancelled,
            summary: String::new(),
            error: Some("child saw cancel".to_string()),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn spawn_assigns_session_scoped_ids_and_reports_running() {
        let mgr = AgentManager::new("sess");
        // `spawn` never yields before returning, so on a single-threaded
        // runtime the wrapper cannot have run yet: fully deterministic.
        let first = mgr
            .spawn(&test_def("explorer"), test_seed(), |_| async {
                completed("findings")
            })
            .await
            .unwrap();
        let second = mgr
            .spawn(&test_def("tester"), test_seed(), |_| async {
                completed("pass")
            })
            .await
            .unwrap();
        assert_eq!(first.to_string(), "sess-0");
        assert_eq!(second.to_string(), "sess-1");
        assert_eq!(mgr.status(&first), Some(AgentState::Running));
        assert_eq!(mgr.active_count(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn completing_child_files_result_and_notice() {
        let mgr = AgentManager::new("sess");
        let id = mgr
            .spawn(&test_def("explorer"), test_seed(), |_| async {
                completed("findings")
            })
            .await
            .unwrap();
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Completed);
                assert_eq!(result.summary, "findings");
                assert_eq!(result.error, None);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert_eq!(mgr.status(&id), Some(AgentState::Completed));
        assert_eq!(mgr.active_count(), 0);
        let notices = mgr.drain_notices().await;
        assert_eq!(
            notices,
            vec![AgentNotice {
                agent_id: id,
                name: "explorer".to_string(),
                status: AgentState::Completed,
            }]
        );
        assert_eq!(mgr.take_overflow(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_result_preserved_with_error() {
        let mgr = AgentManager::new("sess");
        let id = mgr
            .spawn(&test_def("tester"), test_seed(), |_| async {
                failed("boom")
            })
            .await
            .unwrap();
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Failed);
                assert_eq!(result.error.as_deref(), Some("boom"));
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        let notices = mgr.drain_notices().await;
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].status, AgentState::Failed);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wait_times_out_then_finishes() {
        let mgr = AgentManager::new("sess");
        let id = mgr
            .spawn(&test_def("explorer"), test_seed(), |_| async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                completed("late")
            })
            .await
            .unwrap();
        match mgr.wait(&id, Duration::from_millis(50)).await {
            WaitOutcome::Running(state) => assert_eq!(state, AgentState::Running),
            other => panic!("expected Running, got {other:?}"),
        }
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => assert_eq!(result.summary, "late"),
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_id_is_unknown_everywhere() {
        let mgr = AgentManager::new("sess");
        let unknown = AgentId("sess-99".to_string());
        assert_eq!(
            mgr.wait(&unknown, Duration::from_millis(10)).await,
            WaitOutcome::Unknown
        );
        assert_eq!(mgr.status(&unknown), None);
        assert_eq!(mgr.cancel(&unknown).await, None);
        assert!(mgr.drain_notices().await.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_fires_child_token_and_yields_cancelled() {
        let mgr = AgentManager::new("sess");
        let id = mgr
            .spawn(&test_def("explorer"), test_seed(), token_body)
            .await
            .unwrap();
        assert_eq!(mgr.cancel(&id).await, Some(AgentState::Running));
        // `cancel` never yields, so on a single-threaded runtime the
        // wrapper cannot have reaped the entry yet: the fired token is
        // observably the child's own.
        let fired = mgr
            .lock()
            .running
            .get(&id)
            .map(|child| child.token.is_cancelled());
        assert_eq!(fired, Some(true));
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => {
                assert_eq!(result.status, AgentState::Cancelled);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        assert_eq!(mgr.active_count(), 0);
        let notices = mgr.drain_notices().await;
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].status, AgentState::Cancelled);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn at_capacity_rejects_with_running_list_then_frees() {
        let mgr = AgentManager::new("sess");
        let mut ids = Vec::new();
        for n in 0..MAX_CHILDREN {
            ids.push(
                mgr.spawn(&test_def(&format!("agent-{n}")), test_seed(), token_body)
                    .await
                    .unwrap(),
            );
        }
        match mgr
            .spawn(&test_def("one-too-many"), test_seed(), token_body)
            .await
        {
            Err(SpawnError::AtCapacity { limit, running }) => {
                assert_eq!(limit, MAX_CHILDREN);
                assert_eq!(running.len(), MAX_CHILDREN);
                let listed: Vec<AgentId> = running.into_iter().map(|(id, _)| id).collect();
                for id in &ids {
                    assert!(listed.contains(id), "missing {id} in rejection");
                }
            }
            other => panic!("expected AtCapacity, got {other:?}"),
        }
        assert!(mgr
            .spawn(&test_def("x"), test_seed(), token_body)
            .await
            .unwrap_err()
            .to_string()
            .contains(&ids[0].to_string()));
        // Freeing one slot unblocks spawn.
        mgr.cancel(&ids[0]).await;
        match mgr.wait(&ids[0], Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => assert_eq!(result.status, AgentState::Cancelled),
            other => panic!("expected Finished, got {other:?}"),
        }
        mgr.spawn(&test_def("fits-now"), test_seed(), token_body)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn notices_bound_and_overflow_folds() {
        let mgr = AgentManager::new("sess");
        for _ in 0..(MAX_NOTICES + 3) {
            let id = mgr
                .spawn(&test_def("explorer"), test_seed(), |_| async {
                    completed("done")
                })
                .await
                .unwrap();
            assert!(matches!(
                mgr.wait(&id, Duration::from_secs(5)).await,
                WaitOutcome::Finished(_)
            ));
        }
        let notices = mgr.drain_notices().await;
        assert_eq!(notices.len(), MAX_NOTICES);
        // FIFO: the retained notices are the first completions.
        assert_eq!(notices[0].agent_id.to_string(), "sess-0");
        assert_eq!(mgr.take_overflow(), 3);
        assert_eq!(mgr.take_overflow(), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn results_survive_notice_drain() {
        let mgr = AgentManager::new("sess");
        let id = mgr
            .spawn(&test_def("explorer"), test_seed(), |_| async {
                completed("durable")
            })
            .await
            .unwrap();
        assert!(matches!(
            mgr.wait(&id, Duration::from_secs(5)).await,
            WaitOutcome::Finished(_)
        ));
        assert_eq!(mgr.drain_notices().await.len(), 1);
        match mgr.wait(&id, Duration::from_secs(5)).await {
            WaitOutcome::Finished(result) => assert_eq!(result.summary, "durable"),
            other => panic!("expected retained Finished, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_cancels_and_joins_children() {
        let mgr = AgentManager::new("sess");
        let first = mgr
            .spawn(&test_def("explorer"), test_seed(), token_body)
            .await
            .unwrap();
        let second = mgr
            .spawn(&test_def("tester"), test_seed(), token_body)
            .await
            .unwrap();
        mgr.shutdown().await;
        assert_eq!(mgr.active_count(), 0);
        for id in [&first, &second] {
            match mgr.wait(id, Duration::from_secs(5)).await {
                WaitOutcome::Finished(result) => {
                    assert_eq!(result.status, AgentState::Cancelled)
                }
                other => panic!("expected Finished, got {other:?}"),
            }
        }
        assert_eq!(mgr.drain_notices().await.len(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn concurrent_completions_stay_consistent() {
        let mgr = AgentManager::new("sess");
        let gate = std::sync::Arc::new(Barrier::new(MAX_CHILDREN));
        let mut ids = Vec::new();
        for n in 0..MAX_CHILDREN {
            let gate = gate.clone();
            ids.push(
                mgr.spawn(
                    &test_def(&format!("agent-{n}")),
                    test_seed(),
                    |_| async move {
                        gate.wait().await;
                        completed("through the gate")
                    },
                )
                .await
                .unwrap(),
            );
        }
        for id in &ids {
            match mgr.wait(id, Duration::from_secs(5)).await {
                WaitOutcome::Finished(result) => {
                    assert_eq!(result.status, AgentState::Completed)
                }
                other => panic!("expected Finished, got {other:?}"),
            }
        }
        assert_eq!(mgr.active_count(), 0);
        assert_eq!(mgr.drain_notices().await.len(), MAX_CHILDREN);
        assert_eq!(mgr.take_overflow(), 0);
    }
}
