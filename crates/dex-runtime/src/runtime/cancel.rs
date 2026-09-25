//! Cancellation primitives: sync source trait + async wait + process-global Ctrl+C.
//! Layer direction is `agent→tools→workspace/runtime`.

/// Shared cancellation contract used by the provider client and agent loop.
pub use dex_ai::CancellationSource;

/// Async wait for cancellation on the sync trait: polls with async sleep
/// (10ms) so `select!` wakes within ~10ms.
pub async fn wait_cancelled(cancel: &(dyn CancellationSource + Send + Sync)) {
    loop {
        if cancel.is_cancelled() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Process-global cancellation (Ctrl+C) used by non-TUI paths.
#[derive(Clone)]
pub struct GlobalCancellation;

impl CancellationSource for GlobalCancellation {
    fn is_cancelled(&self) -> bool {
        crate::runtime::console::is_interrupted()
    }
    fn take_cancelled(&self) -> bool {
        crate::runtime::console::take_interrupt()
    }
}
