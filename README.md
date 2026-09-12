# dex

A terminal coding agent written in Rust. `dex` talks to OpenAI-compatible Chat
Completions or Responses APIs and Anthropic's native Messages wire, calls tools
(`read`, `bash`, `write`, `edit`, `ffgrep`, `fffind`, plus MCP servers) to
operate on your local files, and offers an interactive TUI, a one-shot prompt
mode, and a raw JSON tool mode. All agent work can run in a daemon over
HTTP+SSE, and conversations persist as resumable JSONL sessions.

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) for how
to build, test, and submit changes. Please follow the
[Code of Conduct](CODE_OF_CONDUCT.md), and file bugs or ideas in
[GitHub issues](https://github.com/arpitsr/dex/issues).

## Features

- **Multi-provider backends** — OpenAI-compatible Chat Completions and
  Responses endpoints (OpenAI, OpenCode Zen, Moonshot/Kimi, …), Anthropic's
  native Messages wire, and ChatGPT-backed Codex OAuth. Streaming responses,
  stalled-stream retries with idle watchdogs, automatic protocol fallback
  (responses → completions, remembered per endpoint+model), and configurable
  reasoning effort.
- **Agentic tool use** — the model can read files, run shell commands, write
  and edit files, and search the filesystem. External tools arrive via MCP
  servers (stdio or HTTP/SSE, with OAuth). Extra built-ins (`git`, `chain`)
  sit behind `DEX_EXTRA_TOOLS=1`. Tool output caching is off by default; set `DEX_TOOL_CACHE=1` to opt in.
- **Client–daemon architecture** — the TUI is a pure HTTP client; a daemon
  does the LLM calls, tools, and sessions. Attach from anywhere, reconnect
  with journal replay, and approve tool calls remotely.
- **Interactive TUI** — a `ratatui` REPL with a streaming markdown transcript,
  multi-line input, autoscroll, a status bar, and a visible
  steering/follow-up queue while the agent is working. `!` shell escape for
  direct commands without the agent.
- **Session persistence** — each conversation is saved as a crash-safe JSONL
  journal. A fresh session starts by default; use `--session` to explicitly
  continue one, or `--reattach <id>` to reattach to a daemon session. Quitting
  the TUI prints the exact resume command for that session.
- **Skills** — lightweight, discoverable agent skills (directories with a
  `SKILL.md` frontmatter) can be injected into the system prompt or loaded on
  demand via `/skill:<name>`.
- **History compaction** — when the context window is exceeded, older turns are summarized deterministically (no LLM call) to keep requests bounded. Set `DEX_COMPACTION_LLM=1` for model summarization.
- **Online context compaction** — set `DEX_ONLINE_COMPACTION=1` to add an `update_plan` tool: the model keeps a working plan, and each completed step is a safe point where history may compact early if the cache re-write cost pays for itself within the projected remaining work (horizon learned from requests-per-boundary; port of SoL-Pi's online-context-compact). Cache-write/read pricing resolves from the models.dev catalog for the running model — explicit write surcharge, else `input / cache_read` (writes bill at the plain input rate), else `1.0` when the provider prices no caching (reads then bill at the input rate, so a re-write costs what a read costs) — with a measured cross-provider fallback of 5.0 for unpriced models.
- **Evidence-preserving reducer** — set `DEX_EVIDENCE_REDUCER=1` to delegate the first read of a large build/test log (`cargo`/`pytest`/`go test`/`make`/…) to a cheap model: the raw clamped output is archived beside the session, and the accepted reduction keeps only lines that are verified byte for byte against that archive (`status` is copied from the exit code, never judged by the model; an error log that names a failure must yield at least one failure quote). Any uncheckable receipt falls open to the raw result — delegation never requires trusting a fluent summary. Port of SoL-Pi's evidence-preserving-reducer; pairs with `obs_recall` for exact readback.

- **Project instructions** — a repo-level `AGENTS.md`/`CLAUDE.md` is appended to
  the system prompt automatically.
- **Runtime logging** — `DEX_LOG=off|error|warn|info|debug|trace` with a
  TUI-safe sink (stderr outside the TUI, `dex.log` inside); no new dependencies.
- **Herdr-aware** — running inside a [Herdr](https://herdr.dev) pane
  (`HERDR_ENV=1`), dex reports `working`/`blocked`/`idle` to the Herdr sidebar
  via `pane report-agent`; no-op everywhere else.
- **No telemetry** — dex makes network calls only to the LLM providers you
  configure (plus `models.dev` for the model catalog). Nothing else.

## Install

Every `v*` tag builds binaries for `x86_64`/`aarch64` Linux (static musl — runs
on any distro, no libssl or glibc constraints), `x86_64`/`aarch64` macOS, and
`x86_64` Windows, and attaches them to the GitHub release.

```sh
curl -fsSL https://raw.githubusercontent.com/arpitsr/dex/HEAD/scripts/install.sh | sh
```

- Pin a version: `curl -fsSL .../install.sh | sh -s -- 0.2.0`
- Custom directory: `DEX_INSTALL_DIR=/usr/local/bin curl -fsSL .../install.sh | sh`
- Windows: grab `dex-v*-*-x86_64-pc-windows-msvc.zip` from the
  [releases page](https://github.com/arpitsr/dex/releases).

No further setup: dex runs with no config (defaults build cleanly), and the
daemon fetches the models.dev catalog in the background on first start
(`dex update --models` for a manual refresh). `dex doctor` prints every
resolved value with its origin.

## Building

Requires a Rust toolchain (edition 2021):

```sh
cargo build --release
# binary: target/release/dex
```

## Releasing

Creating the `v*` tag is the only manual step; CI does the rest:

```sh
scripts/release.sh patch    # tag the next version — also major, minor, or 1.2.3
# or by hand:
git tag v0.1.1 && git push origin v0.1.1
```

`.github/workflows/release.yml` then: bumps `Cargo.toml`/`Cargo.lock` on the
default branch (`develop`) to the tag's version (github-actions bot commit) →
builds all targets from that commit → smoke tests → attaches tarballs +
`SHA256SUMS` to the GitHub release. The install script resolves the latest
release and picks the right asset for the running machine. `git pull`
afterwards to pick up the version bump.

If the default branch is protected, let GitHub Actions push to it (Settings →
Branches → add the Actions bot as bypass), since the bump commit is written by
CI.

## Configuration

One config file, one precedence order, one selection knob. `dex doctor`
prints every resolved value with where it came from.

| Layer (wins first)        | Example                              |
| ------------------------- | ------------------------------------ |
| CLI flags                 | `--model`, `--base-url`              |
| Environment variables     | `DEX_MODEL`, `OPENCODE_API_KEY`, `DEX_HEADERS` |
| Config file               | `$XDG_CONFIG_HOME/dex/config.yaml` (or `$DEX_CONFIG`) |
| Built-in defaults         | provider `opencode`, model `gpt-5.6-luna` |

`model:` is the only selection knob and names provider *and* model:
`<provider>/<model>` (`<endpoint>/<model>` forces an endpoint; a bare
provider name just switches provider). Write-back keeps that form: a
`/model` or `/provider` pick updates `model:` in the file, so the switch
becomes the default for later runs. Session state still re-applies the
exact provider/model on `/resume`.

A minimal `~/.config/dex/config.yaml`:

```yaml
providers:
  opencode:
    api_key: sk-...        # the deposit place for this provider's key
model: zen/gpt-5.6-luna    # endpoint-or-provider / model
```

No file at all also works: export the provider's key and run `dex` — the
daemon bootstraps the models.dev catalog in the background, so a fresh
install needs no manual `dex update --models`.

Run `dex update --models` once to cache the models.dev catalog. After that
a bare `/model <id>` moves `base_url` to the endpoint serving that id, and
the wire protocol follows the same way: a first `/responses` failure falls
back to chat-completions once and is remembered, so per-model knowledge
never needs configuring. An explicit `--base-url` pins the endpoint —
prefixes become naming only and are stripped. Manual overrides are escape hatches only:
`DEX_MODEL_APIS="id=openai-completions,..."` seeds a model's protocol (full
`endpoint/id` key beats bare id). Do NOT set a global `api:` to fix one
model — it pins every model and disables the automatic fallback.

Deprecated file keys are still honored with a one-time warning — move them
to the canonical spots:

| Deprecated                | Replacement                                        |
| ------------------------- | -------------------------------------------------- |
| `active_provider:` / `provider:` | put the provider in `model:` as `provider/model` |
| top-level `base_url:`     | `base_url:` under the provider's entry in `providers:` |
| top-level `api:`          | `api:` under the provider's entry in `providers:`  |
| top-level `headers:` / `http_headers:` | `headers:` under the provider's entry (provider-scoped) or `DEX_HEADERS` (global) |
| `DEX_PROVIDER` env        | `DEX_MODEL=<provider>/<model>`                     |
| `OPENAI_HEADERS` / `ANTHROPIC_CUSTOM_HEADERS` env | `DEX_HEADERS` (same syntax) |

Unknown keys are called out by name (`dex: unknown config key(s) ...`) and
a parse error lists the valid keys: `model`, `providers`,
`thinking_effort`, `mcp_servers` (+ the deprecated ones above). Other keys
are preserved untouched.

### Other OpenAI-compatible providers

Any models.dev provider with an OpenAI-style endpoint works without dedicated
integration. Deposit its key under `providers:` and pick it by name:

```yaml
model: zai/glm-5.3-flash
providers:
  zai:
    api_key: zsk-...       # the deposit place; or export ZHIPU_API_KEY
    # base_url: ...        # optional; defaults to the catalog endpoint
    # api: openai-completions  # optional protocol pin; learned otherwise
    # headers: {X-Custom: ...} # optional; sent only to this provider
```

Anthropic is built in as its own provider — `model: anthropic/claude-sonnet-4-5`
(or `--model anthropic`) with `ANTHROPIC_API_KEY`. It speaks the native
Messages wire (`anthropic-messages`: `x-api-key` auth, `anthropic-version`
header, block-shaped tool calls and thinking blocks replayed with their
signatures). Any other provider entry can also pin that wire with
`api: anthropic-messages` when its endpoint speaks the Messages API:

```yaml
providers:
  gateway:
    api_key: ...
    base_url: https://gateway.example/v1
    api: anthropic-messages
```

`/provider zai` and `/model zai/<id>` switch to it (the completion list shows
`zai/<id>` once configured). The endpoint, model list, pricing, context
windows and reasoning options come from the cached models.dev catalog — run
`dex update --models` once. The key resolves per provider: config
`providers.<name>.api_key` > the provider's own documented env var (from the
catalog, e.g. `ZHIPU_API_KEY`, `OPENROUTER_API_KEY`); opencode's is
`OPENCODE_API_KEY`. There is no per-provider default key var outside the
catalog — one provider's key never leaks into another. A model's advertised thinking options (e.g. `low/high/max`)
are shown in the `/model` confirmation; `/thinking <level>` pins one per model
(remembered per endpoint+model and validated against the advertised list —
unknown models accept anything, a stale catalog never blocks).
`DEX_THINKING_EFFORT` is the fallback when nothing is pinned, and an effort no
model advertises warns once instead of failing opaquely at the API. A file
`thinking_effort:` default sits under both (stored choice > env > file).
Wire protocol resolves like opencode: responses first, one fallback to
completions, remembered per endpoint+model. Native-protocol-only providers
(no OpenAI-compatible endpoint in the catalog, e.g. anthropic) are not
selectable this way.

For ChatGPT-backed Codex, first run `codex --login`, then:

```sh
DEX_MODEL=openai-codex dex
```

`dex` reads the current access token and account ID from
`CODEX_ACCESS_TOKEN`/`CODEX_ACCOUNT_ID` or `$CODEX_HOME/auth.json`
(default `~/.codex/auth.json`). Run `codex --login` again when the local token
expires.

## Usage

Run `dex` with no arguments to launch the interactive TUI:

```sh
dex
```

Ask it to do something:

```
> read src/main.rs and summarize what it does
```

### Doctor

`dex doctor` shows the fully resolved configuration — provider, endpoint,
model, wire protocol, key source, thinking effort, catalog state — each with
the origin (flag > env > file > default). It never touches the network and is
the first thing to run when setup misbehaves.

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
available as `$DEX_BIN`, so ONE bash call can stitch a whole read-only
pipeline — search locally, read excerpts, print only the distilled result —
while intermediate output never enters the conversation:

```sh
"$DEX_BIN" run ffgrep pattern=TODO output_mode=files | while IFS= read -r f; do
  "$DEX_BIN" run read "path=$f" limit=3
done
```

### Model catalog

`dex update --models` refreshes the cached model catalog (context windows
and `/model` autocomplete). A fresh daemon bootstraps it in the background,
so this is a manual refresh, not a required step.

```sh
dex update --models
```

### Self-update

Bare `dex update` replaces the running binary with the latest release
(`--all` also refreshes the model catalog). It resolves the latest version
via the `/releases/latest` redirect, verifies the download against the
release's `SHA256SUMS`, and atomically renames the new binary over the
running one, so a crash mid-update can never leave a torn install. It
refuses (with a pointer at the right command) when the binary was built
from source, installed via `cargo install`, or is a Windows build. It honors
the same `DEX_REPO` and `DEX_VERSION` envs as `scripts/install.sh`:

```sh
dex update                     # latest release
DEX_VERSION=v0.4.0 dex update  # pin (or downgrade to) a release
dex update --all               # self-update + refresh the model catalog
```

### Client–server mode

The TUI is a pure HTTP client; all agent work (LLM calls, tools, sessions)
happens in a daemon. `dex` with no arguments starts a daemon in the
background and attaches the TUI to it:

```sh
dex                     # daemon on a random localhost port + TUI
```

Run the daemon headless (e.g. on a remote machine, in the directory you want
as the agent workspace) and connect the TUI from anywhere:

```sh
dex serve               # daemon on 127.0.0.1:8420
dex serve 0.0.0.0:8420  # reachable from other machines
dex connect http://127.0.0.1:8420
dex connect http://10.0.0.5:8420 "explain the Cargo.toml dependencies"  # one-shot
```

A non-loopback bind always requires a bearer token: the daemon generates one
and prints it, clients present `DEX_DAEMON_TOKEN=<token>` (or the token file
at `$XDG_DATA_HOME/dex/daemon.token`). Loopback-only daemons need no token
unless `DEX_DAEMON_TOKEN` is set (explicit env wins everywhere). One machine,
one token file: two daemons share it (second overwrites), so multi-daemon
clients must pass per-host `DEX_DAEMON_TOKEN` explicitly.

Reconnects: a dropped TUI replays the journal from its cursor and re-POSTs
with the same idempotency key. Completed turns replay their terminal event;
still-running turns answer 409 (reattach with `--reattach`); turns that died
with no terminal re-execute (idempotency can't dedup what never finished).
One reconnect per turn, then an honest error.

The TUI behaves exactly like the local one: assistant text streams live,
tool calls and results appear as they happen, tool approvals pop up as an
overlay (the daemon parks the turn until you decide), and Ctrl+C/Esc cancels
the in-flight turn (a third Ctrl+C force-quits a stuck turn). API keys, the model, and the permission mode are
resolved by the daemon's own environment; client flags like
`--model` and `--permission` are forwarded as per-request overrides.

Note that tools execute on the machine where the daemon runs, confined to
the daemon's working directory.

## Command-line flags

| Flag               | Description                                              |
| ------------------ | -------------------------------------------------------- |
| `--base-url <url>` | Override the API base URL for this run (pins the endpoint; routing prefixes become naming only). |
| `-H`, `--header <"Name: Value">` | Extra provider header (repeatable; `Name=Value` or JSON object also accepted). |
| `--model <name>`   | Override the model for this run: `<provider>/<model>`, `<endpoint>/<model>`, or a bare provider name. |
| `-s`, `--session <path>` | Open/continue a specific session file.             |
| `--no-session`     | Disable session persistence for this run.                |
| `-n`, `--new`      | Start a new session (the default).                       |
| `--permission <mode>` | Tool permissions: `read-only`, `ask-writes`, `ask-shell`, or `trusted` (default `trusted`). |
| `--name <name>`    | Name the session (default `<workspace>-<7 chars>`, e.g. `dex-k3m9x2q`).                                    |
| `--reattach <id>`  | Attach to an existing daemon session and replay its event journal (bare `dex` or `dex connect <url>`, no prompt). |
| `--skill <dir>`    | Add an extra skill directory to discover skills from.    |
| `--tool`           | Run raw JSON tool mode (read JSON lines from stdin).     |

Tool safety defaults to `trusted` (no approval popups). Set `DEX_PERMISSION=ask-writes`
or pass `--permission` to approve writes and shell commands. Paths are confined to the current
workspace; `bash` can execute arbitrary commands in that workspace and should
only be enabled in trusted environments. Shell commands default to 120
seconds and 1 MiB per output stream. HTTP requests default to 10 seconds to
connect and 300 seconds overall. Sessions journal to `$XDG_DATA_HOME/dex/sessions/*.jsonl` with `fsync` only on `turn_*`/`effect_*` (set `DEX_DURABLE=1` for per-line). Audit to `audit.jsonl` is off by default (`DEX_AUDIT=1` to enable). Configure limits with `DEX_TOOL_*` and `DEX_HTTP_*` environment variables
(see table below).

In the interactive TUI, actions requiring approval open a dedicated overlay.
Use the arrow keys and Enter to choose `Allow once`, `Allow for this session`,
or `Deny`; `y`, `s`, and `n` are direct shortcuts, and Esc denies.

Any other arguments are treated as a one-shot prompt. Subcommands (`serve`,
`connect`, `run`, `update`, `mcp`, `doctor`) are covered under
Usage / MCP servers above; `--help`/`-h` and `--version`/`-V` print help
and version without touching config or network.

## TUI slash commands

| Command             | Description                                          |
| ------------------- | ---------------------------------------------------- |
| `/quit`             | Exit the REPL.                                       |
| `/permissions`      | Show permission mode and workspace.                  |
| `/mcp`              | Show MCP servers, tools, and connection errors.      |
| `/clear`            | Clear the conversation history (keeps the system prompt). |
| `/new`              | Start a new session and clear history.               |
| `/session`          | Show the current session id, path, and turn count.   |
| `/resume [index|path]` | List sessions, or resume one by index/path.       |
| `/name <name>`      | Rename the current session.                          |
| `/skill:<name>`     | Load a skill's full content into the conversation.    |
| `/model`           | Show the current model and wire protocol.            |
| `/model <name>`     | Switch the model for the rest of the session.         |
| `/thinking [<level>\|clear]` | Show or set reasoning effort.           |
| `/waive <reason>`   | Waive verification with a reason.                    |
| `/undo`             | Undo the last recorded file change.                  |
| `/provider`        | Show the current and available providers.             |
| `/provider <name>` | Switch provider for the rest of the session.          |
| `/help` (unknown)   | Unknown commands print a hint.                        |

### Shell escape

Prefix any TUI input with `!` to run it as a shell command directly, without
involving the agent:

```
> !ls -la
> !!cargo test -- --nocapture
```

The command runs in the daemon workspace via the `bash` tool and renders as
a tool block. It needs no approval — the `!` itself is the approval, even in
`read-only` mode (that mode constrains the model, not your own typing) — and
the run is saved to session history: `!` output feeds the model's context on
the next turn, while `!!` stays visible in the transcript but is never sent
to the model. A shell run may overlap an agent turn; only one `!`
runs at a time per session — a second is refused until the first finishes,
and `Esc`/`Ctrl+C` cancels it. `dex "!<command>"` and
`dex connect <url> "!<command>"` do the same without the TUI.

### Keyboard controls

- **Enter** — submit the current input.
- **Tab** — autocomplete the selected slash command, provider, or model; **↑/↓** navigate suggestions; **Esc** — discard the draft and close the popup.
- **Shift+Enter** — insert a newline (multi-line input). Needs a terminal with Kitty keyboard-protocol support (e.g. Ghostty, Kitty, WezTerm, foot); otherwise use **Ctrl+J**, which works everywhere.
- **Enter while working** — queue a steering message for the next model boundary.
- **Alt+Enter while working** — queue a follow-up for after the current task.
- **Alt+Up while working** — recall queued steers (newest first), then follow-ups, back into the composer to edit them before delivery; press again for the next one.
- **Esc** or **Ctrl+C** — cancel the active turn and restore queued messages (a third Ctrl+C force-quits a stuck turn); **Ctrl+C** with a drafted prompt clears it first, and **Ctrl+D** on an empty line quits.
- **Ctrl+T** — expand/collapse the full thinking block.
- **Alt+V** — cycle your voice color (plain by default; magenta → sky → peach → violet → rose → amber → coral → plain); the composer and new prompts use it, already-sent rows keep theirs.
- **PageUp/PageDown**, **Shift+Up/Down**, or **mouse wheel** — scroll the transcript.
- **Paste** — pasted text is inserted at the cursor.
- **Mouse wheel** — scrolls the transcript. **Drag** — selects transcript text with a visible highlight and copies it to the clipboard on release (OSC 52; a click just clears). **Shift+drag** (Option+drag in iTerm2) still bypasses mouse reporting for native selection; tmux users may need `set -g set-clipboard on`.

### Steering and follow-ups

While `dex` is working, the input remains available. Submitted steering and
follow-up messages stay visible in the queue directly above the input box until
the worker accepts them. Steering is delivered before the next model call;
follow-ups wait until the current task has finished. The queue is kept separate
from the transcript so pending messages do not scroll away. **Alt+Up** recalls
queued steers (newest first), then follow-ups, back into the composer for
editing (the daemon drops its queued copy); a message already accepted at a
model boundary has been injected and can no longer be recalled.

## Sessions

Sessions are stored as JSONL files under:

- `$XDG_DATA_HOME/dex/sessions` (or `~/.local/share/dex/sessions`),
- organized in subdirectories by a slug of the current working directory.
- new sessions are named `<workspace>-<7 chars>` (workspace directory plus a k8s-style suffix, e.g. `dex-k3m9x2q`); override with `--name` or `/name`.

Each file starts with a `session` header line followed by `message` entries and
optional `session_info` (rename) entries. Entries are appended after every
turn, so a crash or Ctrl+C loses at most the in-progress turn. Starting `dex` in
a directory creates a fresh session; use `--session <path>` to continue a saved
one.

## Skills

Skills are discovered from these directories (first match wins per directory):

- `<cwd>/.dex/skills`
- `<cwd>/.agents/skills`
- `$XDG_CONFIG_HOME/dex/skills` (or `~/.config/dex/skills`)

A skill is a directory containing a `SKILL.md` file with YAML frontmatter:

```markdown
---
name: my-skill
description: Short description surfaced to the model.
---

Detailed instructions / reference content...
```

Only the `name` and `description` are included in the system prompt; the full
body is loaded on demand via `/skill:<name>` or when the conversation
references it.

## Tools

The agent can call the following tools (each maps to a function in the API
schema):

| Tool    | Purpose                                                          |
| ------- | -----------------------------------------------------------------|
| `read`  | Read a file (`path`, `paths`, `glob`). Line-numbered, tab-expanded. |
| `bash`  | Run a shell command via `sh -c` (`command`).                     |
| `write` | Write/overwrite a file (`path`, `content`, optional `then_run`).  |
| `edit`  | Replace exactly one occurrence of text (`path`, `oldText`, `newText`, optional `then_run`). |
| `ffgrep` | Fast frecency-ranked content search (fff engine): regex or plain text, typo-tolerant fuzzy fallback, respects `.gitignore` (`pattern`, `output_mode`, `file_offset`); truncated results end with a counted `[... more exist ...]` trailer naming the next `file_offset`. |
| `fffind` | Fuzzy frecency-ranked file-path search (fff engine, typo-tolerant) (`pattern`, `limit`). |
| `git`*   | Inspect repo status/diff (`mode`). Behind `DEX_EXTRA_TOOLS=1`.   |
| `chain`* | Bounded read-only search→read in one round trip. Behind `DEX_EXTRA_TOOLS=1`. |
| `delegate`* | Spawn a background sub-agent (`explorer`/`reviewer`/`tester`) and return its id immediately. Daemon sessions only. |
| `delegate_output`* | Bounded wait (≤120 s) or poll for a delegated child's result. |
| `delegate_stop`* | Cancel a running child and return its terminal result. |
| `update_plan`† | Replace the complete working plan (`steps`, optional `progress`). A completed step is a compaction boundary. Behind `DEX_ONLINE_COMPACTION=1`. |
| `obs_recall`‡ | Read one page of an archived large tool result (`id`, optional byte `offset`); continue with the returned `next_offset`. Behind `DEX_OBSERVATION_PACK=1`. |

`*` behind `DEX_EXTRA_TOOLS=1` — default is 6 tools. `†` behind `DEX_ONLINE_COMPACTION=1`. `‡` behind `DEX_OBSERVATION_PACK=1` — large tool results (> 10 KB) are sent in full for their first 2 provider requests, then replaced with a placeholder; the original bytes are archived beside the session JSONL and paged back with this tool. Tool results are truncated before being sent back to the model, and a result
cache (`dex-tool-cache.json`) is kept only when `DEX_TOOL_CACHE=1`. `write`/`edit` on distinct files run in parallel; same `path`, any `bash`, or any call carrying `then_run` (which runs a shell command) serializes the batch.

`write`/`edit` take an optional `then_run` shell command that runs in the same call *after* a successful change, so a build, formatter or
test result arrives with the mutation instead of costing another round trip. It is skipped when the change fails, and the observation
reports `[then_run:succeeded]` / `[then_run:failed (exit N)]` followed by the command's clamped output; a command that times out, is
cancelled, or fails to spawn reports plain `[then_run:failed]` with the reason as its output (no phantom exit code). Because it executes
shell, a call carrying `then_run` clears the *shell* permission gate (not the write gate), requires `bash` in a child agent's tool allowlist,
and is logged to the audit trail as its own `bash` record.

### Sub-agents

`delegate` hands a self-contained task to a background child (three built-ins: `explorer`, `reviewer`, `tester`) that runs the same
turn loop with its own context, tool allowlist, and JSONL transcript (`$XDG_DATA_HOME/dex/sessions/<slug>/agents/*.jsonl`).
Completions are announced at the next turn boundary and, while the session is idle with a client attached, a wake turn surfaces
them immediately (off with `agent_wake: false` / `DEX_AGENT_WAKE=0`). `delegate_output` fetches a result on demand. Children
cannot delegate (depth 1). Under `ask-*` modes a mutating call parks a labeled prompt in the session's approval queue —
"explorer wants to run bash: …" — unanswered for five minutes it denies; session-level "allow" approvals apply to children too.
Set `DEX_SUBAGENTS=0` to unregister the tools.

### MCP servers

External tools via [Model Context Protocol](https://modelcontextprotocol.io) (stdio command or HTTP/SSE URL), declared under `mcp_servers:` in
`config.yaml` (`DEX_MCP_SERVERS_JSON` wins when set — same shape as JSON):

```yaml
mcp_servers:
  github: "npx -y github-mcp-server"          # shorthand: command + args
  docs:
    url: "https://docs.example.com/mcp"       # HTTP/SSE server
    headers: { authorization: "Bearer ${DOCS_TOKEN}" }  # $VAR expands, fail-closed
    timeout_secs: 30
    allow: ["search"]                          # optional tool filter (deny wins)
```

Each server's tools appear in the schema as `mcp__<server>__<tool>` (description prefixed with `[<server>]`), plus one
`mcp__<server>_read_resource` reader when the server hosts resources. The merged schema is capped at `DEX_MCP_MAX_TOOLS`
(default 200, sorted by name, dropped count reported); servers connect in the background at startup and a 60s liveness
sweeper marks dead ones `down` before the next turn uses them. `GET /api/mcp` shows per-server state/tool counts plus the
truncated total; `POST /api/mcp/{server}/reconnect` redials a fixed server without restarting the daemon.

HTTP servers with OAuth (RFC9728 protected-resource + RFC8414 discovery + RFC7591 registration + PKCE S256) log in
via `dex mcp login <server>` (browser + loopback callback), `dex mcp logout <server>`, `dex mcp status`; `/mcp`
shows the same auth lines. Tokens live in `$XDG_DATA_HOME/dex/mcp/<server>.json` (0600, never logged), refresh
once per 401 with a 60s backoff on failure (`invalid_grant` drops the file). Discovery/token URLs must be https
(loopback http allowed); optional `oauth_client_id`/`oauth_client_secret`/`oauth_scope` in config skip registration.
When an AS rejects `resource` with `invalid_target`, login retries once without it.

## Environment variables

| Variable             | Description                                              |
| -------------------- | -------------------------------------------------------- |
| `OPENCODE_API_KEY`   | API key for the opencode gateway (required for `opencode`; export it in your shell profile). |
| `ANTHROPIC_API_KEY`  | API key for the built-in `anthropic` provider (`model: anthropic/<model>`); resolves cache-less. |
| `DEX_HEADERS` / `OPENAI_HEADERS` / `ANTHROPIC_CUSTOM_HEADERS` | Extra provider headers (JSON object or `Name: Value` pairs, comma/newline separated; later var wins: `ANTHROPIC_*` < `OPENAI_*` < `DEX_*`). File `headers:`/`http_headers:` < env < `--header`. `authorization` can't be overridden. `OPENAI_HEADERS`/`ANTHROPIC_CUSTOM_HEADERS` are deprecated aliases — use `DEX_HEADERS`. |
| `DEX_MODEL` | Model selection, `provider/model` (`endpoint/model` or a bare provider name work too) — the same knob as the file's `model:` key. |
| `DEX_PROVIDER` | Deprecated provider selection — use `DEX_MODEL=<provider>/<model>` (still honored with a one-time warning). |
| `CODEX_ACCESS_TOKEN` | Optional Codex OAuth access-token override.                |
| `CODEX_ACCOUNT_ID`   | Account ID paired with `CODEX_ACCESS_TOKEN`.               |
| `DEX_MODELS` | Comma-separated models for `/model` autocomplete (default: catalog cache). |
| `DEX_HTTP_CONNECT_TIMEOUT_SECS` | HTTP connect timeout (default 10). |
| `DEX_HTTP_REQUEST_TIMEOUT_SECS` | Total request bound, applied only when explicitly set — streaming LLM/chat paths default to no total timeout so long turns aren't killed. |
| `DEX_TOOL_TIMEOUT_SECS` | Shell command timeout in seconds (default 120). |
| `DEX_TOOL_OUTPUT_BYTES` | Maximum captured stdout/stderr bytes per stream (default 1 MiB). |
| `DEX_STREAM_IDLE_TIMEOUT_SECS` | SSE idle watchdog: no chunk (or keep-alive) for this long counts as a stall (default 90, 300 for reasoning-capable models; `0` disables). Pre-output stalls and dropped connections retry automatically same-protocol (2 retries) before failing the turn. |
| `DEX_MAX_TOOL_ITERATIONS` | Per-turn cap on tool rounds — one round per assistant batch with calls, not per call (default 200). A looping model is stopped with partial progress preserved and a transcript marker. |
| `DEX_DAEMON_TOKEN` | Bearer token for the daemon API. Required by clients when `dex serve` binds a non-loopback address (auto-generated and written to `$XDG_DATA_HOME/dex/daemon.token`, 0600) or when the operator sets one. Loopback-only daemons need no token. |
| `DEX_MODEL_APIS` | Per-model wire protocol table (`id=api,...`; full `endpoint/id` key beats bare id). |
| `DEX_THINKING_EFFORT` | Default reasoning effort (a stored `/thinking` choice wins; file `thinking_effort:` is the fallback). |
| `DEX_PERMISSION` | Tool permission mode (`read-only`, `ask-writes`, `ask-shell`, or `trusted`; default `trusted`). |
| `DEX_LOG` | Runtime log level: `off`, `error`, `warn` (default), `info`, `debug`, `trace` — works on release builds. Logs go to stderr, or to `$XDG_DATA_HOME/dex/dex.log` while the TUI runs. `debug` covers provider requests/responses and tool runs; `trace` adds raw provider SSE lines. |
| `DEX_VERIFY`    | Verification hook: `1` auto-detects `cargo test`/`go test`/`npm test`; or set to a command. Off by default. |
| `DEX_COMPACTION_LLM` | `1` to use LLM summarization for compaction (default deterministic). |
| `DEX_ONLINE_COMPACTION` | `1` to enable online context compaction: adds the `update_plan` tool and compacts at completed plan steps when the cache re-write pays for itself. While on, the fixed message-count cap is suspended (the token threshold still bounds growth). |
| `DEX_OBSERVATION_PACK` | `1` to enable the observation pack: tool results > 10 KB stop being re-sent after a 2-request grace period and are replaced with placeholders; `obs_recall` pages the archived original back. Compaction, resume, and fork still see intact history. |
| `DEX_EVIDENCE_REDUCER` | `1` to enable the evidence-preserving reducer: large diagnostic tool results (`cargo`/`pytest`/`go test`/`make`/…) are reduced to a receipt whose quotes are verified byte for byte against the archived raw output; an uncheckable receipt falls back to the raw result. Requires a daemon session and the observation pack (`DEX_OBSERVATION_PACK=1`), so a receipt always keeps a recallable source archive. |
| `DEX_REDUCER_MODEL` | Model selection (`provider/model`, same syntax as `DEX_MODEL`) for the evidence reducer's cheap delegate call. Unset: the main model is used (still verified, just not cheap). |
| `DEX_DURABLE`   | `1` to `fsync` every session line (default only `turn_*`/`effect_*`). |
| `DEX_AUDIT`     | `1` to write `audit.jsonl` per tool call (default off; session already journals). |
| `DEX_EXTRA_TOOLS` | `1` to expose `git`+`chain` to the model (default 6 tools). |
| `DEX_SUBAGENTS` | `0` to unregister the `delegate`/`delegate_output`/`delegate_stop` tools (default on in daemon sessions). |
| `DEX_AGENT_WAKE` | `0` to disable idle wake turns (default on): when a child agent completes while the session is idle and a client is listening, the daemon runs one wake turn to surface the notice. |
| `DEX_MCP_SERVERS_JSON` | MCP servers as JSON (same shape as `mcp_servers:` in config; wins over the file, handy for tests). |
| `DEX_MCP` / `DEX_NO_MCP` | `0`/`off`/`false`/`no` (or `DEX_NO_MCP=1`) disables all MCP servers. |
| `DEX_MCP_MAX_TOOLS` | Cap on merged MCP schema tools (default 200; head kept sorted by name). |
| `DEX_COST_PER_1K` | Fallback token cost per 1k tok (prompt + completion) for the status-bar spend figure when the pricing catalog has no entry (default `0.002`). |
| `DEX_CONTEXT_WINDOW` | Override model context window (per-model from catalog when unset). |
| `DEX_RESERVE_TOKENS` | Tokens reserved for reply (default 16384). |
| `DEX_KEEP_RECENT_TOKENS` | Recent tokens kept on compaction (default 20000). |
| `DEX_TOOL_CACHE` | `1` to cache tool results across runs (`dex-tool-cache.json`; default off). |
| `DEX_CONFIG` | Override the config file path (default `$XDG_CONFIG_HOME/dex/config.yaml`). |
| `DEX_REPO` | GitHub repo `dex update` downloads releases from, `owner/name` (default `arpitsr/dex`). |
| `DEX_VERSION` | Version pin for `dex update`: `vX.Y.Z` (bare or `V`-prefixed also accepted) installs that exact release, enabling downgrades; `latest` (default) follows the newest release. Both also honored by `scripts/install.sh`. |
| `CODEX_HOME` | Directory holding Codex `auth.json` (default `~/.codex`). |
| `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_CACHE_HOME` | XDG base dirs for config/data/cache. |
| `HOME`               | Fallback when XDG vars are unset.                        |

Restore strict harness: `DEX_DURABLE=1 DEX_AUDIT=1 DEX_EXTRA_TOOLS=1 DEX_VERIFY=1 DEX_COMPACTION_LLM=1 dex`

## Project structure

```
dex/
├── .dex/
│   └── skills/           # (optional) project-level agent skills
├── target/
├── Cargo.lock
├── Cargo.toml
├── README.md
└── src/
    ├── main.rs           # entry point: mode resolution, daemon bootstrap
    ├── cli.rs            # argument parsing / invocation mode
    ├── protocol/         # client<->daemon wire types (HTTP + SSE events)
    ├── client/           # HTTP client: SSE turn streaming, approvals, REPL
    ├── daemon/           # axum daemon: sessions, chat SSE, approve, cancel
    ├── agent/            # turn loop, steering, compaction, tool state
    ├── core/             # console sinks, formatting, highlighting, types
    ├── llm/              # provider clients, streaming parsers, auth, config
    ├── session.rs        # JSONL session persistence
    ├── tools/            # builtin tools: read, bash, write, edit, ffgrep, fffind (fff engine)
    ├── mcp.rs + mcp/     # MCP client: stdio/HTTP/SSE servers, OAuth login
    ├── skills.rs         # skill discovery
    └── ui.rs + ui/       # ratatui TUI (local event loop + remote client UI)
```

## How it works

A turn runs in `src/agent/loop.rs` (`process_turn`): it repeatedly calls the
model with tools enabled, executes any requested tool calls in parallel (`write`/`edit` on distinct files in parallel; `bash`, any `then_run`, or same `path` serializes), feeds
results back, and compacts history deterministically once `tokens > contextWindow - reserveTokens` (`reserve=16384`, `keepRecent=20000` tokens, per-model `contextWindow` from catalog/`DEX_CONTEXT_WINDOW`). The optional `DEX_VERIFY` hook is off by default. Progress is reported through a `Console` (streamed lines + approval
requests).

In client–server mode the daemon runs `process_turn` on a blocking thread and
translates console output into `StreamEvent`s over SSE
(`src/daemon/server.rs`). The TUI (`src/ui/remote.rs`) consumes those events
from a worker thread and renders them live; approvals and cancellation are
round-tripped over `POST .../approve` and `POST .../cancel`. The local TUI
(`src/ui/event.rs`) runs the same loop in-process with direct channels.

## Acknowledgments

`dex` interoperates with conventions from across the terminal-agent
ecosystem: `CLAUDE.md` project instructions and `ANTHROPIC_CUSTOM_HEADERS`
(Claude Code), session headers and response-first wire negotiation on the
`opencode` gateway (OpenCode), OAuth via `codex --login` (Codex),
token-based compaction settings (`pi-mono`), and `fff-search` file search
(`fff.nvim`). Model metadata comes from the models.dev catalog.

## Support

- Bugs: open an [issue](https://github.com/arpitsr/dex/issues/new?template=bug_report.md)
  with `dex --version`, redacted config, and steps to reproduce.
- Ideas: open a [feature request](https://github.com/arpitsr/dex/issues/new?template=feature_request.md).
- Security: do not open a public issue — see [SECURITY.md](SECURITY.md).
- There is no chat or discussion forum; GitHub issues are the contact channel.

## Contributing

Contributions are welcome. There is no formal roadmap — open or pick up an
issue labeled [`good first issue` or `help wanted`](https://github.com/arpitsr/dex/issues).
Please read [CONTRIBUTING.md](CONTRIBUTING.md) first and follow the
[Code of Conduct](CODE_OF_CONDUCT.md). By contributing you agree your work
is dual-licensed MIT/Apache-2.0 like the rest of the project.

Status: pre-`1.0` (`0.x`) — usable daily but expect breaking changes until a
`1.0` release.

Note: the name `dex` collides with an unrelated `dex` crate on crates.io, so
`dex` is distributed via [GitHub releases](https://github.com/arpitsr/dex/releases)
and `scripts/install.sh`, not `cargo install`.

## License

`dex` is dual-licensed under the [MIT](LICENSE) and
[Apache-2.0](LICENSE-APACHE) licenses, at your option (`SPDX: MIT OR
Apache-2.0`). See [CONTRIBUTING.md](CONTRIBUTING.md) and
[SECURITY.md](SECURITY.md) for how to contribute and report security issues.
