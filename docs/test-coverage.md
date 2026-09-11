# Test coverage — dex

Measured with [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov):

```sh
cargo llvm-cov --all-targets                 # run tests under coverage
cargo llvm-cov --all-targets --summary-only  # quick per-file table
```

Baseline (2026-09-11): **78.05% lines, 75.43% functions** over 515 tests
(514 unit + 1 integration), ~20s wall.

After the plan below: **82.76% lines, 80.01% functions** over 562 tests.

## Where we stand

The suite is healthy in *quality* — handlers, providers and pure formatting
logic have real behavior tests, error paths are asserted, and
`daemon/server.rs` already contains a full remote-loop e2e (real router + real
`DaemonClient` + a fake chat-completions provider). The gaps are concentrated:

| Area | Status |
|---|---|
| `protocol/`, `llm/provider.rs`, `core/lang.rs`, `ui/render.rs` | 96–100% — done |
| `agent/loop.rs`, `session.rs`, `llm/config.rs`, `tools/` | 85–93% — good |
| `daemon/server.rs` handler branches | 87% — hermetic handler tests in place |
| `ui/slash.rs` | 83% — dispatch + pure helpers tested |
| `client/repl.rs` | 61% — pure event→decision mapping extracted and tested |
| `client/http.rs` | 92% — daemon API client paths tested against a mock daemon |
| `mcp/oauth.rs` | 83% — PKCE/OAuth flows tested against a local mock |
| `main.rs`, TUI event loops | inherently hard (bootstrap, TTY); out of scope |

## Plan (fix one by one)

1. `daemon/server.rs` — hermetic handler tests for everything not yet covered:
   bearer auth, meta endpoints (`health`/`config`/`git`/`mcp`/`skills`),
   `load_skill` success path, `approve` AllowSession + audit write,
   `cancel` token paths, `recall` followup branch, `lookup_entry` disk
   fallback, `session_events` unknown-type cursor, `reattach`,
   `session_trace`, `session_undo`, `session_waive`, `session_name`.
2. `ui/slash.rs` — unit tests for command dispatch and pure helpers.
3. `client/repl.rs` — extract the pure event→decision mapping, test it.
4. `client/http.rs` — mock-daemon tests for the API client paths.
5. `mcp/oauth.rs` — token exchange against a local mock server.

Out of scope on purpose: `main.rs` bootstrap (conventionally low; covered by
`dex doctor` smoke flows), ratatui/crossterm event loops (need a TTY or heavy
mocking; their pure helpers are tested instead).

## Status

| # | Area | Lines before → after | Notes |
|---|---|---|---|
| — | baseline | 78.05% | this doc |
| 1 | `daemon/server.rs` | 78% → 86.92% lines / 72.67% funcs | bearer auth, meta endpoints, approve/cancel/recall, session_* handlers |
| 2 | `ui/slash.rs` | 32% → 82.78% lines / 76.47% funcs | command dispatch + pure helpers |
| 3 | `client/repl.rs` | 0% → 61.44% lines / 58.82% funcs | pure event→decision mapping extracted; stdio loop still out of scope |
| 4 | `client/http.rs` | 58% → 91.83% lines / 90.85% funcs | API client paths against a mock daemon |
| 5 | `mcp/oauth.rs` | 38% → 82.81% lines / 73.58% funcs | mock AS: register, exchange, refresh, discovery, probe, loopback callback |
