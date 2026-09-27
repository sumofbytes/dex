//! Shared message/role/approval/permission vocabulary for agent, tools,
//! session, and UI.

/// Transcript lines live in `dex-agent-core` (they are the turn engine's
/// host-boundary vocabulary, carried by `runtime::console::Console`, which
/// both server and client link); re-exported here so `protocol::SinkLine`
/// paths keep working.
pub use dex_agent_core::lines::SinkLine;

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
