//! Portable metadata and permission policy for built-in coding-agent tools.

use dex_agent_core::PermissionMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionRequirement {
    Read,
    Write,
    Shell,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolMetadata {
    pub read_only: bool,
    pub mutating: bool,
    pub idempotent: bool,
    pub requires_shell: bool,
    pub permission: PermissionRequirement,
}

const READ_ONLY: ToolMetadata = ToolMetadata {
    read_only: true,
    mutating: false,
    idempotent: true,
    requires_shell: false,
    permission: PermissionRequirement::Read,
};

/// Metadata for a built-in tool, excluding host-provided MCP and extension
/// tools whose properties are resolved by the embedding application.
pub fn native_tool_metadata(name: &str) -> Option<ToolMetadata> {
    Some(match name {
        "read" | "grep" | "ffgrep" | "find" | "fffind" | "ls" => READ_ONLY,
        "bash" => ToolMetadata {
            read_only: false,
            mutating: true,
            idempotent: false,
            requires_shell: true,
            permission: PermissionRequirement::Shell,
        },
        "write" | "edit" => ToolMetadata {
            read_only: false,
            mutating: true,
            idempotent: false,
            requires_shell: false,
            permission: PermissionRequirement::Write,
        },
        _ => return None,
    })
}

/// Whether the tool's permission requirement needs human approval in a mode.
pub fn needs_approval(requirement: PermissionRequirement, mode: PermissionMode) -> bool {
    DefaultApprovalPolicy.needs_approval(requirement, mode)
}

/// Overwritable approval metadata + gate.
///
/// The default answers native tools from [`native_tool_metadata`] and gates
/// everything else as unknown (`None`); hosts extend it with MCP/extension
/// rows. Override to auto-approve lists, per-tool modes, or custom UX
/// without forking dispatch.
pub trait ApprovalPolicy: Send + Sync {
    fn metadata(&self, name: &str) -> Option<ToolMetadata>;
    fn needs_approval(&self, requirement: PermissionRequirement, mode: PermissionMode) -> bool;
}

/// Default policy: native metadata + [`needs_approval`] gate.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultApprovalPolicy;

impl ApprovalPolicy for DefaultApprovalPolicy {
    fn metadata(&self, name: &str) -> Option<ToolMetadata> {
        native_tool_metadata(name)
    }

    fn needs_approval(&self, requirement: PermissionRequirement, mode: PermissionMode) -> bool {
        !matches!(
            (requirement, mode),
            (PermissionRequirement::Read, _) | (_, PermissionMode::Trusted)
        )
    }
}

/// Closure policy with an overridable gate.
///
/// `FnApprovalPolicy::new(f)` overrides metadata and keeps the default
/// gate; `.with_gate(g)` (or `FnApprovalPolicy::with_gate(f, g)`) replaces
/// the gate too — e.g. an auto-approve list — without forking dispatch.
pub struct FnApprovalPolicy<F, G = fn(PermissionRequirement, PermissionMode) -> bool> {
    metadata_fn: F,
    gate_fn: G,
}

fn default_gate(requirement: PermissionRequirement, mode: PermissionMode) -> bool {
    DefaultApprovalPolicy.needs_approval(requirement, mode)
}

impl<F> FnApprovalPolicy<F, fn(PermissionRequirement, PermissionMode) -> bool>
where
    F: Fn(&str) -> Option<ToolMetadata> + Send + Sync,
{
    pub fn new(f: F) -> Self {
        Self {
            metadata_fn: f,
            gate_fn: default_gate,
        }
    }

    pub fn with_gate<G>(self, g: G) -> FnApprovalPolicy<F, G>
    where
        G: Fn(PermissionRequirement, PermissionMode) -> bool + Send + Sync,
    {
        FnApprovalPolicy {
            metadata_fn: self.metadata_fn,
            gate_fn: g,
        }
    }
}

impl<F, G> FnApprovalPolicy<F, G>
where
    F: Fn(&str) -> Option<ToolMetadata> + Send + Sync,
    G: Fn(PermissionRequirement, PermissionMode) -> bool + Send + Sync,
{
    pub fn with_parts(metadata_fn: F, gate_fn: G) -> Self {
        Self {
            metadata_fn,
            gate_fn,
        }
    }
}

impl<F, G> ApprovalPolicy for FnApprovalPolicy<F, G>
where
    F: Fn(&str) -> Option<ToolMetadata> + Send + Sync,
    G: Fn(PermissionRequirement, PermissionMode) -> bool + Send + Sync,
{
    fn metadata(&self, name: &str) -> Option<ToolMetadata> {
        (self.metadata_fn)(name)
    }

    fn needs_approval(&self, requirement: PermissionRequirement, mode: PermissionMode) -> bool {
        (self.gate_fn)(requirement, mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_tool_metadata_and_permission_policy_are_explicit() {
        assert_eq!(
            native_tool_metadata("read").unwrap().permission,
            PermissionRequirement::Read
        );
        assert_eq!(
            native_tool_metadata("write").unwrap().permission,
            PermissionRequirement::Write
        );
        assert_eq!(
            native_tool_metadata("bash").unwrap().permission,
            PermissionRequirement::Shell
        );
        assert!(native_tool_metadata("mcp__server__tool").is_none());
        assert!(!needs_approval(
            PermissionRequirement::Read,
            PermissionMode::ReadOnly
        ));
        assert!(needs_approval(
            PermissionRequirement::Write,
            PermissionMode::Ask
        ));
        assert!(!needs_approval(
            PermissionRequirement::Shell,
            PermissionMode::Trusted
        ));
    }

    #[test]
    fn fn_policy_overrides_metadata_and_gate() {
        let policy = FnApprovalPolicy::new(native_tool_metadata).with_gate(|_, _| false);
        assert!(policy.metadata("read").is_some());
        assert!(!policy.needs_approval(PermissionRequirement::Shell, PermissionMode::Ask));
    }
}
