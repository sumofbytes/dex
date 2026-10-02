//! Turn-scoped token budgeting that reaches upward into `mcp`/`extensions`
//! for the live tool-schema slices. Pure estimation lives one layer down in
//! [`crate::protocol::tokens`].

use std::path::Path;

use crate::protocol::{BaseContextPart, Skill};

// Re-exports so callers keep one path (`agent::tokens::...`). A few are
// only consumed by the TUI (`format_tokens`) or tests, hence the allow.
#[allow(unused_imports)]
pub(crate) use crate::protocol::tokens::{
    estimate_ephemeral_tokens, estimate_tokens, format_tokens, message_char_len, schema_chars,
    schema_token_estimate, TokenLedger, PER_MESSAGE_OVERHEAD,
};

/// Rough cost of the tool definitions sent with every request.
const TOOL_SCHEMA_TOKENS: u64 = 3200;

/// Per-request tool-schema budget for the compaction threshold: the native
/// flat estimate plus the live MCP + extension slices. The live slices are
/// precomputed at refresh time (never re-serialized here), so this is two
/// cached loads — safe to call per model request.
pub(crate) fn schema_budget_tokens() -> u64 {
    // Without the live slices the compaction threshold ignores the
    // per-request schema cost that actually fills the window.
    TOOL_SCHEMA_TOKENS
        + crate::mcp::cached_schema_tokens()
        + crate::extensions::cached_schema_tokens()
}

/// Approximate always-present context of a fresh session's first turn, split
/// by contributor: the system prompt the daemon would build here (base +
/// project instructions + extension appendix + skills + tool guidelines —
/// skills discovered from the daemon's `skill_dirs()`, not per-request ones)
/// plus the per-request tool-schema budget the compaction threshold uses.
/// Same /4 chars→tokens estimation as the status bar, so the startup figure
/// and the footer's early estimates agree. Empty contributors are omitted;
/// order follows the prompt, schemas last.
pub(crate) fn base_context_breakdown(cwd: &Path, skills: &[Skill]) -> Vec<BaseContextPart> {
    let parts = crate::llm::prompt::system_prompt_parts_for(skills, None, Some(cwd), false);
    let mut out = Vec::new();
    let mut push = |label: &str, section: &str| {
        if !section.is_empty() {
            out.push(BaseContextPart {
                label: label.to_string(),
                tokens: section.len() as u64 / 4,
            });
        }
    };
    push("system prompt", &parts.base);
    push("project instructions", &parts.project_context);
    push("extension prompt", &parts.extensions);
    push("skills", &parts.skills);
    push("tool guidelines", &parts.tool_guidelines);
    out.push(BaseContextPart {
        label: "tool schemas".to_string(),
        tokens: schema_budget_tokens(),
    });
    out
}

#[cfg(test)]
mod base_tests {
    use super::*;

    #[test]
    fn breakdown_sums_to_prompt_plus_schema_budget() {
        let cwd = std::env::current_dir().unwrap_or_default();
        let skills = vec![Skill {
            name: "x".into(),
            description: "does x".into(),
            path: std::path::PathBuf::from("/tmp"),
        }];
        let breakdown = base_context_breakdown(&cwd, &skills);
        let prompt =
            crate::llm::prompt::system_prompt_with_override_for(&skills, None, Some(&cwd), false);
        assert_eq!(
            breakdown.iter().map(|p| p.tokens).sum::<u64>(),
            schema_budget_tokens() + prompt.len() as u64 / 4
        );
        // Every shown contributor carries tokens; labels are unique.
        let mut labels: Vec<_> = breakdown.iter().map(|p| p.label.clone()).collect();
        labels.sort();
        labels.dedup();
        assert_eq!(labels.len(), breakdown.len());
        assert!(breakdown.iter().all(|p| p.tokens > 0));
        assert!(breakdown.iter().any(|p| p.label == "system prompt"));
        assert!(breakdown.iter().any(|p| p.label == "skills"));
        assert_eq!(breakdown.last().unwrap().label, "tool schemas");
    }
}
