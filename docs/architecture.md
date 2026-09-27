# Architecture

How `dex` is organized: the reusable workspace crates, the source tree, and
how a turn flows through the system. Part of the
[dex README](../README.md).

## Workspace crates

The Cargo workspace contains the `dex` application and five reusable crates.
They have no dependency on dex's TUI, daemon implementation, session store, or
workspace tool implementations. The app continues to re-export existing types
from `dex::protocol` and `dex::client` for source compatibility.

- `dex-protocol` owns serializable HTTP/SSE wire types shared by clients and
  daemons.
- `dex-ai` owns provider-neutral message and tool types, provider wire mapping,
  SSE parsers, HTTP auth/retry policy, and the model-client API. Dex still owns
  model discovery, configuration resolution, credential refresh, UI streaming,
  and CLI behavior.
- `dex-agent-core` owns permission/agent modes, plans, token accounting,
  deterministic compaction, text limits, context compaction and tool-round
  budget policies, the model-response-to-history transition, and the generic
  model/tool turn engine. Dex implements the engine's host interface for its
  compaction backends, tools, sessions, steering, extensions, and UI.
- `dex-coding-agent` owns the built-in coding tool catalog, stable composition
  with host-provided tool schemas, built-in permission metadata, and
  host-independent cache/repeated-call result policy. Dex supplies delegation
  availability, MCP/extension tools, tool execution, output rendering, and
  cache persistence.
- `dex-client` is a standalone HTTP/SSE daemon client over `dex-protocol`.
  It can be embedded without pulling in the daemon, TUI, session store, or
  workspace tools; callers can provide credentials with `with_token`.

```toml
[dependencies]
dex-protocol = { git = "https://github.com/sumofbytes/dex" }
dex-ai = { git = "https://github.com/sumofbytes/dex" }
dex-agent-core = { git = "https://github.com/sumofbytes/dex" }
dex-coding-agent = { git = "https://github.com/sumofbytes/dex" }
dex-client = { git = "https://github.com/sumofbytes/dex" }
```

## Project structure

```text
dex/
├── crates/
│   ├── dex-protocol/     # reusable HTTP + SSE wire types
│   ├── dex-ai/           # model API, provider wiring, shared AI types
│   ├── dex-agent-core/   # reusable agent engine and policies
│   ├── dex-coding-agent/ # tool catalog, tool policies, schema composition
│   └── dex-client/       # standalone HTTP/SSE daemon client
├── .dex/
│   └── skills/           # (optional) project-level agent skills
├── target/
├── Cargo.lock
├── Cargo.toml
├── README.md
└── src/
    ├── main.rs           # entry point: mode resolution, daemon bootstrap
    ├── cli.rs            # argument parsing / invocation mode
    ├── app/              # client/oneshot entry orchestration
    ├── protocol/         # compatibility exports + app-specific protocol glue
    ├── client/           # HTTP client: SSE turn streaming, approvals, REPL
    ├── daemon/           # axum daemon: sessions, chat SSE, approve, cancel
    ├── agent/            # lifecycle wrapper, AgentHost adapter, app integrations
    ├── runtime/          # console sinks, cancellation, logging
    ├── llm/              # model config, provider resolution, app transport glue
    ├── session/          # JSONL session persistence and journal
    ├── tools/            # workspace tools: read, bash, write, edit, grep, find, ls
    ├── mcp/              # MCP client: stdio/HTTP/SSE servers, OAuth login
    ├── skills/           # skill discovery and parsing
    ├── extensions/       # Lua extension engine and host APIs
    ├── workspace/        # workspace paths and repository context
    └── ui/               # ratatui local/remote TUI
```

## How it works

A turn enters through dex's `process_turn` lifecycle wrapper and runs the
model/tool state machine in `dex-agent-core::run_turn`. Dex implements the
`AgentHost` callbacks for compaction, steering, session writes, tool execution,
usage accounting, and transcript events. Tool calls may run in parallel
(`write`/`edit` on distinct files); `bash`, `then_run`, or conflicting paths
serialize. The host compacts history to keep model requests within the
configured context budget. Progress is reported through a dex `Console`
(streamed lines + approval requests).

In client–server mode the daemon runs `process_turn` on a blocking thread and
translates console output into `StreamEvent`s over SSE (`crates/dex-server/src/daemon/server/`).
The TUI (`src/ui/remote/`) consumes those events from a worker thread and
renders them live; approvals and cancellation are round-tripped over
`POST .../approve` and `POST .../cancel`. The local TUI (`src/ui/app.rs`) runs
the same loop in-process with direct channels.
