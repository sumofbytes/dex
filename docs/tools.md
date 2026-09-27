# Tools

The tools the agent can call, sub-agents via `delegate`, and external tools
via MCP servers. Permissions and approval behavior are described in
[CLI flags](../README.md#command-line-flags); Lua extensions that can shadow
or add tools live in [Extensions](extensions.md).

Part of the [dex README](../README.md).

## Built-in tools

The agent can call the following tools (each maps to a function in the API
schema):

| Tool               | Purpose                                                                                                                                                                                                                                                                  |
| ------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `read`             | Read a file (`path`, `paths`, `glob`). Line-numbered, tab-expanded.                                                                                                                                                                                                      |
| `bash`             | Run a shell command via `sh -c` (`command`).                                                                                                                                                                                                                             |
| `write`            | Write/overwrite a file (`path`, `content`, optional `then_run`).                                                                                                                                                                                                         |
| `edit`             | Replace text (`path`, one `oldText`/`newText` pair or a batch of disjoint `edits[]`, optional `replaceAll`, `then_run`). Matching tolerates indentation, trailing whitespace, and quote/dash variants. |
| `grep`             | Fast frecency-ranked content search (fff engine): regex or plain text, typo-tolerant fuzzy fallback, respects `.gitignore` (`pattern`, `output_mode`, `file_offset`); truncated results end with a counted `[... more exist ...]` trailer naming the next `file_offset`. |
| `find`             | Fuzzy frecency-ranked file-path search (fff engine, typo-tolerant) (`pattern`, `limit`).                                                                                                                                                                                 |
| `ls`               | List files and directories (`path`, default `.`).                                                                                                                                                                                                                        |
| `delegate`*        | Sub-agents via `action`: `spawn` a background child (`explorer`/`reviewer`/`tester`; `resume_from` continues one, `model` overrides — else inherits; resume keeps prior pick), `wait` (bounded, ≤120 s) fetches its result, `stop` cancels it, `list` shows this session's children (live, finished, interrupted on-disk). Daemon sessions only. |

The model-facing schema registers `grep` and `find`; both are also dispatched
under their fff-engine names, `ffgrep`/`fffind` (so `dex run ffgrep …` works
too).

`*` daemon sessions only (sub-agents); the default native schema is 7 tools.
Tool results are truncated before being
sent back to the model, and a result cache (`dex-tool-cache.json`) is kept only
when `DEX_TOOL_CACHE=1`. `write`/`edit` on distinct files run in parallel; same
`path`, any `bash`, or any call carrying `then_run` (which runs a shell command)
serializes the batch.

`write`/`edit` take an optional `then_run` shell command that runs in the same
call _after_ a successful change, so a build, formatter or test result arrives
with the mutation instead of costing another round trip. It is skipped when the
change fails, and the observation reports `[then_run:succeeded]` /
`[then_run:failed (exit N)]` followed by the command's clamped output; a command
that times out, is cancelled, or fails to spawn reports plain
`[then_run:failed]` with the reason as its output (no phantom exit code).
Because it executes shell, a call carrying `then_run` clears the _shell_
permission gate (not the write gate), requires `bash` in a child agent's tool
allowlist, and is logged to the audit trail as its own `bash` record.

## Sub-agents

`delegate` hands a self-contained task to a background child (three built-ins:
`explorer`, `reviewer`, `tester`) that runs the same turn loop with its own
context, tool allowlist, and JSONL transcript
(`$XDG_DATA_HOME/dex/sessions/<slug>/agents/*.jsonl`). Completions are announced
at the next turn boundary and, while the session is idle with a client attached,
a wake turn surfaces them immediately (off with `agent_wake: false` /
`DEX_AGENT_WAKE=0`). The same `delegate` tool's `wait` action fetches a result on
demand. Children may delegate
further up to a nesting depth of 3. A recoverable ending — interrupted, timed
out, or budget-exhausted with progress on disk — is marked `resumable` in its
result: `delegate(resume_from: …)` continues that child from its transcript as
a new generation (re-entry is always manual — there is no automatic recovery,
no spawn queue, and no idle reaper; over-cap spawns reject so the model waits
for or cancels a child and retries). Under `ask-*` modes a mutating call
parks a labeled
prompt in the session's approval queue — "explorer wants to run bash: …" —
unanswered for five minutes it denies; session-level "allow" approvals apply to
children too. The `list` action shows live, finished, and interrupted children.
Set `DEX_SUBAGENTS=0` to unregister the tool.

## MCP servers

External tools via [Model Context Protocol](https://modelcontextprotocol.io)
(stdio command or HTTP/SSE URL), declared under `mcp_servers:` in `config.yaml`
(`DEX_MCP_SERVERS_JSON` wins when set — same shape as JSON):

```yaml
mcp_servers:
  github: "npx -y github-mcp-server" # shorthand: command + args
  docs:
    url: "https://docs.example.com/mcp" # HTTP/SSE server
    headers: { authorization: "Bearer ${DOCS_TOKEN}" } # $VAR expands, fail-closed
    timeout_secs: 30
    allow: ["search"] # optional tool filter (deny wins)
```

Each server's tools appear in the schema as `mcp__<server>__<tool>` (description
prefixed with `[<server>]`), plus one `mcp__<server>_read_resource` reader when
the server hosts resources. The merged schema is capped at `DEX_MCP_MAX_TOOLS`
(default 200, sorted by name, dropped count reported); servers connect in the
background at startup and a 60s liveness sweeper marks dead ones `down` before
the next turn uses them. `GET /api/mcp` shows per-server state/tool counts plus
the truncated total; `POST /api/mcp/{server}/reconnect` redials a fixed server
without restarting the daemon.

HTTP servers with OAuth (RFC9728 protected-resource + RFC8414 discovery +
RFC7591 registration + PKCE S256) log in via `dex mcp login <server>` (browser +
loopback callback), `dex mcp logout <server>`, `dex mcp status`; `/mcp` shows
the same auth lines. Tokens live in `$XDG_DATA_HOME/dex/mcp/<server>.json`
(0600, never logged), refresh once per 401 with a 60s backoff on failure
(`invalid_grant` drops the file). Discovery/token URLs must be https (loopback
http allowed); optional `oauth_client_id`/`oauth_client_secret`/`oauth_scope` in
config skip registration. When an AS rejects `resource` with `invalid_target`,
login retries once without it.
