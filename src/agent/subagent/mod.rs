//! Sub-agent domain types (plan §4): definitions, instances, results,
//! manager, and the delegation tools.
//!
//! The manager owns lifecycle mechanics only — registry, spawn cap,
//! cancel, timeout, completion notices, and resume handles for manual
//! re-entry. There is no automatic recovery, no spawn queue, and no
//! idle reaper: over-cap spawns reject, and the model re-enters dead
//! children by hand with `delegate(resume_from)`. The blanket
//! `dead_code` allow stays only for genuinely optional surface;
//! everything else is dead-code free.

mod definition;
mod exit;
pub(crate) mod manager;
mod model;
pub(crate) mod resume;
pub(crate) mod tools;

#[allow(unused_imports)]
pub(crate) use definition::{
    builtin_definitions, find_definition, AgentDefinition, DEFAULT_AGENT_TIMEOUT, READ_ONLY_TOOLS,
};
#[allow(unused_imports)]
pub(crate) use exit::{classify_body_error, transcript_holds_progress, ExhaustKind, ExitReason};
#[allow(unused_imports)]
pub(crate) use manager::{
    AgentEvent, AgentManager, AgentNotice, ChildInfo, ProgressReporter, SpawnError, SpawnMeta,
    WaitOutcome,
};
#[allow(unused_imports)]
pub(crate) use model::{AgentId, AgentInstance, AgentResult, AgentState, AgentUsage, ContextSeed};
#[allow(unused_imports)]
pub(crate) use resume::{advertised_remaining, resume_note, ResumeHandle, ResumeRequest};
pub(crate) use tools::{
    delegation_enabled, execute_delegation, is_delegation, set_daemon_linked, status_word,
    AgentTurnContext,
};
