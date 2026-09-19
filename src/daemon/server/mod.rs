#[cfg(test)]
use super::lock_map;
#[cfg(test)]
use super::lookup::lookup_entry;
#[cfg(test)]
use super::lookup::persisted_current;
#[cfg(test)]
use super::required_token;
use super::shell::session_shell;
#[cfg(test)]
use super::turn::apply_thinking_override;
#[cfg(test)]
use super::turn::run_turn_inner;
#[cfg(test)]
use super::turn::TurnChannels;
use super::DaemonState;
#[cfg(test)]
use super::PendingApproval;
#[cfg(test)]
use super::SessionEntry;
#[cfg(test)]
use crate::llm::config::LlmConfig;
#[cfg(test)]
use crate::protocol::ApprovalDecision;
#[cfg(test)]
use crate::protocol::ChatRequest;
#[cfg(test)]
use crate::protocol::CreateSessionRequest;
#[cfg(test)]
use crate::protocol::ExtensionRunRequest;
#[cfg(test)]
use crate::protocol::FollowupRequest;
#[cfg(test)]
use crate::protocol::LoadSkillRequest;
#[cfg(test)]
use crate::protocol::QueueMsg;
#[cfg(test)]
use crate::protocol::RecallRequest;
#[cfg(test)]
use crate::protocol::SteerRequest;
#[cfg(test)]
use crate::protocol::StreamEvent;
#[cfg(test)]
use crate::runtime::console::CancellationToken;
#[cfg(test)]
use crate::session::Session;
#[cfg(test)]
use axum::extract::State;
#[cfg(test)]
use axum::http::StatusCode;
use axum::routing::get;
use axum::routing::post;
#[cfg(test)]
use axum::Json;
use axum::Router;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;
#[cfg(test)]
use tokio::sync::mpsc;

mod routes_chat;
mod routes_misc;
mod routes_sessions;
pub(crate) use routes_chat::{approve, cancel, chat, create_queue_pair, followup, recall, steer};
pub(crate) use routes_misc::{
    extensions_reload, extensions_run, get_config, get_extensions, get_git, get_mcp, health,
    list_skills, load_skill, log_requests, mcp_reconnect, require_bearer,
};
#[cfg(test)]
pub(crate) use routes_sessions::steal_wake_and_claim;
pub(crate) use routes_sessions::{
    create_session, list_sessions, reattach, session_events, session_name, session_trace,
    session_undo, session_waive,
};

pub(crate) fn router(state: Arc<DaemonState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/config", get(get_config))
        .route("/api/git", get(get_git))
        .route("/api/mcp", get(get_mcp))
        .route("/api/mcp/{server}/reconnect", post(mcp_reconnect))
        .route("/api/extensions", get(get_extensions))
        .route("/api/extensions/reload", post(extensions_reload))
        .route("/api/extensions/run", post(extensions_run))
        .route("/api/skills", get(list_skills))
        .route("/api/sessions", post(create_session).get(list_sessions))
        .route("/api/sessions/{id}/chat", post(chat))
        .route("/api/sessions/{id}/approve", post(approve))
        .route("/api/sessions/{id}/cancel", post(cancel))
        .route("/api/sessions/{id}/steer", post(steer))
        .route("/api/sessions/{id}/followup", post(followup))
        .route("/api/sessions/{id}/recall", post(recall))
        .route("/api/sessions/{id}/skill", post(load_skill))
        // P10: versioned reattach/replay, P9: trace, P8: undo.
        .route("/api/sessions/{id}/events", get(session_events))
        .route("/api/sessions/{id}/reattach", post(reattach))
        .route("/api/sessions/{id}/trace", get(session_trace))
        .route("/api/sessions/{id}/undo", post(session_undo))
        .route("/api/sessions/{id}/waive", post(session_waive))
        .route("/api/sessions/{id}/name", post(session_name))
        .route("/api/sessions/{id}/shell", post(session_shell))
        .with_state(state)
        .layer(axum::middleware::from_fn(require_bearer))
        .layer(axum::middleware::from_fn(log_requests))
}

#[cfg(test)]
#[cfg(test)]
mod tests;
