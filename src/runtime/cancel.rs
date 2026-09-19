//! Cancellation primitives: sync source trait + async wait + process-global Ctrl+C.
//! Moved down from `agent::state` so `tools` no longer imports `agent`
//! (direction is `agent→tools→workspace/runtime`).

/// Sync cancellation probe (Ctrl+C, per-session token, test doubles).
pub(crate) trait CancellationSource: Send + Sync {
    fn is_cancelled(&self) -> bool;
    fn take_cancelled(&self) -> bool;
}

/// Async wait for cancellation on the sync trait: polls with async sleep
/// (10ms) so `select!` wakes within ~10ms.
pub(crate) async fn wait_cancelled(cancel: &(dyn CancellationSource + Send + Sync)) {
    loop {
        if cancel.is_cancelled() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Process-global cancellation (Ctrl+C) used by non-TUI paths.
#[derive(Clone)]
pub(crate) struct GlobalCancellation;

impl CancellationSource for GlobalCancellation {
    fn is_cancelled(&self) -> bool {
        crate::runtime::console::is_interrupted()
    }
    fn take_cancelled(&self) -> bool {
        crate::runtime::console::take_interrupt()
    }
}
