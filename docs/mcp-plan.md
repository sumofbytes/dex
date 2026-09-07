# MCP for `dex`: analysis + implementation plan

> Revalidated 2026-09-06 against the async runtime (Phases 1–6 done:
> async provider I/O, async agent loop, async tools, async daemon bridges,
> shared `reqwest::Client`, `tokio` `fs/process/time/io-util`).
> The P0 scope is unchanged (stdio + Streamable HTTP, `tools/list` +
> `tools/call` only, `mcp__<server>__<tool>` namespacing, default-ask).
> What changed is *how* to build it: every stale "sync/blocking" assumption
> below is updated to the async shape that now exists. The hand-rolled-client
> decision still holds — but for a different reason (see §5.3).

## 1. Goal

Add **MCP Tools as first-class `dex` tools** behind a daemon-owned connection manager.

- MVP: `stdio` + `Streamable HTTP` transports, `tools/list` + `tools/call` only.
- `mcpServers`-compatible config, `mcp__<server>__<tool>` namespacing.
- Reuse existing `PermissionMode` / approval / session / SSE paths.
- Defer `resources` / `prompts` / `sampling` / `elicitation` / OAuth-dance to phases 2-3.

## 2. Where `dex` stands today (post-async)

| Concern | Current `dex` shape | File |
|---|---|---|
| Tool schema sent to LLM | Static `tools_schema()` in Chat + `responses_tools()` in Responses | `src/llm/protocol.rs:32`, `src/llm/client.rs:188,230` |
| Tool dispatch | `async execute()` / `async execute_outcome()` match on `read/bash/write/edit/grep/find/ls(/git/chain)`; `grep/find` via `spawn_blocking`, rest native async | `src/tools/mod.rs:1152,1201` |
| Tool metadata/permissions | `metadata(name) -> ToolMetadata{read_only,mutating,requires_shell,permission}` | `src/tools/mod.rs:143` |
| Turn loop | `async process_turn()` → `client.complete().await` under `select!(wait_cancelled, complete)` (~10ms cancel) → `JoinSet` tool fan-out (conflict path serializes under async `TOOL_MUTATION_LOCK`) → append `Role::Tool` | `src/agent/loop.rs:131,205,273` |
| Daemon ownership | `DaemonState{sessions,pending_approvals,active_turns,cancel_tokens,...}` — plain `std::Mutex`, short sections never across `.await` + `tokio::mpsc` bridges + `send().await` + `tokio::spawn(run_agent_turn)` | `src/daemon/mod.rs:38`, `src/daemon/server.rs:539` |
| Config layering | `$DEX_CONFIG` / `~/.config/dex/config.yaml` + `DEX_*` / `OPENAI_*` + CLI; cached YAML `Value`; sync `from_env` + `from_env_async` (`spawn_blocking` on miss) for the daemon turn | `src/llm/config.rs:12,36,1418` |
| HTTP/cache runtime | One shared async `reqwest::Client` (`shared_async_client`), one shared 2-worker runtime for sync bridges (`block_on` for `dex run` / one-shot) | `src/client/http.rs:25,75` |
| Client UX | TUI slash (`/model,/provider,/permissions...`), `ChatRequest{prompt,skill_dirs,base_url,model,permission,headers,plan}` | `src/ui/slash.rs:13`, `src/protocol/mod.rs:46` |
| Constraints | Async agent loop + `JoinSet`; `tokio::fs/process`, `reqwest` async-only (no `blocking` feature); workspace-confined tools; minimal prompt; `TOOL_SCHEMA_TOKENS=3200`; no new deps without need | `src/agent/tokens.rs:6`, `Cargo.toml`, `AGENTS.md` |

Integration seams are small: one schema function + one async dispatch function.
MCP slots into both without new threads, channels, or runtimes.

## 3. Industry standard: MCP in 30s

Spec: Anthropic's **Model Context Protocol** (open, `modelcontextprotocol/spec`,
versions `2024-11-05` → `2025-03-26` → `2025-06-18` → `2025-11-25`).
`JSON-RPC 2.0` with roles:

`Host (dex daemon + TUI)` → `Client (per-server connection)` → `Server (npx/uvx/binary/remote URL)`.

| Area | Standard |
|---|---|
| Transports | `stdio` (child `stdin/stdout` NDJSON, `stderr` = logs) covers ~90% of local servers; `Streamable HTTP` (POST `/mcp`, `Accept: application/json,text/event-stream`, `Mcp-Session-Id` header, optional SSE stream, `Last-Event-ID` resume) is current. Old `SSE GET+POST` is deprecated but still seen. |
| Lifecycle | `initialize{protocolVersion,clientInfo,capabilities}` → `initialized` notification → `tools/list`, `tools/call`, `ping`. Version negotiation: client offers newest, falls back to server's. |
| Primitives | **Tools** (model-controlled, `name/description/inputSchema/annotations`), **Resources** (`uri/read`, app-controlled), **Prompts** (`get`, user-controlled). Aux: `roots/list`, `sampling/*` (server asks model), `elicitation/*` (server asks user), `logging/setLevel`, `progress` notifications, `notifications/cancelled`, `completion/complete`. |
| Auth | `stdio`: env passthrough. `HTTP`: custom headers today, `OAuth 2.1` + `WWW-Authenticate` challenge tomorrow. |
| Security model | Servers are **untrusted third-party code**: tool descriptions can poison prompts, `inputSchema` can exfiltrate, servers can rug-pull on reconnect. |

## 4. What leaders do (copy this)

| Agent | Config | Transports | Naming | Approval | Lesson for `dex` |
|---|---|---|---|---|---|
| Claude Code | `mcpServers:{name:{command,args,env,url,headers}}` in `.claude.json` / `settings.json` | stdio+HTTP+SSE | `mcp__<server>__<tool>` | Per-tool allowlist, server origin shown | Copy config key + prefix verbatim for muscle memory. |
| VS Code Copilot | `.vscode/mcp.json`, same shape | stdio+HTTP | `server_tool` + picker | Workspace trust + approval | Same shape = zero-doc migration. |
| Cursor / Zed | `mcp.json` + UI toggle per server/tool | stdio+HTTP | namespaced | `ask` by default for MCP | MCP tools default-deny is norm. |
| Goose (Rust) | `goose-mcp` + `rmcp` crate | stdio+HTTP | namespaced | Extensions permission | Proves `rmcp` works in Rust/Tokio — the P2 fallback if spec churn hurts. |
| OpenAI Agents SDK / LangChain | `MCPServerStdio/Http` adapter → flattened to function tools | both | `server.tool` | Caller decides | Validates `tools/list` → OpenAI functions mapping. |
| `pi` (closest to `dex`) | Minimal tools, parallel calls | n/a | flat | `trusted` default | Keep `dex` prompt minimal; put MCP detail in tool descriptions, not `src/llm/prompt.rs`. |

Takeaway: **config-compat + namespacing + default-ask + lazy connect + `tools`-only MVP**
is the industry consensus.

## 5. Design for `dex`

### 5.1 Scope

- **P0 MVP (this plan):** `stdio` + `Streamable HTTP` client, `initialize` + `tools/list` + `tools/call` + `ping` + `notifications/tools-list-changed`, tool caching, timeout + cancel, approval integration.
- **P1:** `resources/list|read`, `prompts/list|get` (expose as `mcp__<server>_read_resource` synthetic tool or `/mcp:resource` slash, not auto-flattened).
- **P2:** `sampling` (route back into current `LlmConfig` model), `elicitation` (route into `ApprovalRequired` SSE), `roots` (send workspace root), OAuth dance, `/mcp` OAuth login.
- Explicit non-goals P0: MCP server mode, SSE-server compat shim beyond best-effort fallback, per-session servers.

### 5.2 Config (Claude-compatible)

Extend `config.yaml` (untyped `Value` survives write-back, see `src/llm/config.rs:22`),
plus `DEX_MCP_*` env escape hatches:

```yaml
mcp_servers:
  github:
    command: npx
    args: ["-y", "@modelcontextprotocol/server-github"]
    env: { GITHUB_TOKEN: "${GITHUB_TOKEN}" } # ${VAR} expanded, never logged
    cwd: "."
    disabled: false
    timeout_ms: 30000
    allow: ["*"]        # per-tool allowlist; deny wins
    deny: ["delete_*"]
  postgres:
    type: http          # stdio is default when `command` present
    url: https://mcp.internal/mcp
    headers: { Authorization: "Bearer ${MCP_TOKEN}" }
```

Rules: keys lowercased like `providers:`; unknown keys ignored; `disabled` skips spawn;
file cached via existing `CONFIG_CACHE` + mtime check. Add `mcp: { enabled: bool,
max_tools: 64, timeout_ms }` globals with `DEX_MCP_*` overrides. Document in `README.md`
config table.

Async note: parse stays sync (small YAML, same `load_config_file` pattern).
If the daemon turn needs it, expose `load_mcp_config_async()` mirroring
`LlmConfig::from_env_async` (`spawn_blocking` on cache miss only); cache hits
are a mutex bump and stay inline. Never hold the config lock across `.await`.

### 5.3 New module: `src/mcp/`

```text
src/mcp/mod.rs       # McpManager, namespacing, ToolDefinition conversion
src/mcp/config.rs    # McpServerConfig parse from YAML Value + env expansion
src/mcp/transport.rs # StdioTransport (tokio::process) + HttpTransport (shared reqwest::Client)
src/mcp/protocol.rs  # JSON-RPC framing, initialize/list/call/ping types
```

All four files are async from birth (`async fn`, `tokio::sync::RwLock` /
per-server `Mutex`, `JoinSet` fan-out, `tokio::time::timeout`). No `blocking`
reqwest, no `std::process`, no new threads. `dex run <tool>` / `dex --tool`
bridge via the existing `crate::client::http::block_on`, same as
`execute_sync` (`src/tools/mod.rs:1230`).

Why still hand-rolled P0 (revalidated): the old reason (sync loop + blocking
reqwest made `rmcp` an async bridge in the hot path) is gone — the loop, tools,
daemon, and HTTP client are all async now, so `rmcp` *would* fit. The remaining
reason is `AGENTS.md` minimalism: `initialize/list/call/ping` over the
`reqwest` client and `tokio::process` we already own is ~300–400 lines with
`serde_json` only, zero new deps, and full control over timeout / cancel /
clamping / redaction. Adopt `rmcp` at P2 only when one of these is true:
sampling / elicitation / OAuth-resume is needed, or Streamable-HTTP spec churn
(notifications, resume, version negotiation) costs more than the dep. Keep a
trait boundary (`McpBackend`: `list_tools` / `call_tool` / `ping`) so P2 can
swap transports without touching `execute_outcome` or the turn loop.

### 5.4 Lifecycle / ownership

- `DaemonState` gains `mcp: Arc<McpManager>` (`src/daemon/mod.rs:38`).
  `McpManager{configs, clients: RwLock<HashMap<server, Arc<McpClient>>>}`.
  `std::Mutex` sections stay short and never cross `.await` (daemon rule);
  per-connection state lives behind `tokio::sync::Mutex` / `RwLock`.
- Lazy + resilient: spawn on first `tools_schema_with_mcp()` call or background after
  `rebuild_async()`; failure = server marked `down+error`, turn proceeds with native tools
  (never fail-closed P0).
- `stdio`: `tokio::process::Command::new(cmd).args().stdin(piped).stdout(piped).stderr(piped).current_dir(cwd).envs()`;
  `stderr` drained by an async task to the trace log; child killed on `disabled` /
  `remove` / daemon-drop; keep process-group kill (`setsid`/`killpg` on unix,
  same helpers as `run_bash_with_limits` in `src/tools/mod.rs`) so Ctrl-C kills the group.
- `http`: shared async client (`shared_async_client()`, `src/client/http.rs:75`) —
  no per-turn `Client::builder()`. POST with `Mcp-Session-Id` persistence; on `404/410`
  re-`initialize`; on `426` downgrade SSE → streamable. Timeouts via
  `tokio::time::timeout`, cancel via `select!(cancelled, request)`.
- `tools/list` cached per server; invalidated on `notifications/tools-list-changed`
  (stdio line / HTTP SSE event); `ping` every 30s via `tokio::time::interval`;
  reconnect with backoff (250ms → 5s, 3 tries then `down`). Initial fan-out
  across servers uses a `JoinSet` (same shape as `fanout_read` / `rebuild_async`),
  results joined + sorted so `tools_schema` order is deterministic.

### 5.5 Tool mapping (the critical detail)

OpenAI function names allow `^[a-zA-Z0-9_-]{1,64}`:

```rust
fn mcp_tool_name(server: &str, tool: &str) -> String {
    let raw = format!("mcp__{server}__{tool}");
    let s: String = raw.chars().map(|c| {
        if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' }
    }).collect();
    s.chars().take(64).collect() // collisions get __2 suffix + warn
}
```

- `description`: `"[mcp <server>] <upstream description>"` truncated to ~500 chars —
  keeps `src/llm/prompt.rs` minimal per convention, detail lives in `src/llm/protocol.rs`.
- `parameters`: passthrough upstream `inputSchema` (already JSON Schema); fill `{}` when
  missing; validate is object.
- `annotations -> {readOnlyHint, destructiveHint, idempotentHint, openWorldHint}` →
  `ToolMetadata`: default `PermissionRequirement::Shell` (most restrictive = `ask`);
  downgrade to `Read` only when `readOnlyHint == true && !destructive && !openWorld`.
  This mirrors Cursor/Zed default-ask and reuses `Console::approval_key` +
  `session_approvals` without new UI.
- Budget: `tools_schema()` → `tools_schema_with_mcp(&mcp_tools)`; cap `max_tools`
  (default 64), sort native first, then `server.alpha, tool.alpha`; when over cap, drop
  lowest-priority servers + emit `System` SSE event (via `send().await`, never
  `blocking_send`). Schema fetch is async with a sync cached read on the hot path:
  the turn loop awaits listing only on cache miss / invalidation, otherwise it clones
  the cached `Vec<ToolDefinition>`. Bump `TOOL_SCHEMA_TOKENS`
  accounting in `src/agent/tokens.rs:6` by `~80 + schema_len/4` per MCP tool.

### 5.6 Execution

`execute_outcome()` is already `async` (`src/tools/mod.rs:1201`), so MCP is one
branch at the top — no bridge, no thread:

```rust
pub(crate) async fn execute_outcome(
    name: &str, args: &Map<String, Value>, cancel: &(dyn CancellationSource + Send + Sync),
) -> ToolOutcome {
    if let Some((server, tool)) = parse_mcp_name(name) {
        return mcp_manager.call(server, tool, args, cancel).await; // timeout, map content blocks
    }
    // else existing match (read/bash/write/edit/… unchanged)
}
```

- `tools/call{ name, arguments }` with `timeout_ms` (default 30s) via
  `tokio::time::timeout`; cancel via the existing `wait_cancelled` 10ms poll
  pattern (`src/agent/loop.rs:85`) inside `select!` — on cancel send
  `notifications/cancelled{requestId}` (stdio) or drop + reconnect (HTTP),
  then return `Error: cancelled`. `CancellationSource` stays a sync trait;
  async tools check it before/after awaits, never `async fn` in the trait.
- Content mapping: `content: [{type:text,text}, {type:image,...}, {type:resource,...}]`
  → joined markdown string (`text` verbatim, `image` → `[image <mime>]`, `resource` →
  fenced block with `uri`); `isError == true` → `ToolOutcome{ok:false}`. Apply same
  `BASH_CLAMP_LINES` / `BASH_CLAMP_BYTES` clamping (`src/tools/mod.rs:32`) so MCP can't flood context.
- `metadata(name)` returns dynamic entry for `mcp__*` so approval, `tool_preview`,
  `DEX_BIN run mcp__...`, and `dex --tool` JSON mode all work unchanged
  (`execute_sync` → `block_on` covers the raw paths).
- MCP calls join the existing turn fan-out: parallel calls to different
  `mcp__*` tools run concurrently in the loop's `JoinSet`; same-path conflict
  serialization (`tool_calls_conflict` + `TOOL_MUTATION_LOCK`) applies unchanged.
  Panics propagate as tool errors like any other tool.
- Session journal: result stored as normal `Role::Tool` message; no new `Role`;
  survives replay / reattach.

### 5.7 Daemon → client UX

- No `protocol/mod.rs` breaking change P0: MCP surfaces as normal `ToolCall` /
  `ToolResult` SSE with `name=mcp__...`. Approval dialog already shows `name+input`;
  prefix approval title with server (`approval_title` in `src/core/format.rs`).
- Add `GET /api/mcp` (`[{server,status,tools,error}]`) + `POST /api/mcp/{server}/reconnect`;
  TUI `/mcp` lists, `/mcp:reconnect <server>`, `/mcp:logs`. Footer shows `mcp:2/3` count
  via `DaemonInfo` extension (additive field only). Emits use `send().await` on the
  existing tokio bridges (`run_agent_turn`, `src/daemon/server.rs:539`).
- One-shot (`dex "prompt"`) and `Default` (co-located daemon) construct an ephemeral
  `McpManager` from same config so behavior matches `serve` / `connect`. Sync entry
  points use `block_on` on the shared runtime; no second runtime.

### 5.8 Security (must-have, MCP is hostile)

- Default-deny: MCP tools require approval unless `DEX_PERMISSION=trusted` or explicit
  `allow` + prior `AllowSession`. Never auto-allow on `read-only`.
- Show origin: every transcript / approval line carries `[mcp <server>]`; `audit()` logs
  `server/tool + args-hash` (redact `token/key/secret/auth` keys).
- Env expansion only from process env, never from model output; upstream `env` in config
  can't reference another server's secrets.
- Schema size caps (64KB), description caps, output clamps, timeout kills, no shell
  interpolation (args passed as JSON, never `sh -c`).
- Document rug-pull + tool-poisoning risks in `README.md` + `SECURITY.md`; recommend
  pinning `args` versions (`-y package@x.y.z`) and per-tool `deny`.

## 6. Build order (4 PRs, each green)

1. **Config + types:** `src/mcp/config.rs`, `McpServerConfig::from_yaml`, env `${VAR}`
   expansion, tests. Docs skeleton.
2. **Transport + protocol:** async stdio (`tokio::process`) + JSON-RPC
   `initialize/list/call/ping`, async Streamable-HTTP client on the shared
   `reqwest::Client`, `McpManager` with cache / reconnect. `#[tokio::test]`
   with a fake `node -e` echo server + `axum` mock; cancel/timeout tests use
   `select!` + `timeout`, never `sleep`-poll asserts.
3. **Tool wiring:** `tools_schema_with_mcp`, `metadata` + `execute_outcome` dispatch,
   annotation → permission map, clamping, `DEX_BIN run` compat via `block_on`. Update
   `TOOL_SCHEMA_TOKENS`, `compaction` unaffected.
4. **Daemon / UX:** `DaemonState.mcp`, `/api/mcp` routes, `/mcp` slash, approval titles,
   `README.md` + example `mcp_servers`.

Each PR runs: `cargo fmt -- --check`, `cargo test --all-targets`,
`cargo clippy --all-targets -- -D warnings`.

## 7. Tests

- Config: missing file → empty; `${MISSING}` → error, not empty-string leak;
  `disabled` skipped.
- Naming: sanitization, 64-char cut, collision suffix, round-trip `parse_mcp_name`.
- Protocol (`#[tokio::test]`): fake stdio server (initialize version fallback, list,
  call text + image + isError, `tools-list-changed` invalidation, timeout →
  cancel notification); fake HTTP server (`Mcp-Session-Id` persist, 404 → re-init).
- Integration: `execute_outcome("mcp__gh__search",…)` via stub `McpBackend`
  (`.await`, no runtime nesting); approval key includes server; output clamped
  to 32KiB / 400 lines.
- Daemon: server down → turn still completes with native tools; reconnect flips
  `down → up`; concurrent-turn `409` semantics unchanged.

## 8. Effort / risks

~1 week for P0 (config 0.5d, transport 2d, wiring 1.5d, daemon/UX + docs 1d).
Top risks: token bloat from large MCP schemas (mitigate: `max_tools` + allowlists),
flaky `npx` cold start (mitigate: lazy + 30s `timeout` + cached list + `JoinSet`
fan-out so one slow server never blocks the turn), name collisions
(mitigate: suffix + warn), OAuth servers failing P0 (mitigate: headers passthrough,
clear error telling user P2 covers OAuth).

Open decisions for owner: (a) config lives in `config.yaml` vs separate `mcp.json`
(recommend `config.yaml:mcp_servers` + `$DEX_MCP_FILE` override for Claude-drop-in),
(b) confirm hand-rolled async client vs `rmcp` (recommend hand-rolled P0, revisit P2
per the trigger criteria in §5.3).

## 9. Independent review (red-team)

- **Biggest risk is not transport, it is context + trust.** An MCP server with 20 tools
  × 2KB schemas can crowd out native tools and push compaction earlier. `max_tools`
  + `allow/deny` + description truncation are load-bearing, not nice-to-haves.
- **Do not add MCP-specific approval UI P0.** Reuse `ApprovalRequired` + origin prefix;
  a second approval path will drift and break `serve` / `connect` parity.
- **Do not persist MCP credentials or tokens in sessions.** Env expansion at spawn time
  only; journal holds tool args/results, never `env` / `headers`.
- **Do not auto-enable remote HTTP servers.** Local `stdio` first; remote URLs require
  explicit user config (phishing + exfiltration vector).
- **Failure isolation:** one poisoned / slow server must not fail the turn. Per-server
  timeouts, per-server `down` state, and proceed-with-natives are correct.
- **Missing piece to add in P1:** `tool_preview` redaction for MCP args (tokens in args
  are common with atlassian/github servers) and an `mcp__*` audit filter.

## 10. What the async migration changed (why this revalidation was needed)

| Old assumption (pre-async doc) | Reality now | Effect on MCP |
|---|---|---|
| Agent loop sync + `thread::spawn` per tool; `tokio` only in daemon | `process_turn` async, `JoinSet` fan-out, `select!` cancel (~10ms) | MCP `call` is a plain `.await` in the fan-out; no thread/bridge work |
| `reqwest::blocking` + `std::process` transports | `reqwest` async-only, `tokio::{fs,process,time,io-util}` in `Cargo.toml`, shared `Client` + shared runtime | Transports use `shared_async_client()` + `tokio::process`; no `Client::builder()` per turn, no new runtime |
| `execute_outcome` sync; MCP needs an async bridge | `execute`/`execute_outcome` async; `block_on` only at `dex run` edge | Dispatch is one `if mcp__` branch; raw CLI paths reuse `execute_sync` |
| `from_env` sync per turn; config cache described as future | `from_env_async` exists (`spawn_blocking` on miss) | MCP config follows the same sync-parse + async-accessor pattern |
| `spawn_blocking(run_agent_turn)` + `blocking_send` bridges | `tokio::spawn(run_agent_turn)` + `send().await` | MCP status/approval events use the same `send().await` bridges |
| `TOOL_MUTATION_LOCK` std mutex held around join | `tokio::sync::Mutex`, `.lock().await` scoped around dispatch | MCP inherits conflict-serialize behavior for free |
| `rmcp` rejected (async bridge cost) | `rmcp` would fit technically | Still deferred P0 — now justified by dep minimalism, with P2 triggers in §5.3 |

`Cargo.toml` needs **no changes** for P0: `tokio` already has
`rt-multi-thread/macros/net/sync/fs/process/time/io-util`, `reqwest` already has
`json/stream/rustls-tls-native-roots`, `serde_json` covers framing. A P2 `rmcp`
adoption is the only future dep on the table.
