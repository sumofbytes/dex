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
    !matches!(
        (requirement, mode),
        (PermissionRequirement::Read, _) | (_, PermissionMode::Trusted)
    )
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
}
