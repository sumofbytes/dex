# Agent Client Protocol (`dex acp`)

`dex acp [url]` speaks [ACP](https://agentclientprotocol.com) (JSON-RPC 2.0,
newline-delimited, stdio) so editors such as Zed can drive dex. It is a thin
adapter (`crates/dex-acp`) over the daemon's HTTP+SSE API: without `url` it
hosts an in-process daemon; with one it attaches to a running `dex serve`.
Stdout carries protocol frames only; logs go to stderr. Model/header flags
(`--model`, `-H`, ...) apply to every turn.

Zed (`settings.json`):

```json
{ "agent_servers": { "dex": { "command": "dex", "args": ["acp"] } } }
```

## Mapping

| ACP | dex |
| --- | --- |
| `session/new` (`cwd`) | daemon session |
| `session/prompt` | one chat turn; text, resource links and embedded text are inlined |
| `session/cancel` | daemon cancel; prompt returns `stopReason: cancelled` |
| `session/set_mode` (`auto`/`manual`/`plan`) | agent mode |
| `agent_message_chunk` / `agent_thought_chunk` | assistant text / thinking |
| `tool_call` / `tool_call_update` | tool call / result |
| `plan` | plan updates |
| `session/request_permission` | tool approval (once / session / deny; anything else denies) |

Not supported yet: `session/load`, images/audio, client-provided MCP servers,
`ask_user` questions (dismissed so the turn never hangs).

## Web UI

A web UI needs no ACP: the daemon already exposes sessions, chat SSE,
approvals and cancel over HTTP (`dex-protocol` types, `dex-client` as the
reference client).
