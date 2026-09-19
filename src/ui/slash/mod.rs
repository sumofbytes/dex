#![allow(dead_code)]

#[cfg(test)]
use super::App;
#[cfg(test)]
use super::InputField;
#[cfg(test)]
use crate::protocol::ChatMessage;
#[cfg(test)]
use crate::session::Session;

mod completion;
mod parser;

#[allow(unused_imports)] // used from `ui::mod`/`remote` test modules by full path
pub(super) use completion::{
    apply_session_state, cmd_help, complete_slash, dismiss_slash, expand_bare_command,
    handle_slash, parse, popup_open, reset_session_state, slash_suggestions, suggestion_label,
    SlashCache, SlashCommand, EXPAND_ON_ENTER,
};
#[allow(unused_imports)]
pub(super) use parser::COMMANDS;

#[cfg(test)]
use completion::commands_help_line;

#[cfg(test)]
mod tests;
