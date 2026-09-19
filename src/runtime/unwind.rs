//! `Future::catch_unwind` without a new dependency: polls the inner future
//! inside `std::panic::catch_unwind` per poll, so a panicking future yields
//! `Err(message)` instead of aborting its task. Shared by the daemon turn
//! runner (a panicking turn must still deliver `TurnFailed`) and the
//! sub-agent manager (a panicking child body must still file a `Failed`
//! result — plan §14: no terminal path may orphan a registry entry).

pub(crate) struct CatchUnwind<F> {
    inner: std::pin::Pin<Box<F>>,
    message: &'static str,
}

impl<F: std::future::Future> CatchUnwind<F> {
    pub(crate) fn new(inner: std::pin::Pin<Box<F>>, message: &'static str) -> Self {
        Self { inner, message }
    }
}

impl<F: std::future::Future> std::future::Future for CatchUnwind<F> {
    type Output = Result<F::Output, String>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        // `CatchUnwind<F>` is `Unpin` (`Pin<Box<F>>` is), so `get_mut` is safe;
        // polling stays in safe Rust (no pin projection).
        let this = self.get_mut();
        let inner = this.inner.as_mut();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(std::task::Poll::Ready(v)) => std::task::Poll::Ready(Ok(v)),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(_) => std::task::Poll::Ready(Err(this.message.to_string())),
        }
    }
}
