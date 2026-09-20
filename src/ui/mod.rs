#[cfg(test)]
use crate::protocol::SinkLine;
#[cfg(test)]
use crate::session::Session;
#[cfg(test)]
use ratatui::layout::Rect;
#[cfg(test)]
use ratatui::style::Color;
#[cfg(test)]
use ratatui::style::Style;
#[cfg(test)]
use ratatui::text::Line;
#[cfg(test)]
use ratatui::text::Span;
#[cfg(test)]
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;

pub(crate) mod herdr;
pub(crate) mod input;
pub(crate) mod remote;
pub(crate) mod render;
pub(crate) mod slash;
pub(crate) mod status;
pub(crate) mod theme;
pub(crate) mod wrapping;

pub(crate) use self::input::InputField;
#[cfg(test)]
pub(crate) use crate::agent::state::ToolState;
#[cfg(test)]
pub(crate) use crate::llm::config::LlmConfig;
pub(crate) use app::{
    TerminalCleanup, APPROVAL_HEIGHT, HORIZONTAL_GUTTER, INPUT_BORDER_ROWS, INPUT_MIN_ROWS,
    INPUT_PAD_Y, INPUT_PROMPT, INPUT_PROMPT_WIDTH, INPUT_STATUS_GUTTER, STATUS_CONTENT_ROWS,
    TAB_WIDTH, VERTICAL_GUTTER,
};
#[cfg(test)]
pub(crate) use app::{STREAM_FLUSH_INTERVAL, THINKING_TEXT_CAP, THINKING_TEXT_SLACK};
pub(crate) use remote::{mark_launch_start, run_ratatui_repl_with_remote};
pub(crate) use render::view;
pub(crate) use transcript::{
    bump_thinking_stamps, deny_all_approvals, push_info, push_info_line, resolve_approval,
    scroll_transcript,
};
#[cfg(test)]
pub(crate) use transcript::{move_activity_to_tail, BANNER};

pub(crate) mod app;
pub(crate) mod format;
pub(crate) mod selection;
pub(crate) mod transcript;
pub(crate) use app::{
    format_tokens, AgentChip, App, EnableMouseScroll, PendingApproval, TranscriptBlock,
    WrappedBlock, NOTICE_LIFETIME, TRANSCRIPT_INDENT,
};
#[cfg(test)]
pub(crate) use selection::{b64, line_width};
pub(crate) use selection::{
    last_col, line_selection_text, mouse_display_cell, selection_text, word_bounds, Selection,
};
pub(crate) use transcript::{
    append_sink_line, close_thinking, flush_assistant, indent_transcript_line, push_banner,
    rebuild_transcript, render_message_slice, render_user_prompt, settle_activity, start_activity,
    transcript_indent,
};

#[cfg(test)]
mod tests;
