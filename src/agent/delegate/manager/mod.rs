//! Phase 4 — per-session child-agent lifecycle: registry, spawn cap,
//! bounded wait, cancel, timeout, and completion notices.
//!
//! The manager owns *mechanics only*: id allocation, the spawn cap,
//! cancellation tokens, task handles, result retention, and the notice
//! queue. It never builds prompts, touches the model, or interprets
//! results — the caller supplies the child body at [`AgentManager::spawn`]
//! (Phase 5's delegate tool builds it from the parent turn; Phase 7 adds
//! tool filtering and the model override). Tests inject mock bodies,
//! which keeps every lifecycle path hermetic.
//!
//! Mechanics the wrapper enforces here so no terminal path can orphan a
//! registry entry (plan §14): body panics funnel a synthesized `Failed`
//! result through `finish`; a run outliving its definition's `timeout`
//! ends `TimedOut`; and after `shutdown` the manager is closed, so a
//! stale clone cannot orphan a child into a registry nobody will join.
//!
//! Note on the plan (§7, §14): the sketch shows `spawn(def, seed)` with the
//! child body implied. The body arrives as an argument instead, because the
//! manager must not contain model logic and the only code that can build
//! the child future (parent-turn config, client, tools, policy) lives with
//! the caller. Same seam, one parameter wider.

#[cfg(test)]
use super::definition::AgentDefinition;
#[cfg(test)]
use super::exit::ExitReason;
#[cfg(test)]
use super::model::AgentId;
#[cfg(test)]
use super::model::AgentResult;
#[cfg(test)]
use super::model::AgentState;
#[cfg(test)]
use super::model::AgentUsage;
#[cfg(test)]
use crate::runtime::console::CancellationToken;
#[cfg(test)]
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::time::Duration;

mod lifecycle;
mod registry;
pub(crate) use lifecycle::{AgentEvent, AgentManager, ProgressReporter};
#[cfg(test)]
pub(crate) use lifecycle::{MAX_CHILDREN, MAX_NOTICES};
pub(crate) use registry::{AgentNotice, ChildInfo, SpawnError, SpawnMeta, WaitOutcome};

#[cfg(test)]
mod tests;
