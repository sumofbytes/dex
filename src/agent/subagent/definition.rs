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
pub(crate) const READ_ONLY_TOOLS: &[&str] = &["read", "ffgrep", "fffind"];

/// Permission policy inheritance (plan §12). V1a has exactly one rule —
/// the child inherits the parent's mode — as an enum (not a bool) so
/// post-V1 policies extend the shape instead of re-plumbing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PermissionInherit {
    Inherit,
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
    /// Allowlist against the real registry; clamped against the parent's
    /// at spawn (§11). Never contains delegation tools in a child filter —
    /// that exclusion is enforced at dispatch, not trusted from this set.
    pub(crate) tools: BTreeSet<String>,
    /// Subset clamp of the parent policy (§12).
    pub(crate) permissions: PermissionInherit,
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
/// defaults to the read-only trio), `permissions` (`inherit`, the only V1
/// rule), `max_tool_iterations`, `timeout_secs`. The body after the
/// closing `---` is the persona prompt (required, non-empty).
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
    let mut permissions: Option<String> = None;
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
            "permissions" => permissions = Some(value),
            "max_tool_iterations" => max_tool_iterations = Some(value),
            "timeout_secs" => timeout_secs = Some(value),
            // Forward-compatible: unknown keys are ignored (user files may
            // carry post-V1 keys before this build understands them).
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
    let permissions = match permissions.as_deref().map(str::trim) {
        None | Some("") | Some("inherit") => PermissionInherit::Inherit,
        Some(other) => {
            return Err(format!(
                "agent '{name}' has unknown permissions '{other}': V1 supports only 'inherit'"
            ));
        }
    };
    let max_tool_iterations = match max_tool_iterations {
        None => None,
        Some(raw) => {
            let n: u32 = raw
                .trim()
                .parse()
                .map_err(|_| {
                    format!(
                        "agent '{name}' has invalid max_tool_iterations '{raw}': want a positive integer"
                    )
                })
                .and_then(|n| {
                    if n > 0 {
                        Ok(n)
                    } else {
                        Err(format!(
                            "agent '{name}' has invalid max_tool_iterations '{raw}': want a positive integer"
                        ))
                    }
                })?;
            Some(n)
        }
    };
    let timeout = match timeout_secs {
        None => DEFAULT_AGENT_TIMEOUT,
        Some(raw) => {
            let secs: u64 = raw.trim().parse().map_err(|_| {
                format!("agent '{name}' has invalid timeout_secs '{raw}': want positive seconds")
            })?;
            if secs == 0 {
                return Err(format!(
                    "agent '{name}' has invalid timeout_secs '{raw}': want positive seconds"
                ));
            }
            Duration::from_secs(secs)
        }
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
                set.insert(entry.to_string());
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
        permissions,
        max_tool_iterations,
        timeout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "---\nname: scout\ndescription: Finds things.\nmodel: opencode/gpt-5\ntools: read, ffgrep, mcp__gh__*\npermissions: inherit\nmax_tool_iterations: 50\ntimeout_secs: 120\n---\nYou find things.\n";

    #[test]
    fn parses_all_frontmatter_fields() {
        let def = parse_definition(VALID).unwrap();
        assert_eq!(def.name, "scout");
        assert_eq!(def.description, "Finds things.");
        assert_eq!(def.model.as_deref(), Some("opencode/gpt-5"));
        assert_eq!(def.prompt, "You find things.");
        assert_eq!(def.permissions, PermissionInherit::Inherit);
        assert_eq!(def.max_tool_iterations, Some(50));
        assert_eq!(def.timeout, Duration::from_secs(120));
        assert!(def.tools.contains("read"));
        assert!(def.tools.contains("ffgrep"));
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
        assert_eq!(def.permissions, PermissionInherit::Inherit);
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
    fn rejects_missing_name_description_or_prompt() {
        assert!(parse_definition("no frontmatter").is_err());
        assert!(parse_definition("---\ndescription: d\n---\nbody\n").is_err());
        assert!(parse_definition("---\nname: x\n---\nbody\n").is_err());
        assert!(parse_definition("---\nname: x\ndescription: d\n---\n   \n").is_err());
        assert!(parse_definition("---\nname: 'bad name!'\ndescription: d\n---\nbody\n").is_err());
    }

    #[test]
    fn rejects_bad_tool_budget_timeout_and_permissions() {
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
        assert!(parse_definition(
            "---\nname: x\ndescription: d\npermissions: escalate\n---\nbody\n"
        )
        .is_err());
    }
}
