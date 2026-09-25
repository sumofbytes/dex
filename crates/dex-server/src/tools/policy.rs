use dex_coding_agent::{native_tool_metadata, needs_approval};

pub use dex_coding_agent::{PermissionRequirement, ToolMetadata};

pub fn metadata(name: &str) -> Option<ToolMetadata> {
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
/// call in `ask` mode.
pub fn metadata_native(name: &str) -> Option<ToolMetadata> {
    if let Some(metadata) = native_tool_metadata(name) {
        return Some(metadata);
    }
    Some(match name {
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

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::protocol::{ApprovalDecision, ApprovalRequest, PermissionMode};
use crate::runtime::cancel::{wait_cancelled, CancellationSource};
use crate::runtime::console::Console;

use super::error::ToolError;
use super::meta::Policy;

impl Policy {
    pub fn trusted() -> Self {
        Self {
            mode: PermissionMode::Trusted,
            console: None,
            agent: None,
        }
    }

    pub fn turn(mode: PermissionMode, console: &Console) -> Self {
        Self {
            mode,
            console: Some(console.clone()),
            agent: None,
        }
    }
}

/// Phase 0 approval gate: dispatch consults the turn's policy before any
/// tool runs. Reads always pass; `trusted` passes everything; otherwise
/// mutating tools park an `ApprovalRequest` on the console's approval
/// channel and block for the verdict, with session approvals
/// short-circuiting first. `read-only` rejects up front.
/// Denial, cancellation, and no-channel surface as `ToolError::Denied` —
/// the loop records it as a failed tool result, and the post-fan-out
/// cancellation check still unwinds a turn cancelled mid-prompt.
pub async fn enforce_policy(
    name: &str,
    args: &Map<String, Value>,
    requirement: PermissionRequirement,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
) -> Result<(), ToolError> {
    if !needs_approval(requirement, policy.mode) {
        return Ok(());
    }
    if policy.mode == PermissionMode::ReadOnly {
        return Err(ToolError::Denied(format!(
            // Mode-agnostic: `read-only` is reachable via plan mode, but also
            // via `--permission read-only` / `DEX_PERMISSION` where there is
            // no plan to present.
            "{name} is blocked in read-only mode — read/search tools only; propose the change for the user to apply"
        )));
    }
    let Some(console) = policy.console.as_ref() else {
        return Err(ToolError::Denied(format!(
            "{name} needs approval ({} mode) but no approval channel is attached",
            policy.mode.as_str()
        )));
    };
    let input = serde_json::Value::Object(args.clone()).to_string();
    if console.session_approved(name, &input) {
        return Ok(());
    }
    let Some(sender) = console.approval() else {
        return Err(ToolError::Denied(format!(
            "{name} needs approval but no approval channel is attached"
        )));
    };
    let (response_tx, mut response_rx) = tokio::sync::mpsc::channel(1);
    // §12 V1b: a child console stamps its identity, so the daemon parks the
    // request under the child's id and labels the prompt with its name.
    let (agent_id, agent) = match console.agent.as_ref() {
        Some((id, label)) => (Some(id.clone()), Some(label.clone())),
        None => (None, None),
    };
    sender
        .send(ApprovalRequest {
            name: name.to_string(),
            input: input.clone(),
            agent_id,
            agent,
            response: response_tx,
        })
        .await
        .map_err(|_| {
            ToolError::Denied(format!(
                "{name} needs approval but the approval channel is closed"
            ))
        })?;
    let decision = tokio::select! {
        () = wait_cancelled(cancel) => {
            return Err(ToolError::Denied(
                "cancelled while awaiting approval".to_string(),
            ));
        }
        verdict = response_rx.recv() => verdict,
    };
    match decision {
        Some(ApprovalDecision::AllowOnce) => Ok(()),
        Some(ApprovalDecision::AllowSession) => {
            // Same-turn repeats skip the prompt via the turn policy's
            // console copy; the daemon's /approve handler persists to its
            // session map for later turns (the console is per-turn).
            console.record_session_approval(name, &input);
            Ok(())
        }
        Some(ApprovalDecision::Deny) => {
            Err(ToolError::Denied(format!("{name} denied by approver")))
        }
        None => Err(ToolError::Denied(format!(
            "{name} approval went unanswered; treating as denied"
        ))),
    }
}

/// Allowlist enforced at dispatch (Phase 2 runtime extraction; plan §11).
/// `owner` names the agent the set belongs to and appears in denial errors
/// so the model can self-correct; `allowed` holds exact tool names plus
/// `prefix*` wildcards (`mcp__gh__*` covers one server; `mcp__*` covers all
/// MCP tools). A bare `*` would allow everything — never put it in a child
/// set. `None` (no filter) preserves the parent's unfiltered behavior at
/// every existing call site.
#[derive(Clone, Debug)]
pub struct ToolFilter {
    pub owner: String,
    pub allowed: BTreeSet<String>,
}

impl ToolFilter {
    // Test-only constructor; the daemon builds child filters from
    // definitions via struct literal (same precedent as
    // DaemonState::is_session_approved).
    #[cfg(test)]
    pub fn new(
        owner: impl Into<String>,
        allowed: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            owner: owner.into(),
            allowed: allowed.into_iter().map(Into::into).collect(),
        }
    }

    /// Exact name match, or a `prefix*` wildcard entry covering it.
    pub fn allows(&self, name: &str) -> bool {
        if self.allowed.contains(name) {
            return true;
        }
        self.allowed
            .iter()
            .filter_map(|entry| entry.strip_suffix('*'))
            .any(|prefix| name.starts_with(prefix))
    }
}
