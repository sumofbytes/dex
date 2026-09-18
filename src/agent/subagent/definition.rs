use std::collections::BTreeSet;
use std::time::Duration;

use crate::skills::unquote;

/// Default child timeout (§14): a run exceeding it ends `TimedOut` with a
/// synthesized partial-status error. Per-definition `timeout_secs`
/// overrides it.
pub(crate) const DEFAULT_AGENT_TIMEOUT: Duration = Duration::from_secs(600);

/// Exact tool names a definition may grant. Wildcard entries (`mcp__gh__*`)
/// and bare `mcp__*` are accepted without further checks — the policy layer
/// must not assume a closed enum (§11); MCP servers arrive dynamically.
const KNOWN_TOOLS: &[&str] = &[
    "read", "bash", "write", "edit", "grep", "ffgrep", "find", "fffind", "ls", "git", "chain",
];

/// Safe default when a definition names no tools: the explorer trio.
/// Read-only, never prompts — a definition that grants nothing dangerous
/// by omission.
pub(crate) const READ_ONLY_TOOLS: &[&str] = &["read", "grep", "find"];

/// Alias frontmatter entries → canonical registry names. `ToolFilter::allows`
/// matches exactly and the schema exposes only canonical names, so an alias
/// left in an allowlist would deny every call. Pre-rename user files keep
/// parsing; they canonicalize here. If `KNOWN_TOOLS` ever drops the legacy
/// names, the unknown-tool check runs before this mapping — keep both in
/// sync or old definitions start failing validation.
/// Parse a frontmatter value that must be a strictly positive integer. `key`
/// names the field and `want` is the error's human hint, so each key keeps its
/// own wording (both `max_tool_iterations` and `timeout_secs` gate a ladder).
fn positive_int<T>(agent: &str, key: &str, raw: &str, want: &str) -> Result<T, String>
where
    T: std::str::FromStr + PartialOrd + Default,
{
    let invalid = || format!("agent '{agent}' has invalid {key} '{raw}': want {want}");
    let n: T = raw.trim().parse().map_err(|_| invalid())?;
    if n > T::default() {
        Ok(n)
    } else {
        Err(invalid())
    }
}

fn canonical_tool(entry: &str) -> &str {
    match entry {
        "ffgrep" => "grep",
        "fffind" => "find",
        other => other,
    }
}

/// What an agent is: declarative, no runtime state (plan §4).
#[derive(Clone, Debug)]
pub(crate) struct AgentDefinition {
    /// Routing key (`delegate(agent, …)` resolves it) + session-path slug.
    pub(crate) name: String,
    /// Shown to the parent model for routing.
    pub(crate) description: String,
    /// Body: the child's system-prompt persona.
    pub(crate) prompt: String,
    /// Same single knob as the main agent (`provider/model` against the
    /// models.dev catalog); `None` inherits the parent's resolved model.
    /// Resolved through the existing config path at spawn (Phase 7) — no
    /// aliases the catalog doesn't have, zero provider-specific logic here.
    pub(crate) model: Option<String>,
    /// Allowlist against the real registry. The spawn filter strips
    /// delegation tools out of this set, then re-adds them when the child
    /// may delegate (depth + 1 under the cap) — so a child filter may name
    /// tools the parent schema hides (e.g. reviewer `git` under a parent
    /// without `DEX_EXTRA_TOOLS`).
    pub(crate) tools: BTreeSet<String>,
    /// Feeds the existing turn budget (`max_tool_iterations()`); `None`
    /// keeps the default. Same knob re-parameterized, not a second counter.
    pub(crate) max_tool_iterations: Option<u32>,
    /// Per-definition timeout; defaults to [`DEFAULT_AGENT_TIMEOUT`].
    pub(crate) timeout: Duration,
}

/// Parse one `markdown + frontmatter` definition — the shape user-defined
/// agent files take post-V1 (§19), exercised today by the embedded
/// built-ins so the parser is live code, not a stub. Mirrors
/// `parse_skill`: `---` delimiters, `key: value` scalars, [`unquote`].
///
/// Recognized keys: `name` (required, ascii alnum/`-`/`_`), `description`
/// (required — the parent routes on it), `model`, `tools` (comma list,
/// defaults to the read-only trio), `max_tool_iterations`, `timeout_secs`.
/// The body after the closing `---` is the persona prompt (required,
/// non-empty). A child always inherits its parent's permission mode and
/// never re-enters on its own: `permissions:` / `recover*` keys are
/// ignored when present.
pub(crate) fn parse_definition(text: &str) -> Result<AgentDefinition, String> {
    let mut lines = text.lines();
    match lines.next() {
        Some(first) if first.trim() == "---" => {}
        _ => {
            return Err("agent definition must start with a --- frontmatter block".to_string());
        }
    }
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut model: Option<String> = None;
    let mut tools: Option<String> = None;
    let mut max_tool_iterations: Option<String> = None;
    let mut timeout_secs: Option<String> = None;
    let mut prompt: Option<String> = None;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            prompt = Some(lines.collect::<Vec<_>>().join("\n"));
            break;
        }
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = unquote(value.trim());
        match key.trim() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            "model" => model = Some(value),
            "tools" => tools = Some(value),
            "max_tool_iterations" => max_tool_iterations = Some(value),
            "timeout_secs" => timeout_secs = Some(value),
            // Forward-compatible: unknown keys are ignored (user files may
            // carry post-V1 keys before this build understands them —
            // including the removed `permissions:` / `recover*` keys).
            _ => {}
        }
    }
    let name = name
        .filter(|n| !n.is_empty())
        .ok_or_else(|| "agent definition is missing a 'name' frontmatter key".to_string())?;
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "invalid agent name '{name}': use ascii letters, digits, '-' or '_'"
        ));
    }
    let description = description.filter(|d| !d.is_empty()).ok_or_else(|| {
        format!("agent '{name}' is missing a 'description': the parent model routes on it")
    })?;
    let prompt = prompt
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .ok_or_else(|| format!("agent '{name}' has an empty persona prompt"))?;
    let max_tool_iterations = match max_tool_iterations {
        None => None,
        Some(raw) => Some(positive_int(
            &name,
            "max_tool_iterations",
            &raw,
            "a positive integer",
        )?),
    };
    let timeout = match timeout_secs {
        None => DEFAULT_AGENT_TIMEOUT,
        Some(raw) => Duration::from_secs(positive_int(
            &name,
            "timeout_secs",
            &raw,
            "positive seconds",
        )?),
    };
    let tools = match tools {
        None => READ_ONLY_TOOLS.iter().map(|t| t.to_string()).collect(),
        Some(raw) => {
            let mut set = BTreeSet::new();
            for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
                let wildcard = entry.strip_suffix('*');
                let known = KNOWN_TOOLS.contains(&entry)
                    || wildcard.is_some_and(|prefix| !prefix.is_empty());
                if !known {
                    return Err(format!(
                        "agent '{name}' grants unknown tool '{entry}': want one of {} or a 'prefix*' wildcard",
                        KNOWN_TOOLS.join(", ")
                    ));
                }
                set.insert(canonical_tool(entry).to_string());
            }
            if set.is_empty() {
                return Err(format!(
                    "agent '{name}' grants no tools: omit 'tools' for the read-only default"
                ));
            }
            set
        }
    };
    let model = model.filter(|m| !m.is_empty());
    Ok(AgentDefinition {
        name,
        description,
        prompt,
        model,
        tools,
        max_tool_iterations,
        timeout,
    })
}

const EXPLORER_MD: &str = "---\nname: explorer\ndescription: Understand code without modifying it. Give it a question about the codebase; it returns findings in prose with file paths. Read-only: never modifies files or runs commands.\ntools: read, grep, find\n---\nYou are an explorer. Answer the task with findings in prose: file paths, relevant snippets, risks. Never modify files or run shell commands — you do not have those tools. If the task needs something outside your tools, say so in your result instead of working around it.\n";

const REVIEWER_MD: &str = "---\nname: reviewer\ndescription: Review a diff or change for correctness and regressions. Give it what changed; it returns findings in prose. Read-only.\ntools: read, grep, find, git\n---\nYou are a reviewer. Review the change for correctness, regressions, and missed edge cases; report findings in prose with file paths and line references. Never modify files or run shell commands outside your tools.\n";

const TESTER_MD: &str = "---\nname: tester\ndescription: Investigate and run relevant tests. Give it what to verify; it reports pass/fail plus failures in prose. May run shell commands: they run under a trusted permission policy and are auto-denied otherwise.\ntools: read, grep, find, bash\n---\nYou are a tester. Investigate the requested area and run the relevant tests with your shell; report pass/fail plus the failures in prose with file paths. Keep commands read-only in spirit (run tests, do not deploy or delete). If a command is denied, report that instead of working around it.\n";

/// V1a ships three built-ins, zero required configuration (§19). Parsed
/// through the same frontmatter parser user files will use, so the parser
/// stays live. Child allowlists are independent of the parent prompt-token
/// hiding (`DEX_EXTRA_TOOLS`): the reviewer keeps `git` even when the
/// parent schema hides it.
pub(crate) fn builtin_definitions() -> Vec<AgentDefinition> {
    [EXPLORER_MD, REVIEWER_MD, TESTER_MD]
        .into_iter()
        .map(|md| parse_definition(md).expect("built-in agent must parse"))
        .collect::<Vec<_>>()
}

/// Resolve a definition by name. Unknown names fail with the available
/// agent list — clean rejection, no fallback (§10.1).
pub(crate) fn find_definition(name: &str) -> Result<AgentDefinition, String> {
    let defs = builtin_definitions();
    defs.into_iter().find(|d| d.name == name).ok_or_else(|| {
        let available = builtin_definitions()
            .iter()
            .map(|d| d.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
        format!("unknown agent '{name}'; available agents: {available}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "---\nname: scout\ndescription: Finds things.\nmodel: opencode/gpt-5\ntools: read, ffgrep, mcp__gh__*\nmax_tool_iterations: 50\ntimeout_secs: 120\n---\nYou find things.\n";

    #[test]
    fn parses_all_frontmatter_fields() {
        let def = parse_definition(VALID).unwrap();
        assert_eq!(def.name, "scout");
        assert_eq!(def.description, "Finds things.");
        assert_eq!(def.model.as_deref(), Some("opencode/gpt-5"));
        assert_eq!(def.prompt, "You find things.");
        assert_eq!(def.max_tool_iterations, Some(50));
        assert_eq!(def.timeout, Duration::from_secs(120));
        assert!(def.tools.contains("read"));
        // Frontmatter aliases parse and canonicalize to schema names.
        assert!(def.tools.contains("grep"), "{:?}", def.tools);
        assert!(!def.tools.contains("ffgrep"));
        assert!(def.tools.contains("mcp__gh__*"));
    }

    #[test]
    fn omissions_take_safe_defaults() {
        let def = parse_definition(
            "---\nname: scout\ndescription: Finds things.\n---\nYou find things.\n",
        )
        .unwrap();
        assert_eq!(def.model, None);
        assert_eq!(def.max_tool_iterations, None);
        assert_eq!(def.timeout, DEFAULT_AGENT_TIMEOUT);
        // Default is the read-only trio (order-free set comparison).
        assert_eq!(
            def.tools,
            READ_ONLY_TOOLS
                .iter()
                .map(|t| t.to_string())
                .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn removed_supervision_keys_are_ignored() {
        // Old files may still carry `permissions:` / `recover*` keys and
        // even indented `supervision:` blocks — all ignored, never an
        // error, so upgrades do not break existing definitions.
        let def = parse_definition(
            "---\nname: x\ndescription: d\npermissions: inherit\nrecover: resume\nrecover_max: 3\nrecover_window_secs: 120\n---\nbody\n",
        )
        .unwrap();
        assert_eq!(def.name, "x");
        let def = parse_definition(
            "---\nname: x\ndescription: d\nsupervision:\n  recover: resume\n---\nbody\n",
        )
        .unwrap();
        assert_eq!(def.name, "x");
        // Bogus values are ignored with the keys — no validation.
        for ignored in [
            "---\nname: x\ndescription: d\nrecover: always\n---\nbody\n",
            "---\nname: x\ndescription: d\npermissions: escalate\n---\nbody\n",
        ] {
            assert!(parse_definition(ignored).is_ok(), "{ignored}");
        }
    }

    #[test]
    fn rejects_missing_name_description_or_prompt() {
        assert!(parse_definition("no frontmatter").is_err());
        assert!(parse_definition("---\ndescription: d\n---\nbody\n").is_err());
        assert!(parse_definition("---\nname: x\n---\nbody\n").is_err());
        assert!(parse_definition("---\nname: x\ndescription: d\n---\n   \n").is_err());
        assert!(parse_definition("---\nname: 'bad name!'\ndescription: d\n---\nbody\n").is_err());
    }

    #[test]
    fn rejects_bad_tool_budget_and_timeout() {
        let base =
            |tools: &str| format!("---\nname: x\ndescription: d\ntools: {tools}\n---\nbody\n");
        assert!(parse_definition(&base("read, nope")).is_err());
        assert!(parse_definition(&base("")).is_err());
        assert!(parse_definition(
            "---\nname: x\ndescription: d\nmax_tool_iterations: 0\n---\nbody\n"
        )
        .is_err());
        assert!(parse_definition(
            "---\nname: x\ndescription: d\nmax_tool_iterations: many\n---\nbody\n"
        )
        .is_err());
        assert!(
            parse_definition("---\nname: x\ndescription: d\ntimeout_secs: 0\n---\nbody\n").is_err()
        );
    }

    #[test]
    fn builtins_load_with_documented_tool_sets() {
        let defs = builtin_definitions();
        let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["explorer", "reviewer", "tester"]);
        for def in &defs {
            assert!(!def.description.is_empty(), "{}", def.name);
            assert!(!def.prompt.is_empty(), "{}", def.name);
            assert_eq!(def.model, None, "{} inherits the model", def.name);
        }
        let tools = |n: &str| {
            defs.iter()
                .find(|d| d.name == n)
                .unwrap()
                .tools
                .iter()
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(tools("explorer"), ["find", "grep", "read"]);
        // The reviewer keeps `git` regardless of the parent's
        // `DEX_EXTRA_TOOLS` prompt-token hiding (DEX-9).
        assert_eq!(tools("reviewer"), ["find", "git", "grep", "read"]);
        assert_eq!(tools("tester"), ["bash", "find", "grep", "read"]);
    }

    #[test]
    fn unknown_names_reject_with_the_available_list() {
        let err = find_definition("oracle").unwrap_err();
        assert!(err.contains("unknown agent 'oracle'"), "{err}");
        for name in ["explorer", "reviewer", "tester"] {
            assert!(err.contains(name), "{err}");
        }
        assert!(find_definition("explorer").is_ok());
    }
}
