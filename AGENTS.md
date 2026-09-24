# AGENTS.md — dex

Terminal coding agent in Rust (`dex`): OpenAI-compatible Chat Completions / Responses, tool use (`read`/`bash`/`write`/`edit`/`grep`/`find`/`ls`), ratatui TUI, client↔daemon over HTTP+SSE, JSONL sessions.

## Build & check

```sh
cargo build --release          # -> target/release/dex
cargo fmt -- --check           # must pass
cargo test --all-targets       # must pass
cargo clippy --all-targets -- -D warnings  # must pass
```

Requires Rust edition 2021. Config: `$XDG_CONFIG_HOME/dex/config.yaml` (or `$DEX_CONFIG`) as defaults; CLI flags > env (`DEX_*`) > file > built-in default — see `README.md` § Configuration and `dex doctor`.

## Structure

- `crates/dex-protocol/` — standalone shared HTTP/SSE wire types used by dex clients and daemons; `src/protocol/` re-exports it for internal compatibility and keeps app-domain types private.
- `crates/dex-ai/` — provider-neutral model/message API, wire mapping, SSE parsers, and reusable HTTP auth/retry policy. Model discovery/configuration, credential refresh, and UI streaming remain app-owned in `src/llm/`.
- `crates/dex-agent-core/` — reusable permission/agent modes, plans, token accounting, deterministic compaction, context/tool-round budget policies, text limits, normalized model-response history transition, and generic model/tool turn engine (`AgentHost` boundary). Dex implements app integrations in `src/agent/turn_loop/host.rs`.
- `crates/dex-coding-agent/` — coding-agent built-in tool catalog and deterministic schema composition, native permission metadata, and host-independent tool-result cache/repeat-call policy. Dex supplies dynamic MCP/extension schemas, tool execution, rendering, and cache persistence.
- `crates/dex-client/` — standalone HTTP/SSE daemon client over `dex-protocol`; `src/client/` re-exports it for in-repo compatibility. It owns its runtime/HTTP defaults and optional local token lookup, with `with_token` for embedding hosts.
- `src/main.rs` — entry, mode resolution, daemon bootstrap
- `src/cli.rs` — arg parsing / `Mode` (`Default`/`Serve`/`Connect`/`OneShot`/`Tool`/`RunTool`/`Doctor`/`Update`/`Mcp`/`Extensions`/`Usage`/`Help`/`Version`)
- `src/daemon/` + `src/protocol/` + `src/client/` — daemon (axum + SSE), wire types, client
- `src/agent/` — `turn_loop.rs` lifecycle wrapper plus `turn_loop/host.rs` dex `AgentHost` adapter, `state.rs`, app compaction backend, `subagent/` (definitions, manager, delegate tool)
- `src/llm/` — provider clients and adapters, `config/` (provider/model/endpoint resolution, `/model` write-back, `dex doctor`), `prompt.rs` (system prompt)
- `src/tools/` — workspace-confined tools (`mod.rs`, `search.rs` for the fff engine backing `grep`/`find`, `shell.rs` exposes `$DEX_BIN`)
- `src/mcp/` — MCP client (stdio/HTTP/SSE, OAuth, `mcp_servers:` config)
- `src/session/` — append-only JSONL (`store.rs`, `header.rs`, `journal.rs`; `$XDG_DATA_HOME/dex/sessions/<slug>/*.jsonl`)
- `src/skills/` / `src/ui/` — skills discovery (`discovery.rs`, `parse.rs`), TUI (`app.rs`, `render/`, `remote/`, `slash/`)
- This file + `CLAUDE.md` (if present, nearest parent wins) is auto-appended to the system prompt via `src/llm/prompt.rs:project_context()`.

## Config surface

All of this lives in `src/llm/config/` — don't add a second way to express any of it:

- One selection knob: `model: <provider|endpoint>/<model>` (file `model:`, env `DEX_MODEL`, flag `--model`; bare provider name switches provider and keeps the model). `/model`/`/provider` write back that single key and drop the deprecated ones — never write back `active_provider:`/`provider:`/top-level `base_url:`.
- Endpoints, model lists, pricing, context windows come from the models.dev catalog (cache via `dex update --models`); wire protocol is learned per endpoint+model (`learned-apis.json`), with `providers.<name>.api:` as the pin.
- Per-provider keys live in `providers.<name>.api_key` or the provider's own catalog env var (opencode: `OPENCODE_API_KEY`). Never add per-provider default key env vars.
- Extra headers precedence: file (provider-scoped `headers:` > global, per-key) < env (`ANTHROPIC_CUSTOM_HEADERS` < `OPENAI_HEADERS` < `DEX_HEADERS`) < `--header`; `authorization` can't be overridden.
- Deprecated keys/env stay honored with `warn_once` + a pointer at the replacement; new knobs must do the same, add themselves to the unknown-key list in `load_config_file` (`config/mod.rs`), add a `dex doctor` origin row, and appear in the README env table.
- `dex doctor` prints every resolved value with its origin — keep it in sync with `from_env` when resolution changes. Config tests use `EnvRestore` + `TEST_SESSIONS_ENV_LOCK` and hermetic `XDG_CACHE_HOME`/`DEX_CONFIG` paths.
- Lua extensions (`dex extensions`, `Mode::Extensions`) are the scripting surface; don't add parallel knob types elsewhere.

## Working rules

- Batch independent `read`/`grep`/`find` into one parallel call; don't do one file per turn.
- `read` before `edit`; `edit` needs exact `oldText` (unique match, or `replaceAll: true`). `write` overwrites.
- Tools are workspace-confined (`resolve_workspace_path`); symlinks can't escape. Verify with `bash` + tests, show file paths clearly.
- `write`/`edit` on distinct `path` run in parallel; same `path` or any `bash` serializes. `read`/`grep`/`find` are read-only/idempotent.
- Tool output is truncated for the model: bash ~400 lines/32 KiB, read 2000 lines/256 KiB, fan-out caps 10 files. Use `$DEX_BIN run <tool>` inside `bash` to stitch pipelines without flooding context.
- Sessions journal incrementally (`src/session/store.rs`); `turn_start`/`turn_complete`/`turn_failed` markers — crash loses at most the in-flight event.

## Conventions

- No new dependencies without clear need — check `Cargo.toml` first, prefer stdlib/native.
- Worktrees live outside this repo (`git worktree add ../dex-<name> <branch>`), not in `.worktrees/` — in-repo worktrees are gitignored, so `ffgrep`/`fffind` never index them and they'd show duplicate hits if un-ignored. When working in a worktree, remember: only the main checkout's `grep`/`find` cover the main checkout.
- Keep `src/llm/prompt.rs` minimal; tool behavior belongs in `src/llm/protocol.rs` tool descriptions, not the prompt. Schema surface is guarded by `schema_surface_matches_docs` (`src/lib.rs`) — git/chain stay dropped, delegate stays one tool with `action: spawn|wait|stop|list`.
- Skills: directory with `SKILL.md` frontmatter (`name`, `description`). Discovered via `skill_dirs()` (`src/skills/discovery.rs`) — cwd `.dex/skills`, `.agents/skills`, then `$XDG_CONFIG_HOME/dex/skills`. Sorted, first `name` wins, duplicates warned.
- Permissions default `trusted` (`read-only`/`ask`/`trusted`; deprecated `ask-writes`/`ask-shell` map to `ask`); `bash` is mutating. Native schema: `read`/`bash`/`write`/`edit`/`grep`/`find`/`ls` plus `delegate` (sub-agents, one tool with `action: spawn|wait|stop|list`, daemon sessions only).
- Compaction is deterministic by default (`DEX_COMPACTION=llm` for LLM, `=jev` for verbatim tool-output pruning). Keep `tokens > contextWindow - reserveTokens` logic intact.

## Release

- `scripts/release.sh [major|minor|patch|X.Y.Z]` — tags the release commit. `develop` is protected (no pushes, not even CI), so bump `version` in `Cargo.toml` + `cargo update -p dex` via a normal PR first, merge, pull, then re-run to tag its tip. A bump word applies to the last tag, so the same command aborts with instructions before the bump lands and tags right after. Working tree must be clean and the default branch up to date; the bump touches only `Cargo.toml`/`Cargo.lock` — the doctor snapshot test takes its version from `CARGO_PKG_VERSION`.

## Before submitting

Run the three checks above. Keep diffs minimal, reuse existing helpers, don't add scaffolding for later. Don't commit `docs/` working notes (plans, sweep ledgers, reviews) — agent scratch stays local; only user-facing docs (`README.md`) are committed.
