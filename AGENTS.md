# AGENTS.md — dex

Terminal coding agent in Rust (`dex`): OpenAI-compatible Chat Completions / Responses, tool use (`read`/`bash`/`write`/`edit`/`ffgrep`/`fffind`), ratatui TUI, client↔daemon over HTTP+SSE, JSONL sessions.

## Build & check

```sh
cargo build --release          # -> target/release/dex
cargo fmt -- --check           # must pass
cargo test --all-targets       # must pass
cargo clippy --all-targets -- -D warnings  # must pass
```

Requires Rust edition 2021. Config: `~/.config/dex/config.yaml` (or `$DEX_CONFIG`) as defaults; env (`DEX_*`/`OPENAI_*`) and CLI flags override — see `README.md`.

## Structure

- `src/main.rs` — entry, mode resolution, daemon bootstrap
- `src/cli.rs` — arg parsing / `Mode` (`Default`/`Serve`/`Connect`/`OneShot`/`Tool`/`RunTool`/`Update`)
- `src/daemon/` + `src/protocol/` + `src/client/` — daemon (axum + SSE), wire types, client
- `src/agent/` — `loop.rs` turn loop, `state.rs`, `compaction.rs`
- `src/llm/` — provider clients, streaming parsers, `prompt.rs` (system prompt)
- `src/tools/` — workspace-confined tools (`mod.rs`, `fff.rs` for fff engine)
- `src/session.rs` — append-only JSONL (`$XDG_DATA_HOME/dex/sessions/<slug>/*.jsonl`)
- `src/skills.rs` / `src/ui/` / `src/core/` — skills discovery, TUI, formatting
- This file + `CLAUDE.md` (if present, nearest parent wins) is auto-appended to the system prompt via `src/llm/prompt.rs:project_context()`.

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

- `scripts/release.sh [major|minor|patch|X.Y.Z]` — cuts a release; the only manual step is this tag+push. Never bump `Cargo.toml` yourself: CI does the version bump on the default branch after the tag lands. Working tree must be clean and the default branch up to date.

## Before submitting

Run the three checks above. Keep diffs minimal, reuse existing helpers, don't add scaffolding for later.
