# dex

A terminal coding agent written in Rust. It calls tools (`read`, `bash`,
`write`, `edit`, `grep`, `find`, `ls`, plus MCP servers) to work on your local
files, talks to OpenAI-compatible, Anthropic, and Codex providers, and saves
every conversation as a resumable JSONL session.

The TUI is a pure HTTP client — a daemon does the LLM calls, tools, and
sessions over HTTP+SSE — and that engine is a workspace of reusable crates
(`dex-ai`, `dex-agent-core`, `dex-session`, …), so the harness pieces can be
reused to build your own agent.

## Features

- **Multi-provider backends** — OpenAI-compatible Chat Completions/Responses
  endpoints, Anthropic's Messages API, and ChatGPT-backed Codex OAuth, with
  streaming, retries, and automatic protocol fallback. See
  [Configuration](docs/configuration.md).
- **Agentic tool use** — the model reads, runs, writes, edits, and searches
  your files. Bring your own tools via MCP servers (stdio or HTTP/SSE, with
  OAuth); `DEX_TOOL_CACHE=1` opts into result caching.
- **Client–daemon architecture** — the TUI is a pure HTTP client; the daemon
  does the LLM calls, tools, and sessions. Attach from anywhere, reconnect
  with journal replay, and approve tool calls remotely.
- **Interactive TUI** — a `ratatui` REPL with a streaming markdown transcript,
  multi-line input, autoscroll, a status bar, and a follow-up queue while the
  agent works. `!` runs shell commands without the agent.
- **Session persistence** — each conversation is a crash-safe JSONL journal.
  Use `--session` to continue one, or `--reattach <id>` to reattach to a
  daemon session (quitting the TUI prints the exact resume command).
- **Skills** — teach the agent something by dropping a directory with a
  `SKILL.md` frontmatter; inject into the system prompt or load on demand via
  `/skill:<name>`. See [Sessions and skills](docs/sessions-and-skills.md).
- **History compaction** — when the context window fills, older turns are
  summarized deterministically (no LLM call). `DEX_COMPACTION=llm` or `=jev`
  switch strategies — see [Environment variables](docs/environment.md).
- **Modular by design** — providers, the agent engine, sessions, protocol, and
  server live in separate crates; see [Architecture](docs/architecture.md) and
  [Runtime composability](docs/runtime.md).
- **Project instructions** — a repo-level `AGENTS.md`/`CLAUDE.md` is appended
  to the system prompt automatically.
- **Runtime logging** — `DEX_LOG=off|error|warn|info|debug|trace` with a
  TUI-safe sink (stderr outside the TUI, `dex.log` inside).
- **Herdr-aware** — running inside a [Herdr](https://herdr.dev) pane
  (`HERDR_ENV=1`), dex reports `working`/`blocked`/`idle` to the Herdr
  sidebar; no-op everywhere else.
- **No telemetry** — dex makes network calls only to the LLM providers you
  configure (plus `models.dev` for the model catalog). Nothing else.

## Documentation

| Document | Contents |
| --- | --- |
| [docs/getting-started.md](docs/getting-started.md) | Install → configure → verify → first run, everyday usage, troubleshooting |
| [docs/configuration.md](docs/configuration.md) | Config file, precedence, `model:` knob, provider examples, legacy keys, system prompt |
| [docs/tui.md](docs/tui.md) | Slash commands, `!` shell escape, agent modes, keyboard controls, steering queue |
| [docs/tools.md](docs/tools.md) | Built-in tools, `then_run`, sub-agents (`delegate`), MCP servers |
| [docs/sessions-and-skills.md](docs/sessions-and-skills.md) | JSONL session storage, skill discovery and `SKILL.md` format |
| [docs/extensions.md](docs/extensions.md) | Lua extensions: tools, hooks, harness slots, `agent_loop` |
| [docs/environment.md](docs/environment.md) | Every `DEX_*` and provider environment variable |
| [docs/architecture.md](docs/architecture.md) | Workspace crates, project structure, how a turn works |
| [docs/releasing.md](docs/releasing.md) | Release/tag process and CI |
| [docs/runtime.md](docs/runtime.md) | Runtime composability: slots, profiles, replaceable agent loop |

## Install

Every `v*` tag builds binaries for `x86_64`/`aarch64` Linux (static musl — runs
on any distro, no libssl or glibc constraints), `x86_64`/`aarch64` macOS, and
`x86_64` Windows, and attaches them to the GitHub release.

```sh
curl -fsSL https://raw.githubusercontent.com/sumofbytes/dex/HEAD/scripts/install.sh | sh
```

- Pin a version: `curl -fsSL .../install.sh | sh -s -- 0.2.0`
- Custom directory:
  `DEX_INSTALL_DIR=/usr/local/bin curl -fsSL .../install.sh | sh`
- Windows: grab `dex-v*-*-x86_64-pc-windows-msvc.zip` from the
  [releases page](https://github.com/sumofbytes/dex/releases).

First run needs one thing: a model — the
[Getting started guide](docs/getting-started.md) walks through it. Copy a
sample from `examples/config.yaml`, set `model: <provider>/<model>` in the
config (see [Configuration](docs/configuration.md)), and run `dex doctor` to
verify (exit 1 means setup is still incomplete). The daemon fetches the
models.dev catalog in the background on first start (`dex update --models` for
a manual refresh).

## Building

Requires a Rust toolchain (edition 2021):

```sh
cargo build --release
# binary: target/release/dex
```

Releases are tagged with `scripts/release.sh` — see
[docs/releasing.md](docs/releasing.md).

## Usage

Run `dex` with no arguments to launch the interactive TUI:

```sh
dex
```

Ask it to do something:

```text
> read src/main.rs and summarize what it does
```

### Doctor

`dex doctor` shows the fully resolved configuration — provider, endpoint, model,
wire protocol, key source, thinking effort, catalog state — each with the origin
(flag > env > file > default). It never touches the network and is the first
thing to run when setup misbehaves. Exit 0 means the config builds cleanly,
exit 1 means `resolve ERROR` (script-checkable).

```sh
dex doctor
```

### One-shot mode

Pass a prompt as arguments to get a single answer (no TUI):

```sh
dex "explain the Cargo.toml dependencies"
```

### Raw tool mode

`dex --tool` reads JSON tool-invocation lines from stdin and prints JSON
results. Useful for piping tool calls from another process:

```sh
echo '{"name":"read","args":{"path":"Cargo.toml"}}' | dex --tool
# => {"ok":"[package]\nname = \"dex\"\n..."}
```

An empty line quits raw tool mode.

### One-shot tool mode

`dex run <tool> <key>=<value>...` executes a single tool and prints the raw
result (errors go to stderr with exit code 1). Values that look like JSON
numbers/booleans are coerced (`limit=5`, `replaceAll=true`); a single JSON
object string is also accepted. Inside agent shell commands the binary is
available as `$DEX_BIN`, so ONE bash call can stitch a whole read-only pipeline
— search locally, read excerpts, print only the distilled result — while
intermediate output never enters the conversation:

```sh
"$DEX_BIN" run grep pattern=TODO output_mode=files | while IFS= read -r f; do
  "$DEX_BIN" run read "path=$f" limit=3
done
```

### Model catalog

`dex update --models` refreshes the cached model catalog (context windows and
`/model` autocomplete). A fresh daemon bootstraps it in the background, so this
is a manual refresh, not a required step.

```sh
dex update --models
```

### Self-update

Bare `dex update` replaces the running binary with the latest release (`--all`
also refreshes the model catalog). It resolves the latest version via the
`/releases/latest` redirect, verifies the download against the release's
`SHA256SUMS`, and atomically renames the new binary over the running one, so a
crash mid-update can never leave a torn install. It refuses (with a pointer at
the right command) when the binary was built from source, installed via
`cargo install`, or is a Windows build. It honors the same `DEX_REPO` and
`DEX_VERSION` envs as `scripts/install.sh`:

```sh
dex update                     # latest release
DEX_VERSION=v0.4.0 dex update  # pin (or downgrade to) a release
dex update --all               # self-update + refresh the model catalog
```

### Client–server mode

The TUI is a pure HTTP client; all agent work (LLM calls, tools, sessions)
happens in a daemon. `dex` with no arguments starts a daemon in the background
and attaches the TUI to it:

```sh
dex                     # daemon on a random localhost port + TUI
```

Run the daemon headless (e.g. on a remote machine, in the directory you want as
the agent workspace) and connect the TUI from anywhere:

```sh
dex serve               # daemon on 127.0.0.1:8420
dex serve 0.0.0.0:8420  # reachable from other machines
dex connect http://127.0.0.1:8420
dex connect http://10.0.0.5:8420 "explain the Cargo.toml dependencies"  # one-shot
```

A non-loopback bind always requires a bearer token: the daemon generates one and
prints it, clients present `DEX_DAEMON_TOKEN=<token>` (or the token file at
`$XDG_DATA_HOME/dex/daemon.token`). Loopback-only daemons need no token unless
`DEX_DAEMON_TOKEN` is set (explicit env wins everywhere). One machine, one token
file: two daemons share it (second overwrites), so multi-daemon clients must
pass per-host `DEX_DAEMON_TOKEN` explicitly.

If the connection drops, the TUI replays the session journal and re-POSTs with
the same idempotency key: finished turns replay their result, still-running
turns answer 409 — use `--reattach <id>` to take over — and turns that died
with the daemon re-execute from their last journal event. Everything looks
local: text streams live, tool calls and results appear as they happen, tool
approvals take the composer's place (the daemon parks the turn until you decide),
and Ctrl+C cancels the in-flight turn. API keys and permissions resolve in the
daemon's environment; client flags like `--model` are forwarded as
per-request overrides. Tools execute on the daemon's machine, confined to its
working directory.

## Command-line flags

| Flag                             | Description                                                                                                       |
| -------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `--base-url <url>`               | Override the API base URL for this run (pins the endpoint; routing prefixes become naming only).                  |
| `-H`, `--header <"Name: Value">` | Extra provider header (repeatable; `Name=Value` or JSON object also accepted).                                    |
| `--model <name>`                 | Override the model for this run: `<provider>/<model>` (a bare provider name has no model and is rejected). |
| `--system-prompt <text>`         | Replace the built-in base system prompt for this run (project/extensions/skills still append).                    |
| `--system-prompt-file <path>`    | Read the replacement base system prompt from a file (client-side, so remote daemons work).                        |
| `-s`, `--session <path>`         | Open/continue a specific session file.                                                                            |
| `--no-session`                   | Disable session persistence for this run.                                                                         |
| `-n`, `--new`                    | Start a new session (the default).                                                                                |
| `--permission <mode>`            | Tool permission ceiling: `read-only`, `ask`, or `trusted` (default `trusted`). |
| `--mode <plan\|manual\|auto>`    | Agent mode for this run (`plan` = read-only + planning directive); clamped to the permission ceiling.               |
| `--name <name>`                  | Name the session (default `<workspace>-<7 chars>`, e.g. `dex-k3m9x2q`).                                           |
| `--reattach <id>`                | Attach to an existing daemon session and replay its event journal (bare `dex` or `dex connect <url>`, no prompt). |
| `--skill <dir>`                  | Add an extra skill directory to discover skills from.                                                             |
| `--tool`                         | Run raw JSON tool mode (read JSON lines from stdin).                                                              |

Defaults, all overridable via `DEX_*` env vars — see
[Environment variables](docs/environment.md):

- Permission ceiling: `trusted` (no approvals); set `DEX_PERMISSION=ask` or
  `--permission` to approve writes and shell commands. A client can only go
  stricter than the daemon's ceiling; the ceiling seeds the TUI's agent mode
  ([Agent modes](docs/tui.md#agent-modes)).
- Shell commands: 120 s timeout, 1 MiB per output stream (`DEX_TOOL_*`).
- HTTP: 10 s to connect, 300 s overall (`DEX_HTTP_*`).
- Sessions: journals live in `$XDG_DATA_HOME/dex/sessions/*.jsonl`, `fsync`
  on turn/effect events only (`DEX_DURABLE=1` for every line).
- Audit: off; `DEX_AUDIT=1` writes `audit.jsonl` per tool call.
- Paths: tools are confined to the current workspace (`bash` can still run
  arbitrary commands there — use `trusted` only in environments you trust).

In the interactive TUI, actions requiring approval replace the composer with an inline panel (the
transcript stays visible). Use the arrow keys and Enter to choose `Allow once`, `Allow for this session`, or
`Deny`; `y`, `s`, and `n` are direct shortcuts, and Esc denies.

Any other arguments are treated as a one-shot prompt. Subcommands (`serve`,
`connect`, `run`, `usage`, `update`, `mcp`, `doctor`) are covered under
[Usage](#usage) above and [MCP servers](docs/tools.md#mcp-servers);
`--help`/`-h` and `--version`/`-V` print help and version without
touching config or network.

## Acknowledgments

`dex` interoperates with conventions from across the terminal-agent ecosystem:
`CLAUDE.md` project instructions and `ANTHROPIC_CUSTOM_HEADERS` (Claude Code),
response-first wire negotiation on OpenAI-compatible gateways, OAuth via `codex --login` (Codex), token-based compaction settings
(`pi-mono`), and `fff-search` file search (`fff.nvim`). Model metadata comes
from the models.dev catalog.

## Support

- Bugs: open an
  [issue](https://github.com/sumofbytes/dex/issues/new?template=bug_report.md) with
  `dex --version`, redacted config, and steps to reproduce.
- Ideas: open a
  [feature request](https://github.com/sumofbytes/dex/issues/new?template=feature_request.md).
- Security: do not open a public issue — see [SECURITY.md](SECURITY.md).
- There is no chat or discussion forum; GitHub issues are the contact channel.

## Contributing

Contributions are welcome. There is no formal roadmap — open or pick up an issue
labeled
[`good first issue` or `help wanted`](https://github.com/sumofbytes/dex/issues).
Please read [CONTRIBUTING.md](CONTRIBUTING.md) first and follow the
[Code of Conduct](CODE_OF_CONDUCT.md). By contributing you agree your work is
dual-licensed MIT/Apache-2.0 like the rest of the project. New to `dex`? The
[Getting started guide](docs/getting-started.md) is the fastest way to see it
work.

Status: pre-`1.0` (`0.x`) — usable daily but expect breaking changes until a
`1.0` release.

Note: the name `dex` collides with an unrelated `dex` crate on crates.io, so
`dex` is distributed via
[GitHub releases](https://github.com/sumofbytes/dex/releases) and
`scripts/install.sh`, not `cargo install`.

## License

`dex` is dual-licensed under the [MIT](LICENSE) and [Apache-2.0](LICENSE-APACHE)
licenses, at your option (`SPDX: MIT OR Apache-2.0`). See
[CONTRIBUTING.md](CONTRIBUTING.md) and [SECURITY.md](SECURITY.md) for how to
contribute and report security issues.
