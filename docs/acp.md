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
| `session/list` (`cwd`, `cursor`) | the daemon's sessions that hold a conversation, most recently changed first, 50 to a page with `nextCursor`; `title` is the session name if someone chose one, else the first line of the first prompt; `updatedAt` is when the session file last changed (a rename counts); `cursor` is an offset, so a session that changes between pages can shift the list |
| `session/load` | reattach and replay the journal as updates |
| `session/prompt` | one chat turn; text, resource links and embedded text are inlined |
| `session/cancel` | daemon cancel; prompt returns `stopReason: cancelled` |
| `session/set_mode` (`auto`/`manual`/`plan`) | agent mode |
| `agent_message_chunk` / `agent_thought_chunk` | assistant text / thinking |
| `tool_call` / `tool_call_update` | tool call / result |
| `plan` | plan updates |
| `session/request_permission` | tool approval (once / session / deny; anything else denies) |

Sessions start in `auto` mode (dex's default `trusted` permissions: no
approval prompts). Switch to `manual` in the editor to get
`session/request_permission` prompts; child-agent approvals are labeled
`[agent] tool: ...`. Only one prompt runs per session at a time, and a
cancelled prompt returns within a few seconds even if the daemon is slow to
stop.

Child-agent transcripts, background-task events and token usage are not
forwarded to the editor. `session/load` replays the agent's side of the
conversation; the user's own first prompts are not in the event journal, so
they are not replayed (steered and follow-up messages are).

Not supported yet: images/audio, client-provided MCP servers,
`ask_user` questions (dismissed so the turn never hangs).

## Web UI

A web UI needs no ACP: the daemon already exposes sessions, chat SSE,
approvals and cancel over HTTP (`dex-protocol` types, `dex-client` as the
reference client).
