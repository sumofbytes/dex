#[cfg(test)]
use super::slash::popup_open;
#[cfg(test)]
use super::App;
#[cfg(test)]
use crate::client::http::ChatOptions;
#[cfg(test)]
use crate::client::http::DaemonClient;
#[cfg(test)]
use crate::protocol::AgentMode;
#[cfg(test)]
use crate::protocol::ApiProtocol;
#[cfg(test)]
use crate::protocol::ApprovalDecision;
#[cfg(test)]
use crate::protocol::PermissionMode;
#[cfg(test)]
use crate::protocol::Provider;
#[cfg(test)]
#[cfg(test)]
use crate::runtime::console::DIM;
#[cfg(test)]
use crate::runtime::console::RESET;
#[cfg(test)]
use crate::session::Session;
#[cfg(test)]
use crossterm::event::KeyCode;
#[cfg(test)]
#[cfg(test)]
use crossterm::event::KeyModifiers;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
#[cfg(test)]
use std::sync::atomic::AtomicU64;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::time::Instant;
#[cfg(test)]
use tokio::sync::mpsc;

mod boot;
mod commands;
mod input;
mod keys;
mod osc;
mod pollers;
mod repl;
mod resume;
mod state;
mod worker;

pub(crate) use repl::run_ratatui_repl_with_remote;
pub(crate) use state::mark_launch_start;

#[cfg(test)]
pub(crate) use boot::{launch_time_line, skills_listing_line};
#[cfg(test)]
pub(crate) use commands::{
    handle_remote_slash, is_builtin_command, remote_unknown_or_extension, split_remote_extension,
};
#[cfg(test)]
pub(crate) use input::{find_local_session_file, output_rate};
#[cfg(test)]
pub(crate) use keys::{handle_key, handle_paste, recall_candidate, recall_queued};
#[cfg(test)]
pub(crate) use osc::is_osc_report;
#[cfg(test)]
pub(crate) use pollers::{premature_close_error, spawn_git_poller, GIT_REFRESH_INTERVAL};
#[cfg(test)]
pub(crate) use resume::{connection_label, format_resume_hint, resume_command, shell_quote};
#[cfg(test)]
pub(crate) use state::{RemoteApp, WorkerMessage};

#[cfg(test)]
mod tests;
