//! Host-independent policy applied after a coding-agent tool finishes.

use std::collections::HashMap;

/// Result returned by a host's tool executor. `ok` is decided where the
/// execution status is known; it is never inferred from output text.
#[derive(Clone, Debug)]
pub struct ToolExecutionResult {
    pub text: String,
    pub ok: bool,
    /// Display-only diff captured before a file mutation.
    pub diff: Option<String>,
}

/// Result after coding-agent cache and repeated-call policies are applied.
#[derive(Clone, Debug)]
pub struct NormalizedToolResult {
    pub text: String,
    pub ok: bool,
    pub diff: Option<String>,
    pub cache_hit: bool,
    /// Whether normalization mutated the supplied cache.
    pub cache_changed: bool,
}

/// Apply cache and repeated-call policy to one completed tool call.
///
/// The host supplies a cache key that includes any environment-specific
/// freshness token (for example, a file metadata fingerprint). `cache` and
/// `recent_calls` remain host-owned so persistence and turn lifetime stay
/// with the embedding application.
pub fn normalize_tool_result(
    cache: &mut HashMap<String, String>,
    recent_calls: &mut Vec<String>,
    cache_key: String,
    tool_name: &str,
    outcome: ToolExecutionResult,
) -> NormalizedToolResult {
    ResultPolicy::default().normalize(cache, recent_calls, cache_key, tool_name, outcome)
}

/// Overwritable cache + repeat-call policy.
///
/// Defaults match the historic behavior (cacheable reads, `write`/`edit`
/// invalidate, 6-deep recent window, ≥3 repeats rejected). Override to
/// cache more tools, disable the repeat guard, or scope keys per workspace.
#[derive(Clone, Copy, Debug)]
pub struct ResultPolicy {
    pub recent_window: usize,
    pub repeat_limit: usize,
}

impl Default for ResultPolicy {
    fn default() -> Self {
        Self {
            recent_window: 6,
            repeat_limit: 3,
        }
    }
}

impl ResultPolicy {
    pub fn with_recent_window(mut self, window: usize) -> Self {
        self.recent_window = window;
        self
    }

    pub fn with_repeat_limit(mut self, limit: usize) -> Self {
        self.repeat_limit = limit;
        self
    }

    /// Without the repeat guard (repeat_limit = usize::MAX).
    pub fn without_repeat_guard(mut self) -> Self {
        self.repeat_limit = usize::MAX;
        self
    }

    pub fn is_cacheable(tool_name: &str) -> bool {
        matches!(
            tool_name,
            "read" | "grep" | "ffgrep" | "find" | "fffind" | "ls"
        )
    }

    pub fn is_mutating(tool_name: &str) -> bool {
        matches!(tool_name, "write" | "edit")
    }

    pub fn normalize(
        &self,
        cache: &mut HashMap<String, String>,
        recent_calls: &mut Vec<String>,
        cache_key: String,
        tool_name: &str,
        outcome: ToolExecutionResult,
    ) -> NormalizedToolResult {
        let succeeded = outcome.ok;
        if succeeded {
            while recent_calls.len() >= self.recent_window.max(1) {
                recent_calls.remove(0);
            }
            recent_calls.push(cache_key.clone());
        }
        let repeated_count = recent_calls.iter().filter(|key| **key == cache_key).count();
        let cacheable = Self::is_cacheable(tool_name);
        let mut cache_hit = false;
        let mut cache_changed = false;
        let mut ok = succeeded;
        let text = if repeated_count >= self.repeat_limit {
            ok = false;
            "Error: repeated identical tool call; choose a different action or finish.".to_string()
        } else if cacheable && succeeded {
            if let Some(cached) = cache.get(&cache_key) {
                cache_hit = true;
                cached.clone()
            } else {
                cache_changed = true;
                cache.insert(cache_key, outcome.text.clone());
                outcome.text
            }
        } else {
            if Self::is_mutating(tool_name) {
                cache_changed = !cache.is_empty();
                cache.clear();
            }
            outcome.text
        };

        NormalizedToolResult {
            text,
            ok,
            diff: outcome.diff,
            cache_hit,
            cache_changed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn success(text: &str) -> ToolExecutionResult {
        ToolExecutionResult {
            text: text.into(),
            ok: true,
            diff: None,
        }
    }

    #[test]
    fn caches_reads_and_marks_cache_hits() {
        let mut cache = HashMap::new();
        let mut recent = Vec::new();
        let first = normalize_tool_result(
            &mut cache,
            &mut recent,
            "read:file".into(),
            "read",
            success("contents"),
        );
        let second = normalize_tool_result(
            &mut cache,
            &mut recent,
            "read:file".into(),
            "read",
            success("fresh contents"),
        );
        assert_eq!(first.text, "contents");
        assert!(!first.cache_hit);
        assert_eq!(second.text, "contents");
        assert!(second.cache_hit);
    }

    #[test]
    fn repeated_call_guard_and_mutation_invalidation_are_policy_owned() {
        let mut cache = HashMap::from([("read:file".into(), "stale".into())]);
        let mut recent = vec!["read:file".into(), "read:file".into()];
        let repeated = normalize_tool_result(
            &mut cache,
            &mut recent,
            "read:file".into(),
            "read",
            success("new"),
        );
        assert!(!repeated.ok);
        assert!(repeated.text.contains("repeated identical"));

        normalize_tool_result(
            &mut cache,
            &mut recent,
            "write:file".into(),
            "write",
            success("written"),
        );
        assert!(cache.is_empty());
    }
}
