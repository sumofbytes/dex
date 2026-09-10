# AGENTS.md — dex

Terminal coding agent in Rust (`dex`): OpenAI-compatible Chat Completions / Responses, tool use (`read`/`bash`/`write`/`edit`/`ffgrep`/`fffind`), ratatui TUI, client↔daemon over HTTP+SSE, JSONL sessions.

## Build & check

```sh
cargo build --release          # -> target/release/dex
cargo fmt -- --check           # must pass
cargo test --all-targets       # must pass
cargo clippy --all-targets -- -D warnings  # must pass
```

Requires Rust edition 2021. Config: `$XDG_CONFIG_HOME/dex/config.yaml` (or `$DEX_CONFIG`) as defaults; CLI flags > env (`DEX_*`) > file > built-in default — see `README.md` § Configuration and `dex doctor`.

## Structure

- `src/main.rs` — entry, mode resolution, daemon bootstrap
- `src/cli.rs` — arg parsing / `Mode` (`Default`/`Serve`/`Connect`/`OneShot`/`Tool`/`RunTool`/`Doctor`/`Update`/`Mcp`/`Help`/`Version`)
- `src/daemon/` + `src/protocol/` + `src/client/` — daemon (axum + SSE), wire types, client
- `src/agent/` — `loop.rs` turn loop, `state.rs`, `compaction.rs`
- `src/llm/` — provider clients, streaming parsers, `config.rs` (provider/model/endpoint resolution, `/model` write-back, `dex doctor`), `prompt.rs` (system prompt)
- `src/tools/` — workspace-confined tools (`mod.rs`, `fff.rs` for fff engine)
- `src/mcp.rs` + `src/mcp/` — MCP client (stdio/HTTP/SSE, OAuth, `mcp_servers:` config)
- `src/session.rs` — append-only JSONL (`$XDG_DATA_HOME/dex/sessions/<slug>/*.jsonl`)
- `src/skills.rs` / `src/ui.rs` + `src/ui/` / `src/core/` — skills discovery, TUI, formatting
- This file + `CLAUDE.md` (if present, nearest parent wins) is auto-appended to the system prompt via `src/llm/prompt.rs:project_context()`.

## Config surface

All of this lives in `src/llm/config.rs` — don't add a second way to express any of it:

- One selection knob: `model: <provider|endpoint>/<model>` (file `model:`, env `DEX_MODEL`, flag `--model`; bare provider name switches provider and keeps the model). `/model`/`/provider` write back that single key and drop the deprecated ones — never write back `active_provider:`/`provider:`/top-level `base_url:`.
- Endpoints, model lists, pricing, context windows come from the models.dev catalog (cache via `dex update --models`); wire protocol is learned per endpoint+model (`learned-apis.json`), with `providers.<name>.api:` as the pin.
- Per-provider keys live in `providers.<name>.api_key` or the provider's own catalog env var (opencode: `OPENCODE_API_KEY`). Never add per-provider default key env vars.
- Extra headers precedence: file (provider-scoped `headers:` > global, per-key) < env (`ANTHROPIC_CUSTOM_HEADERS` < `OPENAI_HEADERS` < `DEX_HEADERS`) < `--header`; `authorization` can't be overridden.
- Deprecated keys/env stay honored with `warn_once` + a pointer at the replacement; new knobs must do the same, add themselves to the unknown-key list in `load_config_file`, add a `dex doctor` origin row, and appear in the README env table.
- `dex doctor` prints every resolved value with its origin — keep it in sync with `from_env` when resolution changes. Config tests use `EnvRestore` + `TEST_SESSIONS_ENV_LOCK` and hermetic `XDG_CACHE_HOME`/`DEX_CONFIG` paths.

## Working rules

- Batch independent `read`/`ffgrep`/`fffind` into one parallel call; don't do one file per turn.
- `read` before `edit`; `edit` needs exact `oldText` (unique match, or `replaceAll: true`). `write` overwrites.
- Tools are workspace-confined (`resolve_workspace_path`); symlinks can't escape. Verify with `bash` + tests, show file paths clearly.
- `write`/`edit` on distinct `path` run in parallel; same `path` or any `bash` serializes. `read`/`ffgrep`/`fffind` are read-only/idempotent.
- Tool output is truncated for the model: bash ~400 lines/32 KiB, read 2000 lines/256 KiB, fan-out caps 10 files. Use `$DEX_BIN run <tool>` inside `bash` to stitch pipelines without flooding context.
- Sessions journal incrementally; `turn_start`/`turn_complete`/`turn_failed` markers — crash loses at most the in-flight event.

## Conventions

- No new dependencies without clear need — check `Cargo.toml` first, prefer stdlib/native.
- Keep `src/llm/prompt.rs` minimal; tool behavior belongs in `src/llm/protocol.rs` tool descriptions, not the prompt.
- Skills: directory with `SKILL.md` frontmatter (`name`, `description`). Discovered via `skill_dirs()` — cwd `.dex/skills`, `.agents/skills`, then `$XDG_CONFIG_HOME/dex/skills`. Sorted, first `name` wins, duplicates warned.
- Permissions default `trusted` (`read-only`/`ask-writes`/`ask-shell`/`trusted`); `bash` is mutating. `DEX_EXTRA_TOOLS=1` adds `git`/`chain`.
- Compaction is deterministic by default (`DEX_COMPACTION_LLM=1` for LLM). Keep `tokens > contextWindow - reserveTokens` logic intact.

## Release

- `scripts/release.sh [major|minor|patch|X.Y.Z]` — tags the release commit. `develop` is protected (no pushes, not even CI), so bump `version` in `Cargo.toml` + `cargo update -p dex` via a normal PR first, merge, pull, then tag its tip. Working tree must be clean and the default branch up to date.

## Before submitting

Run the three checks above. Keep diffs minimal, reuse existing helpers, don't add scaffolding for later.
