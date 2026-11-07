//! Run phase of a cancellable job (an agent turn or a `!` shell escape).
//!
//! ```text
//!   Idle --start--> Running --cancel--> Cancelling { presses }
//!     ^                |                     |  ^
//!     +-----finish-----+---------finish-------+  +-- bump (Ctrl+C again)
//! ```
//!
//! One value replaces the old `busy` / `cancel_requested` / `cancel_presses`
//! flag trio, so "cancelling but not busy" and "presses without a cancel" are
//! unrepresentable. Every transition is a method; callers never poke the
//! variants directly.

/// Third Ctrl+C press while cancelling force-quits (stuck daemon/shell).
pub(crate) const FORCE_QUIT_PRESSES: u8 = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Phase {
    #[default]
    Idle,
    Running,
    /// Cancel requested, awaiting the job's terminal event. `presses` counts
    /// Ctrl+C presses since the cancel began.
    Cancelling {
        presses: u8,
    },
}

impl Phase {
    /// Running or cancelling: the job has not ended yet.
    pub(crate) fn is_active(self) -> bool {
        !matches!(self, Self::Idle)
    }

    pub(crate) fn is_cancelling(self) -> bool {
        matches!(self, Self::Cancelling { .. })
    }

    #[cfg(test)]
    pub(crate) fn presses(self) -> u8 {
        match self {
            Self::Cancelling { presses } => presses,
            _ => 0,
        }
    }

    /// Begin a job from any phase, discarding any stale cancel.
    pub(crate) fn start(&mut self) {
        *self = Self::Running;
    }

    /// `Running -> Cancelling`. Returns whether the transition happened, so a
    /// repeat request stays idempotent (no re-POST, no repeated notice).
    pub(crate) fn cancel(&mut self) -> bool {
        if matches!(self, Self::Running) {
            *self = Self::Cancelling { presses: 0 };
            true
        } else {
            false
        }
    }

    /// Count one more Ctrl+C while cancelling; returns the new total (0 when
    /// not cancelling).
    pub(crate) fn bump(&mut self) -> u8 {
        if let Self::Cancelling { presses } = self {
            *presses = presses.saturating_add(1);
            *presses
        } else {
            0
        }
    }

    /// Drop a pending cancel without ending the job (`Cancelling -> Running`).
    pub(crate) fn clear_cancel(&mut self) {
        if self.is_cancelling() {
            *self = Self::Running;
        }
    }

    /// The job ended (from any phase).
    pub(crate) fn finish(&mut self) {
        *self = Self::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_is_idempotent_and_only_from_running() {
        let mut p = Phase::Idle;
        assert!(!p.cancel());
        p.start();
        assert!(p.cancel());
        assert!(!p.cancel());
        assert_eq!(p, Phase::Cancelling { presses: 0 });
    }

    #[test]
    fn presses_count_only_while_cancelling() {
        let mut p = Phase::Running;
        assert_eq!(p.bump(), 0);
        p.cancel();
        assert_eq!((p.bump(), p.bump()), (1, 2));
        assert_eq!(p.presses(), 2);
        p.clear_cancel();
        assert_eq!(p, Phase::Running);
        assert_eq!(p.presses(), 0);
    }

    #[test]
    fn finish_resets_everything() {
        let mut p = Phase::Running;
        p.cancel();
        p.bump();
        p.finish();
        assert_eq!(p, Phase::Idle);
        assert!(!p.is_active() && !p.is_cancelling());
    }
}
