use serde_json::{Map, Value};

use crate::runtime::cancel::CancellationSource;

use super::audit::audit;
use super::edit::{change_diff_async, tool_edit};
use super::error::ToolError;
use super::meta::{tool_ls, Policy};
use super::outcome::ToolOutcome;
use super::policy::{enforce_policy, metadata, metadata_native, PermissionRequirement, ToolFilter};
use super::read::tool_read;
use super::search::{tool_fffind, tool_ffgrep};
use super::shell::tool_bash;
use super::then_run::{append_then_run, then_run_command};
use super::write::tool_write;

/// Execute a tool using paths confined to the current workspace: the H1
/// `tool.before` seam, the gates, and the `then_run` follow-up — the full
/// pipeline every model-issued call goes through.
pub async fn execute(
    name: &str,
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
    filter: Option<&ToolFilter>,
) -> Result<String, ToolError> {
    // H1 (`tool.before`, plan §8): mutate/deny seam before every gate.
    // Delegation tools skip it — the child allowlist sees the call the
    // parent issued, unmodified by a third party.
    let owned_args: Map<String, Value>;
    let mut hooks_ran = false;
    // Zero-cost when no extension subscribes: the args pass through uncloned.
    let args = if crate::agent::delegate::is_delegation(name)
        || !crate::extensions::has_event_handlers("tool.before")
    {
        args
    } else {
        hooks_ran = true;
        match crate::extensions::apply_before_hooks(name, args, cancel, policy, filter).await {
            crate::extensions::BeforeOutcome::Proceed { args, mutated_by } => {
                for ext in &mutated_by {
                    audit(&format!("{name}[tool.before:{ext}]"), &args, "mutated");
                }
                owned_args = args;
                &owned_args
            }
            crate::extensions::BeforeOutcome::Denied { by, reason } => {
                return Err(ToolError::Denied(format!(
                    "tool.before hook from extension '{by}' denied the call: {reason}"
                )));
            }
        }
    };
    // Resolve `then_run` once — it decides both the permission requirement
    // inside `dispatch_tool` and the follow-up run below, and a malformed
    // value must fail before anything else happens. Only `write`/`edit`
    // accept the field, so for every other tool — delegation and MCP ones
    // included — this is a side-effect-free `Ok(None)`.
    let then_run = match then_run_command(name, args) {
        Ok(command) => command,
        Err(error) => return Err(error),
    };
    let result = dispatch_tool(name, args, then_run, cancel, policy, filter, true).await;
    // A tool.before hook already saw the args even when a later gate denies
    // the call: record that observation, so the audit trail shows a third
    // party witnessed a call it never got to influence.
    if hooks_ran {
        if let Err(ToolError::Denied(reason)) = &result {
            audit(
                &format!("{name}[tool.before]"),
                args,
                &format!("observed; call denied: {reason}"),
            );
        }
    }
    // Run `then_run` in this same call so the mutation and its
    // verification arrive as one observation, instead of costing a second
    // provider round-trip that re-sends the whole prefix just to learn whether
    // the build passed. A failed mutation skips the command: the edit error is
    // the observation, and a command output must never be handed back as if it
    // had run against the new content. (`then_run` is only ever `Some` for
    // `write`/`edit`, which always take the workspace path above.)
    let result = match (result, then_run) {
        (Ok(text), Some(command)) => {
            let (text, code) = append_then_run(text, command, cancel).await;
            // Audit the shell run separately from the mutation: `DEX_AUDIT=1`
            // must show that a command ran and how it exited, not just a
            // successful `write`/`edit`.
            let mut shell_args = Map::new();
            shell_args.insert("command".into(), Value::String(command.to_string()));
            shell_args.insert("then_run_of".into(), Value::String(name.to_string()));
            let shell_outcome = match code {
                Some(0) => "ok".to_string(),
                Some(code) => format!("exit {code}"),
                None => "no exit status (timeout, cancel, or spawn failure)".to_string(),
            };
            audit("bash", &shell_args, &shell_outcome);
            Ok(text)
        }
        (result, _) => result,
    };
    // One audit row per tool call: every exit path — the delegation denial, a
    // malformed `then_run`, an unknown tool, the allowlist rejections, the
    // policy denial, the provider/tool failure — lands here with exactly the
    // outcome string the per-exit audits used to record.
    let outcome = match &result {
        Ok(_) => "ok".to_string(),
        Err(error) => error.to_string(),
    };
    audit(name, args, &outcome);
    result
}

/// Re-dispatch the shadowed built-in for `dex.tools.call_original`: the
/// same gates as any call (approval included), but never the shadow itself.
/// `then_run` stays with the outer shadow call, which resolves and audits it.
pub async fn dispatch_original(
    name: &str,
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
    filter: Option<&ToolFilter>,
) -> Result<String, ToolError> {
    dispatch_tool(name, args, None, cancel, policy, filter, false).await
}

/// The tool-selection core behind `execute`: routes and gates a
/// call, then runs it — with no `audit` calls (the single audit row lives in
/// the caller). The gate order is load-bearing (§11): delegation route first,
/// then the permission requirement, then the child allowlist (a sharper,
/// cheaper rejection than a prompt), then `enforce_policy` — so a denial
/// never triggers a prompt — and only then dispatch.
#[allow(clippy::too_many_arguments)]
async fn dispatch_tool(
    name: &str,
    args: &Map<String, Value>,
    then_run: Option<&str>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
    filter: Option<&ToolFilter>,
    resolve_shadow: bool,
) -> Result<String, ToolError> {
    // Delegation tools route first (Phase 5): they are not workspace tools
    // and have no static registry entry. The child allowlist never contains
    // one, so the same availability gate rejects a child's call before any
    // delegation logic runs (§11 — at-cap children carry no delegation tools).
    if crate::agent::delegate::is_delegation(name) {
        if let Some(filter) = filter {
            if !filter.allows(name) {
                return Err(ToolError::Denied(format!(
                    "tool '{name}' is not in {}'s tool allowlist",
                    filter.owner
                )));
            }
        }
        return crate::agent::delegate::execute_delegation(name, args, cancel, policy).await;
    }
    // Inside a shadow re-dispatch (`resolve_shadow == false`) the shadow's
    // Shell row must not raise the gate again — use the native requirement.
    let requirement = match if resolve_shadow {
        metadata(name)
    } else {
        metadata_native(name)
    } {
        // MCP tools are external processes: their `metadata()` row already
        // carries the most restrictive gate (same as shell), so the separate
        // `mcp__` requirement check is gone from here.
        Some(meta) => {
            // A `write`/`edit` carrying `then_run` also runs a shell
            // command, so it must clear the *shell* gate rather than the weaker
            // write gate — `bash` is not the write gate, and an `edit` with
            // `then_run` must not become a way to run a command unapproved.
            if then_run.is_some() {
                PermissionRequirement::Shell
            } else {
                meta.permission
            }
        }
        None => return Err(ToolError::Unknown(name.to_string())),
    };
    // Availability before approval: a sharper, cheaper rejection naming the
    // agent's allowlist (plan §11 — the model self-corrects, never executes).
    // Checked before `enforce_policy` so a denied tool never prompts.
    if let Some(filter) = filter {
        let error = if !filter.allows(name) {
            Some(ToolError::Denied(format!(
                "tool '{name}' is not in {}'s tool allowlist",
                filter.owner
            )))
        } else if then_run.is_some() && !filter.allows("bash") {
            // `then_run` runs a shell command, so an allowlist that permits
            // `edit` but not `bash` must not become a shell escape hatch.
            Some(ToolError::Denied(format!(
                "'{name}' carries then_run, which needs 'bash' in {}'s tool allowlist",
                filter.owner
            )))
        } else {
            None
        };
        if let Some(error) = error {
            return Err(error);
        }
    }
    if resolve_shadow && crate::extensions::is_shadowed(name) {
        // Same gates as the ext__ row in metadata(): a shadow intercepts a
        // built-in, so it can lie about what the built-in does.
        enforce_policy(name, args, PermissionRequirement::Shell, cancel, policy).await?;
        return crate::extensions::call_shadow_global(name, args, cancel, policy, filter)
            .await
            .map_err(ToolError::Internal);
    }
    enforce_policy(name, args, requirement, cancel, policy).await?;
    if name.starts_with("mcp__") {
        // `ToolError::Internal` displays as the raw message, so the caller's
        // single audit row records exactly the string audited here before.
        return crate::mcp::call_global(name, args, cancel)
            .await
            .map_err(ToolError::Internal);
    }
    if crate::extensions::is_extension_tool(name) {
        // Same audit contract as MCP: the raw string lands in the one row.
        // (`call_global` normalizes the deprecated `lua__` alias.)
        return crate::extensions::call_global(name, args, cancel, policy, filter)
            .await
            .map_err(ToolError::Internal);
    }
    // fff owns its threads + lock; run inside spawn_blocking (10s grep budget stays).
    if matches!(name, "grep" | "ffgrep" | "find" | "fffind") {
        let name_owned = name.to_string();
        let args_owned = args.clone();
        return tokio::task::spawn_blocking(move || match name_owned.as_str() {
            "grep" | "ffgrep" => tool_ffgrep(&args_owned),
            _ => tool_fffind(&args_owned),
        })
        .await
        .unwrap_or(Err(ToolError::Internal("fff worker panicked".into())));
    }
    match name {
        "read" => tool_read(args).await,
        "bash" => tool_bash(args, cancel).await,
        "write" => tool_write(args).await,
        "edit" => tool_edit(args).await,
        "grep" | "ffgrep" | "find" | "fffind" => unreachable!("handled above"),
        "ls" => tool_ls(args).await,
        _ => unreachable!("metadata and dispatch must stay in sync"),
    }
}

/// Execute a tool, reporting success explicitly. Callers must not re-derive
/// success from the output text: tool output can legitimately contain
/// strings like `[exit 1]` (shell markers appear in source files and logs).
pub async fn execute_outcome(
    name: &str,
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
    policy: &Policy,
    filter: Option<&ToolFilter>,
) -> ToolOutcome {
    // Capture the unified diff BEFORE `execute` mutates the file; the
    // before-image is gone afterwards. Display-only: it never reaches the
    // model, only the transcript preview via `ToolOutcome::diff`. A `then_run`
    // (e.g. a formatter) can touch the file again afterwards; the
    // preview stays the mutation's diff, which is the change the model asked
    // for and asked to see.
    let pending_diff = if matches!(name, "write" | "edit") {
        change_diff_async(name, args).await
    } else {
        None
    };
    let (mut text, mut ok) = match execute(name, args, cancel, policy, filter).await {
        Ok(out) => (out, true),
        Err(e) => (format!("Error: {}", e), false),
    };
    // H2 middleware: `tool.after` may rewrite the result. Fail-open — a hook
    // error keeps the host result (the manager already logs it).
    let after =
        crate::extensions::apply_after_hooks(name, args, &text, ok, cancel, policy, filter).await;
    text = after.text;
    ok = after.ok;
    ToolOutcome {
        text,
        ok,
        diff: pending_diff,
    }
}

/// Sync wrappers for `dex run <tool>` / `dex --tool` raw paths (no async CLI
/// plumbing needed per plan §5). Explicit user invocations run trusted —
/// the command itself is the approval — so no policy parameter; unfiltered
/// too (explicit invocations run the full toolset, never a child allowlist).
pub fn execute_sync(
    name: &str,
    args: &Map<String, Value>,
    cancel: &(dyn CancellationSource + Send + Sync),
) -> Result<String, ToolError> {
    crate::runtime::http::block_on(execute(name, args, cancel, &Policy::trusted(), None))
}
