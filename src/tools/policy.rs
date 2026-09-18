#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ToolMetadata {
    pub read_only: bool,
    pub mutating: bool,
    pub idempotent: bool,
    pub requires_shell: bool,
    pub permission: PermissionRequirement,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PermissionRequirement {
    Read,
    Write,
    Shell,
}

/// One row shared by the eight tools that neither mutate nor shell out:
/// `read`, `ls`, the in-process fff tools (`grep`/`ffgrep`/`find`/`fffind`),
/// `obs_recall` (reads the session's observation archive; never touches the
/// workspace) and `update_plan` (pure bookkeeping: validates and echoes the
/// plan snapshot; the agent loop owns the boundary state and compaction
/// decision).
const READONLY: ToolMetadata = ToolMetadata {
    read_only: true,
    mutating: false,
    idempotent: true,
    requires_shell: false,
    permission: PermissionRequirement::Read,
};

pub(crate) fn metadata(name: &str) -> Option<ToolMetadata> {
    if crate::extensions::is_shadowed(name) {
        return Some(ToolMetadata {
            read_only: false,
            mutating: true,
            idempotent: false,
            requires_shell: true,
            permission: PermissionRequirement::Shell,
        });
    }
    metadata_native(name)
}

/// Native gate for a built-in, ignoring extension shadows. `dispatch_tool`
/// uses this when re-dispatching the original from inside a shadow
/// (`dex.tools.call_original`): the shadow call already cleared its own
/// Shell gate, and the native requirement is what the built-in itself needs
/// — re-reading the shadow's row here would prompt twice for one wrapped
/// call in `ask-shell` mode.
pub(crate) fn metadata_native(name: &str) -> Option<ToolMetadata> {
    Some(match name {
        "read" | "grep" | "ffgrep" | "find" | "fffind" | "ls" | "obs_recall" | "update_plan" => {
            READONLY
        }
        "git" => ToolMetadata {
            read_only: true,
            mutating: false,
            idempotent: true,
            requires_shell: true,
            permission: PermissionRequirement::Read,
        },
        "chain" => ToolMetadata {
            read_only: true,
            mutating: false,
            idempotent: false,
            requires_shell: true,
            permission: PermissionRequirement::Read,
        },
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
        // Extension tools are untrusted third-party code in a sandbox:
        // most restrictive gate (`ask` unless trusted), same as shell/MCP.
        // Resolved dynamically so loaded extensions don't need a static
        // entry each (`lua__` is the deprecated alias for `ext__`).
        _ if crate::extensions::is_extension_tool(name) => ToolMetadata {
            read_only: false,
            mutating: true,
            idempotent: false,
            requires_shell: true,
            permission: PermissionRequirement::Shell,
        },
        // MCP tools are external processes: most restrictive gate (`ask`
        // unless trusted), same as shell. Resolved dynamically so cached
        // server tools don't need a static entry each.
        _ if name.starts_with("mcp__") => ToolMetadata {
            read_only: false,
            mutating: true,
            idempotent: false,
            requires_shell: true,
            permission: PermissionRequirement::Shell,
        },
        _ => return None,
    })
}
