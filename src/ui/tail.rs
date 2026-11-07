//! What the transcript's tail block is currently streaming.
//!
//! ```text
//!            text delta            thinking delta
//!   Idle ----------------> Assistant ------------> Thinking
//!    ^  <---- block ends ----'   ^                     |
//!    |                           '---- text delta -----' (thinking closes first)
//!    '------------------ any other line / turn end ----'
//! ```
//!
//! One value replaces the `assistant_open` / `thinking_open` flag pair: the
//! tail can be coalescing assistant text or streaming thinking, never both.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Tail {
    /// No streaming block at the tail: the next delta starts a fresh one.
    #[default]
    Idle,
    /// The tail `Assistant` block is open for coalescing further deltas.
    Assistant,
    /// The tail `Thinking` block is still receiving deltas.
    Thinking,
}

impl Tail {
    pub(crate) fn is_assistant(self) -> bool {
        self == Self::Assistant
    }

    pub(crate) fn is_thinking(self) -> bool {
        self == Self::Thinking
    }

    /// An assistant block was just pushed or extended.
    pub(crate) fn open_assistant(&mut self) {
        *self = Self::Assistant;
    }

    /// A non-assistant block took the tail. Leaves an open thinking block
    /// alone: only `end_thinking` (which also re-wraps the spinner) closes it.
    pub(crate) fn close_assistant(&mut self) {
        if self.is_assistant() {
            *self = Self::Idle;
        }
    }

    /// A thinking delta arrived. Returns whether the block was already open.
    pub(crate) fn start_thinking(&mut self) -> bool {
        let was = self.is_thinking();
        *self = Self::Thinking;
        was
    }

    /// Close the thinking block; returns whether one was open.
    pub(crate) fn end_thinking(&mut self) -> bool {
        let was = self.is_thinking();
        if was {
            *self = Self::Idle;
        }
        was
    }

    /// Conversation reset: nothing is streaming.
    pub(crate) fn reset(&mut self) {
        *self = Self::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_and_thinking_are_exclusive() {
        let mut t = Tail::Idle;
        t.open_assistant();
        assert!(t.is_assistant() && !t.is_thinking());
        assert!(!t.start_thinking());
        assert!(t.is_thinking() && !t.is_assistant());
        assert!(t.start_thinking(), "second delta: already open");
    }

    #[test]
    fn close_assistant_never_closes_thinking() {
        let mut t = Tail::Thinking;
        t.close_assistant();
        assert!(t.is_thinking());
        assert!(t.end_thinking());
        assert!(!t.end_thinking());
        assert_eq!(t, Tail::Idle);
    }
}
