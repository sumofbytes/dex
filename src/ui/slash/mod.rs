#![allow(dead_code)]

#[cfg(test)]
use super::App;
#[cfg(test)]
use super::InputField;
#[cfg(test)]
use crate::protocol::ChatMessage;
#[cfg(test)]
use crate::session::Session;

mod commands;
mod completion;
mod parser;

#[allow(unused_imports)] // used from `ui::mod`/`remote` test modules by full path
pub(super) use commands::{
    apply_session_state, cmd_help, handle_slash, parse, reset_session_state, SlashCommand,
};
pub(super) use completion::{
    complete_slash, dismiss_slash, expand_bare_command, popup_open, slash_suggestions,
    suggestion_label, SlashCache, EXPAND_ON_ENTER,
};
#[allow(unused_imports)]
pub(super) use parser::COMMANDS;

#[cfg(test)]
use commands::commands_help_line;

#[cfg(test)]
mod tests;
