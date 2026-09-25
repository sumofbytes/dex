//! Shared message/role/approval/permission vocabulary (from the old
//! core/types.rs;
//! dissolving in Phase 3 — see protocol/shared.rs for the split map).

/// Transcript lines live in `dex-runtime` (they are carried by
/// `runtime::console::Console`, which both server and client link);
/// re-exported here so `protocol::SinkLine` paths keep working.
pub use dex_runtime::lines::SinkLine;

/// One message on a per-turn steering or follow-up queue. `Content` enqueues a
/// not-yet-delivered item; `Recall` cancels a queued item (matched by content)
/// so the client can pull it back into the composer and edit it. The consumer
/// applies recalls in arrival order, so a recall only cancels an item that has
/// not yet been injected into the conversation — an already-injected item is
/// part of the transcript and cannot be pulled back.
#[derive(Debug)]
pub enum QueueMsg {
    Content(String),
    Recall(String),
}
