# Contributing to dex

Thanks for your interest in improving `dex`. This file has the fast facts;
[`AGENTS.md`](AGENTS.md) documents the architecture and working conventions
in depth (it is the same file the agent itself receives as part of its system
prompt).

## Build and check

```sh
cargo build --release          # -> target/release/dex
cargo fmt -- --check           # must pass
cargo test --all-targets       # must pass (network-dependent tests sit behind --features network-tests)
cargo clippy --all-targets -- -D warnings  # must pass
```

Requires a stable Rust toolchain (edition 2021).

## Pull requests

- Target the default branch (`develop`).
- Keep diffs minimal; reuse existing helpers; no scaffolding "for later".
- Run the three checks above. CI runs the same, plus a Windows build gate and
  `cargo audit`.
- Conventional-commit style subjects (`fix:`, `feat:`, `chore:`, ...) are
  preferred.
- New dependencies need a clear reason — check `Cargo.toml` first and prefer
  stdlib.

## Code conventions

- Tool behavior belongs in the tool descriptions (`src/tools/`, wire types),
  not in the system prompt; `src/llm/prompt.rs` stays minimal.
- Tools are workspace-confined through `resolve_workspace_path` — never add a
  code path that can escape it.
- Sessions journal incrementally with `turn_start` / `turn_complete` /
  `turn_failed` markers; keep the crash-safe guarantees intact (a crash may
  lose at most the in-flight event).
- Compaction stays deterministic by default; keep the
  `tokens > contextWindow - reserveTokens` trigger logic intact.

## Reporting bugs / security issues

- Bugs: open a GitHub issue with the bug template (`dex --version`, config
  with API keys redacted, steps to reproduce).
- Security: see [SECURITY.md](SECURITY.md) — use private vulnerability
  reporting, not public issues.

## License

By contributing, you agree that your contributions are dual-licensed under
the MIT and Apache-2.0 licenses, the same as the project (see
[LICENSE](LICENSE) and [LICENSE-APACHE](LICENSE-APACHE)).
