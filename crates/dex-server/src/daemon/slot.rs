//! The per-session turn slot: one entry per session with a turn in flight.
//!
//! This replaces four parallel maps (`active_turns`, `cancel_tokens`,
//! `steering_txs`, `followup_txs`) whose entries had to be inserted and
//! removed in step. A session is idle when it has no entry; otherwise the
//! entry is in exactly one phase:
//!
//! ```text
//!            claim                set_cancel              attach_queues
//!   (none) ---------> Claimed -----------------> Cancellable ------------> Streaming
//!     ^                  |                            |                        |
//!     +---- release -----+------------ release -------+--- release_owned ------+
//! ```
//!
//! * `Claimed`: the slot is held (a wake, a chat turn still setting up, or a
//!   short-lived rename/rewrite) but nothing can cancel it yet.
//! * `Cancellable`: a turn token is registered, so `POST /cancel` reaches it.
//! * `Streaming`: the turn's steering/follow-up queues exist, so `POST /steer`
//!   and `POST /followup` have a target.
//!
//! `cancel` and the queue senders only exist in the phases that own them, so
//! a queue without a turn (or a token without a slot) is unrepresentable.

use tokio::sync::mpsc;

use crate::protocol::QueueMsg;
use crate::runtime::console::CancellationToken;

use super::state::{lock_map, DaemonState};

pub(crate) enum Slot {
    Claimed,
    Cancellable {
        cancel: CancellationToken,
    },
    Streaming {
        cancel: CancellationToken,
        steering: mpsc::Sender<QueueMsg>,
        followup: mpsc::Sender<QueueMsg>,
    },
}

impl Slot {
    fn cancel(&self) -> Option<&CancellationToken> {
        match self {
            Self::Claimed => None,
            Self::Cancellable { cancel } | Self::Streaming { cancel, .. } => Some(cancel),
        }
    }
}

impl DaemonState {
    /// Whether a turn (or a claim standing in for one) holds the session.
    pub(crate) fn turn_active(&self, session_id: &str) -> bool {
        lock_map(&self.turns).contains_key(session_id)
    }

    /// Take the session's slot (`None -> Claimed`); `false` when it is held.
    pub(crate) fn claim_turn(&self, session_id: &str) -> bool {
        let mut turns = lock_map(&self.turns);
        if turns.contains_key(session_id) {
            return false;
        }
        turns.insert(session_id.to_string(), Slot::Claimed);
        true
    }

    /// Register the turn's cancel token (`Claimed -> Cancellable`). An
    /// unclaimed session gets the slot outright; a later phase keeps its
    /// queues and only swaps the token.
    pub(crate) fn set_turn_cancel(&self, session_id: &str, cancel: CancellationToken) {
        let mut turns = lock_map(&self.turns);
        let slot = match turns.remove(session_id) {
            Some(Slot::Streaming {
                steering, followup, ..
            }) => Slot::Streaming {
                cancel,
                steering,
                followup,
            },
            _ => Slot::Cancellable { cancel },
        };
        turns.insert(session_id.to_string(), slot);
    }

    /// Create the turn's steering/follow-up queues and register their
    /// senders (`-> Streaming`), so `/steer` and `/followup` can push at
    /// once. `cancel` is the owning turn's token.
    pub(crate) fn attach_queues(
        &self,
        session_id: &str,
        cancel: &CancellationToken,
    ) -> (mpsc::Receiver<QueueMsg>, mpsc::Receiver<QueueMsg>) {
        let (steering, steering_rx) = mpsc::channel::<QueueMsg>(16);
        let (followup, followup_rx) = mpsc::channel::<QueueMsg>(16);
        lock_map(&self.turns).insert(
            session_id.to_string(),
            Slot::Streaming {
                cancel: cancel.clone(),
                steering,
                followup,
            },
        );
        (steering_rx, followup_rx)
    }

    /// Free the slot unconditionally (a claim that never became a turn).
    pub(crate) fn release_turn(&self, session_id: &str) {
        lock_map(&self.turns).remove(session_id);
    }

    /// Free the slot only if `cancel` is the token registered in it, and
    /// report whether it was. An idle wake stolen by a chat POST must not
    /// free the chat turn's fresh slot when the wake finally unwinds.
    pub(crate) fn release_owned_turn(&self, session_id: &str, cancel: &CancellationToken) -> bool {
        let mut turns = lock_map(&self.turns);
        let owned = turns
            .get(session_id)
            .and_then(Slot::cancel)
            .is_some_and(|token| token.same_token(cancel));
        if owned {
            turns.remove(session_id);
        }
        owned
    }

    /// The registered turn token, cloned so callers cancel outside the lock.
    pub(crate) fn turn_cancel_token(&self, session_id: &str) -> Option<CancellationToken> {
        lock_map(&self.turns)
            .get(session_id)
            .and_then(Slot::cancel)
            .cloned()
    }

    /// The live turn's steering (or follow-up) sender; `None` before the
    /// queues exist or when no turn is running.
    pub(crate) fn turn_queue(
        &self,
        session_id: &str,
        followup: bool,
    ) -> Option<mpsc::Sender<QueueMsg>> {
        match lock_map(&self.turns).get(session_id) {
            Some(Slot::Streaming {
                steering,
                followup: f,
                ..
            }) => Some(if followup { f } else { steering }.clone()),
            _ => None,
        }
    }
}

#[cfg(test)]
impl DaemonState {
    /// Test seam: put `session_id` in the `Streaming` phase with caller-owned queues.
    pub(crate) fn seed_streaming(
        &self,
        session_id: &str,
        steering: mpsc::Sender<QueueMsg>,
        followup: mpsc::Sender<QueueMsg>,
    ) {
        lock_map(&self.turns).insert(
            session_id.to_string(),
            Slot::Streaming {
                cancel: CancellationToken::new(),
                steering,
                followup,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_advance_and_release() {
        let state = DaemonState::new();
        assert!(!state.turn_active("s"));
        assert!(state.turn_cancel_token("s").is_none());
        assert!(state.turn_queue("s", false).is_none());

        assert!(state.claim_turn("s"));
        assert!(!state.claim_turn("s"), "one turn per session");
        assert!(state.turn_cancel_token("s").is_none(), "claimed: no token");

        let cancel = CancellationToken::new();
        state.set_turn_cancel("s", cancel.clone());
        assert!(state.turn_cancel_token("s").is_some());
        assert!(state.turn_queue("s", false).is_none(), "no queues yet");

        let (_s, _f) = state.attach_queues("s", &cancel);
        assert!(state.turn_queue("s", false).is_some());
        assert!(state.turn_queue("s", true).is_some());

        assert!(state.release_owned_turn("s", &cancel));
        assert!(!state.turn_active("s"));
    }

    #[test]
    fn stale_owner_cannot_free_a_newer_turn() {
        let state = DaemonState::new();
        let old = CancellationToken::new();
        state.set_turn_cancel("s", old.clone());
        // A chat turn takes over the slot with its own token.
        state.release_turn("s");
        let new = CancellationToken::new();
        assert!(state.claim_turn("s"));
        state.set_turn_cancel("s", new.clone());
        assert!(
            !state.release_owned_turn("s", &old),
            "old wake is not owner"
        );
        assert!(state.turn_active("s"));
        assert!(state.release_owned_turn("s", &new));
    }

    #[test]
    fn claimed_slot_is_not_owned_by_any_token() {
        let state = DaemonState::new();
        assert!(state.claim_turn("s"));
        assert!(!state.release_owned_turn("s", &CancellationToken::new()));
        state.release_turn("s");
        assert!(!state.turn_active("s"));
    }

    #[test]
    fn swapping_the_token_on_a_streaming_slot_keeps_its_queues() {
        let state = DaemonState::new();
        let first = CancellationToken::new();
        state.set_turn_cancel("s", first.clone());
        let (_s, _f) = state.attach_queues("s", &first);
        let second = CancellationToken::new();
        state.set_turn_cancel("s", second.clone());
        assert!(state.turn_queue("s", false).is_some(), "queues survive");
        assert!(
            !state.release_owned_turn("s", &first),
            "old token no longer owns"
        );
        assert!(state.release_owned_turn("s", &second));
    }

    #[test]
    fn release_leaves_neither_token_nor_queues_behind() {
        let state = DaemonState::new();
        let cancel = CancellationToken::new();
        assert!(state.claim_turn("s"));
        state.set_turn_cancel("s", cancel.clone());
        let (_s, _f) = state.attach_queues("s", &cancel);
        state.release_turn("s");
        assert!(!state.turn_active("s"));
        assert!(state.turn_cancel_token("s").is_none());
        assert!(state.turn_queue("s", false).is_none());
        assert!(state.turn_queue("s", true).is_none());
        assert!(state.claim_turn("s"), "slot is claimable again");
    }
}
