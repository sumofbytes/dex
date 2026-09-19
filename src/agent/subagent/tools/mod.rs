//! Phase 5 — the three delegation tools plus the child body that runs the
//! standard turn loop (plan §5, §8, §10): `delegate` spawns through the
//! per-session [`AgentManager`] and returns the id immediately, `delegate_output`
//! is the bounded poll-wait (≤120 s, ~250 ms sleeps, early exit on completion
//! or parent cancel — never `select!` on steering, which is `&mut`-borrowed by
//! the parent loop and unreachable from a tool call), `delegate_stop` cancels.
//!
//! The child body is the same `process_turn` runtime with an isolated bundle:
//! its own config clone (model override via the one-knob path, §13), its own
//! JSONL session (§16), its own console (a child-local sink; no approval
//! channel — background children cannot prompt, §12 V1a detached auto-deny),
//! the definition's tool allowlist enforced at dispatch (§11), and no
//! steering. Nesting is a depth counter (`MAX_AGENT_DEPTH`): a child whose
//! depth is under the cap keeps a daemon context and may delegate further;
//! at the cap the bundle carries none, so delegation rejects at dispatch.

#[cfg(test)]
use super::exit::ExitReason;
#[cfg(test)]
use super::manager::AgentManager;
#[cfg(test)]
use super::manager::WaitOutcome;
#[cfg(test)]
use super::model::AgentId;
#[cfg(test)]
use super::model::AgentResult;
#[cfg(test)]
use super::model::AgentState;
#[cfg(test)]
use super::model::ContextSeed;
#[cfg(test)]
use super::resume::ResumeHandle;
#[cfg(test)]
use super::resume::ResumeRequest;
#[cfg(test)]
use super::SpawnMeta;
#[cfg(test)]
use crate::protocol::ChatMessage;
#[cfg(test)]
use crate::runtime::console::CancellationToken;
#[cfg(test)]
use crate::session::Session;
#[cfg(test)]
use crate::tools::Policy;
#[cfg(test)]
use crate::tools::ToolFilter;
#[cfg(test)]
use serde_json::json;
#[cfg(test)]
use serde_json::Map;
#[cfg(test)]
use serde_json::Value;
#[cfg(test)]
use std::collections::HashSet;
#[cfg(test)]
use std::path::PathBuf;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

mod exec;
mod schema;
pub(crate) use exec::execute_delegation;
#[cfg(test)]
pub(crate) use exec::{
    child_system_prompt, delegate, delegate_list, delegate_output, effective_child_model,
    parse_generation, resolve_child_config, resolve_resume_handle, resume_messages, resume_nudge,
    seed_task_text,
};
#[cfg(test)]
pub(crate) use schema::DELEGATION_TOOLS;
pub(crate) use schema::{
    delegation_enabled, is_delegation, set_daemon_linked, status_word, AgentTurnContext,
};

#[cfg(test)]
mod tests;
