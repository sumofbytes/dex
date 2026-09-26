//! Reusable agent turn engine, policies, and context accounting.
//!
//! The dex application supplies tools, persistence, and UI behavior around
//! these types; this crate depends only on the shared AI model API.

mod budgets;
mod compaction;
mod components;
mod modes;
mod plan;
mod runtime;
mod text;
mod tokens;
mod turn;

pub use budgets::{
    tool_budget_exhausted_note, CompactionBudget, ToolRoundBudget, ToolRoundOutcome,
};
pub use compaction::{
    attach_file_section, deterministic_summary, extract_file_ops_from_message, FileOps,
};
pub use components::{
    AlwaysCompact, CompactionTrigger, ConflictDetector, CutConfig, DefaultOverflowDetector,
    DefaultPruneScorer, DeterministicSummarizer, FnCatalog, FnConflictDetector, FnOverflowDetector,
    FnPruneScorer, FnSummarizer, FnTrigger, Harness, HarnessLimits, NeverCompact, NeverConflict,
    OverflowDetector, PairVerdict, PruneScorer, PruneThresholds, SerializeAll, StaticCatalog,
    StaticSummarizer, Summarizer, ToolCatalog, OVERFLOW_PHRASES,
};
pub use modes::{AgentMode, PermissionMode};
pub use plan::Plan;
pub use runtime::{run_turn, AgentHost, AgentTurnError};
pub use text::{clamp_lines, clamp_lines_checked, clip_chars, truncate_text, MAX_LINE_CHARS};
pub use tokens::{
    estimate_ephemeral_tokens, estimate_tokens, format_tokens, message_char_len, schema_chars,
    schema_token_estimate, TokenLedger, PER_MESSAGE_OVERHEAD,
};
pub use turn::{apply_model_turn, AppliedModelTurn};
