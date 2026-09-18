# dex `src/` structure refactor plan (local scratch — do not commit)

> Source: `origin/develop` @ `cc5b713`, worktree `../dex-structure-audit`.
> This is agent scratch per `AGENTS.md`. Do not commit `docs/` working notes; only `README.md` is user-facing.
> Goal: standard Rust layout + clean bounded contexts, minimal diffs, no behavior change.
>
> Rev 3 (naming + domain review): top-level bounded contexts KEPT (`agent/daemon/client/llm/tools/session/protocol/ui/mcp/extensions/skills` all correct). Fixes are dependency-direction leaks, not renames — see §8.
>
> Rev 2 (review amendments): incorporates module-by-module audit findings.
> Three corrections block the original Rev 1 as written — see §8.
> No behavior change, no new deps, prompt stays minimal, `tokens > contextWindow - reserveTokens` intact,
> `model:` single-knob + `dex doctor` origins stay in sync.

## 0. Why

* No `src/lib.rs` — everything binary-private, `pub(crate)` everywhere, no integration tests.
* Fat parents: `mcp.rs:1208` + `mcp/`, `ui.rs:2929` + `ui/`, `tools/mod.rs:2778`, `daemon/mod.rs:1204`, `extensions/mod.rs:2292`, `session/mod.rs:2469`. Parent should be facade only.
* God files: `llm/config.rs:7255`, `ui/render.rs:4615`, `ui/remote.rs:4023`, `daemon/server.rs:3579`, `agent/loop.rs:2606` (total ~76k LOC). Plus missed: `llm/sse.rs:2200`, `mcp/oauth.rs:1771`, `subagent/manager.rs:1834`, `subagent/tools.rs:1790`, `ui/slash.rs:1904`.
* `src/core/` grab-bag (`console:586,format:1955,fs:92,highlight:1390,logging:291,markdown:443,palette:70,types:624,unwind:36`) — no cohesion, classic `utils/` smell.
* Mixed depth in `agent/` (top files + single-file dirs `router/`, `experiments/`), mixed `foo.rs+foo/` vs `foo/mod.rs+foo/` conventions.
* Cryptic names: `tools/fff.rs:301`, `ui/herdr.rs:320`, `agent/obs_pack:1007`, `agent/evidence_reducer:1403`; singletons `update.rs:355/usage.rs:582/skills.rs:401` at top level.
* Dependency-direction leaks (domain review, 2026-09-18 — names right, wiring wrong): `tools/mod.rs:43-49` imports UP into `agent::{online_compaction,state}` + `core::{console,format,types}` (rule: `agent→tools`, never `tools→agent`); `daemon/mod.rs` imports `agent::subagent` + `core::console/types` instead of owning `daemon/state.rs`; `daemon/turn.rs` vs `agent/loop.rs` boundary undocumented (HTTP handler vs state machine); `llm/` mixes 3 domains flat (config/routing vs transport vs schema); `main.rs:40,54` owns `spend_summary/chat_options_from_args` (belong `telemetry/`+`client/`); `tools/sandbox.rs` (used by daemon/session/llm) trapped inside `tools/`; `ui/remote.rs` is a `client/`, not a view.

Non-goals: no behavior change, no new deps (`Cargo.toml` first, stdlib first), no prompt/tool-description moves (prompt stays minimal per `AGENTS.md`), keep `tokens > contextWindow - reserveTokens` compaction invariant, keep `model:` single-knob + `dex doctor` origins in sync.

## 1. Guiding rules (standard Rust)

* `snake_case` files/mods (already OK). Full words over abbrev except canonical `cli/ui/mcp/llm/sse/http`.
* One convention: `foo/mod.rs` + `foo/` for dirs with logic; `foo.rs` only if leaf. Never fat `foo.rs` + full `foo/`.
* `mod.rs` / `foo.rs` parent = `mod` decls + `pub(crate) use` re-exports only, ~20-60 lines. No structs/fns/business logic.
* Binary thin: `src/main.rs` = `parse_args + dispatch` only. All logic in `src/lib.rs` library. Enables `tests/*.rs` integration tests.
* Size budget: warn at >800 LOC/file, hard split at >1200 LOC. One bounded context per file.
* Reuse existing helpers (`resolve_workspace_path`, `lock_map`, `warn_once`); `cargo fmt -- --check`, `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings` green after every phase.
* Dependency rules (domain review): `agent→tools→workspace/runtime`, never reverse (`tools→agent` forbidden — move `format_plan_snapshot/parse_plan_progress/parse_plan_steps` + `wait_cancelled/CancellationSource` usage down to `tools/` or `runtime/`); `daemon::turn` = HTTP handler (auth, idempotency, SSE emit) vs `agent::turn_loop` = state machine (`process_turn/AgentRuntime`) — doc boundary at top of both files; `llm/{config,transport,schema}` subgrouping enforces config vs wire vs schema; `workspace/` + `runtime/` are leaf infra (depend on nothing except `protocol/` types); `ffgrep 'use crate::agent::' src/tools/` must be empty at end of Phase 3.

## 2. Current map (LOC, `wc -l`)

```
src/main.rs:771 cli.rs:497 skills.rs:401 update.rs:355 usage.rs:582
src/llm/config.rs:7255 sse:2200 client:615 anthropic:573 protocol:714 + auth:172/dispatch:269/http:296/learned:138/prompt:320/provider:259/thinking:62
src/ui.rs:2929 ui/render:4615 remote:4023 slash:1904 + status:496 theme:400 input:344 herdr:320 wrapping:239
src/tools/mod:2778 + read:446/edit:369/fff:301/meta:263/shell:259/write:146/sandbox:96
src/daemon/mod:1204 server:3579 turn:859 + approvals:159/auth:94/lookup:167/shell:163/wake:146
src/session/mod:2469 + changes:124/discovery:194/events:292
src/extensions/mod:2292 engine:1818 manager:748 global:680 + appendix:114/discovery:189/hooks:149/manifest:240/state:66
src/agent/loop:2606 compaction:1217 online_compaction:1193 evidence_reducer:1403 obs_pack:1007 router/mod:753+stem:119 experiments/mod:196 + state:249/tokens:199 + subagent/{manager:1834 tools:1790 definition:365/model:154/resume:72/exit:88}
src/core/format:1955 highlight:1390 console:586 types:624 markdown:443 logging:291 + fs:92 palette:70 unwind:36 mod:9
src/protocol/mod:557 src/client/http:1349+repl:445/sse:130/runtime:108 src/mcp.rs:1208 mcp/oauth:1771 transport:432 config:396 mapping:132 redact:78
```

Notes: `session/{discovery,events,changes}` already proves the facade pattern works. `extensions/` already has `manager/global/engine` — `mod.rs` duplicates them. `tools/` leaves are thin; ~60% of logic sits in `tools/mod.rs`.

## 3. Target tree (names = what it is, not where it came from)

```
src/
  main.rs            # thin: Args -> Mode -> run (no business logic)
  lib.rs             # NEW: all `mod` decls; `pub mod cli, protocol` for tests/smoke.rs, rest `pub(crate)`
  cli.rs             # keep: Args/Mode/parse_args/resolve_mode only; chat_options conversion -> client/options.rs (NOT http.rs)
  protocol/          # wire types only; do NOT bulk-absorb core/types.rs (budget — see §4 Phase 3)
  runtime/           # NEW (from core/): console.rs logging.rs unwind.rs + format_runtime.rs (headless text)
  workspace/         # NEW (from core/fs.rs + tools/sandbox.rs): xdg/cached_parse/FileCache + confinement (own error type first)
  skills/{mod,parse,discovery}.rs  # split now (401 LOC, 100 from budget); parse_skill/parse_frontmatter vs discover/skill_dirs
  telemetry/         # NEW: self_update.rs (from update.rs) + session_plot.rs (from usage.rs) — NOT usage.rs (collides with Usage)
  client/{mod,http,sse,repl,runtime,options}.rs  # NEW options.rs: ChatOptions + chat_options_from_args; http.rs already 1349
  daemon/{mod(state facade),state.rs(DaemonState/SessionEntry/PendingApproval),server/{mod,routes_sessions,routes_chat,routes_misc},turn,approvals,auth,lookup,shell,wake}.rs
  session/{mod(facade),store.rs,header.rs,changes.rs,events.rs,discovery.rs}
  agent/{mod,turn_loop.rs(from loop.rs),state.rs,budget/tokens.rs,compaction/{mod,deterministic.rs},router.rs(subsumes stem.rs),experiments/ (KEEP — see §8),subagent/{mod,manager/{mod,registry,lifecycle},tools/{mod,schema,exec},definition,model,resume,exit}}
  llm/{mod,config/{mod,selection,provider,catalog_index,catalog_query,ctx_index,cost,headers,permission,routing,prompt_source,extension_model,doctor},transport/{client,dispatch,http,sse/{mod,parser,turn},anthropic},schema/{protocol,provider,auth,learned,thinking},prompt.rs}
  tools/{mod(facade),error.rs,outcome.rs,policy.rs,dispatch.rs,audit.rs,registry.rs,output.rs,read.rs,edit.rs,write.rs,shell.rs,search.rs(from fff.rs),meta.rs}
  mcp/{mod(facade),config,mapping,oauth/{mod,flow,storage},redact,transport}.rs
  ui/{mod(facade),app.rs(App state),format.rs(presentation only),theme/{palette,highlight,markdown},views/{render_messages,render_sidebar,render_composer,status,slash/{parser,completion},input,wrapping},client.rs+stream.rs(from remote.rs),herdr.rs(KEEP NAME — see §8)}
  extensions/{mod(facade),engine,hooks,manifest,discovery,manager,state}
```

Rename table:

| now | to | why / status |
|---|---|---|
| `tools/fff.rs` | `tools/search.rs` | ✅ `fffind/ffgrep` are search; `fff` opaque. Keep engine ref in docs |
| `ui/herdr.rs` | KEEP (`presence.rs` only if renamed) | ❌ NOT `diff.rs` — file is Herdr pane lifecycle (`HERDR_ENV/HERDR_PANE_ID`, working/blocked/idle), not diff rendering |
| `agent/obs_pack/` | KEEP | ❌ NOT `compaction/pack.rs` — opt-in experiment, owns gate+schema+doctor row (see §8) |
| `agent/evidence_reducer/` | KEEP | ❌ NOT `compaction/evidence.rs` — same seam |
| `agent/online_compaction/` | KEEP | ❌ NOT `compaction/online.rs` — same seam |
| `agent/loop.rs` | `agent/turn_loop.rs` | ✅ kills `r#loop`; document vs `daemon::turn` (HTTP handler) vs `agent::turn_loop` (state machine) |
| `core/types.rs` | SPLIT, not 1:1 | ⚠️ wire (`ChatMessage/ToolDefinition/Skill`) → `protocol/`; domain (`Plan/PermissionMode`) → stay domain (`agent/`/`tools/`). Bulk move blows 557+624=1181 budget |
| `core/format.rs` | SPLIT | ⚠️ presentation → `ui/format.rs`; headless (`bash_context_text`, lifecycle text used by `main.rs`/daemon) → `runtime/format_runtime.rs` |
| `core/{highlight,markdown,palette}.rs` | `ui/theme/*` | ✅ presentation only |
| `core/{console,logging,unwind}.rs` | `runtime/*` | ✅ (`CancellationToken` flows daemon+agent, fine) |
| `core/fs.rs` + `tools/sandbox.rs` | `workspace/` | ⚠️ invert error first: `workspace` returns `std::io::Error`/`WorkspaceError`, `tools` maps. Else `workspace → tools::ToolError` cycles. Do BEFORE `llm/config` split |
| `mcp.rs` | `mcp/mod.rs` | ✅ cosmetic uniformity |
| `ui.rs` | `ui/mod.rs` | ✅ thin facade |
| `update.rs`+`usage.rs` | `telemetry/{self_update,session_plot}.rs` | ⚠️ NOT `{update,usage}` — `usage` collides with `core::types::Usage`/`StreamEvent::Usage` |
| `agent/router/stem.rs` | fold into `router.rs` | ✅ 753+119=872, under budget. No single-child dir |
| `agent/experiments/mod.rs` | KEEP dir | ❌ NOT leaf `experiments.rs` — registry will grow |
| `llm/sse.rs:2200` | `llm/transport/sse/{parser,turn}.rs` | ✅ missed god-file (`Turn` + streaming parsers) |
| `mcp/oauth.rs:1771` | `mcp/oauth/{flow,storage}.rs` | ✅ missed god-file |
| `ui/slash.rs:1904` | `ui/views/slash/{parser,completion}.rs` | ✅ missed god-file |
| `ui/remote.rs:4023` | `ui/{client,stream}.rs` | ✅ daemon TUI client split |
| `subagent/tools.rs:1790` | `subagent/tools/{schema,exec}.rs` | ✅ missed god-file |
| `subagent/manager.rs:1834` | `subagent/manager/{registry,lifecycle}.rs` | ✅ missed god-file |
| `llm/` grouping | `llm/{transport,schema}` | ✅ NOT `wire/` — vague. `prompt.rs` stays minimal |

Uniform rule going forward: dir => `foo/mod.rs` facade; never `foo.rs` + `foo/` with logic in both.

## 4. Phases (each = edit + `cargo fmt && cargo build && cargo test --all-targets && cargo clippy --all-targets -- -D warnings`)

### Phase 0 — `lib.rs` + thin `main.rs` + `workspace` error inversion (enablers, no moves)

* Add `src/lib.rs` with all current `mod` decls copied from `main.rs`. `pub mod cli, protocol` only (so `tests/smoke.rs` importing `dex::cli` compiles); everything else `pub(crate)`.
* `src/main.rs` keeps `fn main()` + dispatch; move `chat_options_from_args` -> NEW `client/options.rs` (NOT `client/http.rs` — already 1349 LOC, over budget), `spend_summary` (+`COST_SUMMARY_MIN_USD`) -> `telemetry/session_plot.rs`, `cli_system_prompt` stays wired to `llm::config::resolve_cli_system_prompt`.
* Invert `tools/sandbox.rs` error type FIRST: `workspace::{resolve_workspace_path,workspace_root}` return `std::io::Error`/`WorkspaceError`; `tools` maps to `ToolError`. Unblocks Phase 3 without a cycle.
* Record `tools→agent` inversion debt (fixed in Phase 3): `tools/mod.rs:43-49` uses `agent::online_compaction::{format_plan_snapshot,parse_plan_progress,parse_plan_steps}` + `agent::state::{wait_cancelled,CancellationSource}` — new `workspace/`+`runtime/` must NOT import `agent/` or `tools/`; the plan helpers move down to `tools/` or `runtime/format_runtime.rs` in Phase 3 so `agent→tools→workspace` holds.
* Add one smoke `tests/smoke.rs` importing `dex::cli` to lock lib surface.
* Verify: `cargo build --release`, existing tests pass. Diff: +~50 lines.

### Phase 1 — thin facades (no logic moves, only extract decls)

For each in order (separate commits, easy revert): `tools/mod.rs`, `daemon/mod.rs`, `session/mod.rs`, `extensions/mod.rs`, `mcp.rs`, `ui.rs`.

* Pattern: parent keeps `mod x; pub(crate) use x::{...};`, delete inlined structs/fns (move verbatim to child, no refactor).
* `tools/mod.rs:2778` -> `tools/{error,outcome,policy,dispatch,audit}.rs` + `registry.rs` (`metadata_native`) + `output.rs` (clamp). Leaves (`read/edit/fff/meta/shell/write/sandbox`) stay.
* `daemon/mod.rs:1204` -> `daemon/state.rs` (`DaemonState/SessionEntry/PendingApproval/lock_map`); `server.rs` untouched this phase.
* `session/mod.rs:2469` -> `session/store.rs` + `session/header.rs` (`SessionHeader/FileId/PathCache`); `discovery/events/changes` already split, don't touch.
* `extensions/mod.rs:2292`: merge split `mod` blocks (lines 9-11 + 31-35) into one block, push `tools_list/set_active_global/call_shadow/install/remove/discovered_*` down into `manager.rs`/`global.rs`; standardize `pub(crate)` like `daemon/mod.rs`.
* `mcp.rs:1208` -> `mcp/mod.rs` facade (cosmetic); real split is `oauth.rs` in Phase 2.
* `ui.rs:2929` -> `ui/mod.rs` facade + NEW `ui/app.rs` (`App` state machine) FIRST, before any `render/` split.
* Verify per-file: `wc -l src/<facade>` <100, build+clippy green.

### Phase 2 — kill god-files (biggest payoff)

* `llm/config.rs:7255` -> `llm/config/{mod,selection,provider,catalog_index,catalog_query,ctx_index,cost,headers,permission,routing,prompt_source,extension_model,doctor}.rs`. Keep in `mod.rs`: `LlmConfig`, `Resolved<T>`, `load_config_file` unknown-key list (~line 136), `warn_once`. Move verbatim first (`env_vars`, `parse_headers_str`/`merge_header_layers`, `catalog_*` ~992-1230, `route_turn/routing_resolution`, `usage_cost`, `doctor` ~3240-3746 + `LlmConfig impl` ~2709+531). Keep `dex doctor` origin rows + `from_env` in sync per `AGENTS.md`; update unknown-key list if keys move. No new knobs without `warn_once` + doctor row + README env table. AFTER `workspace/` extraction (`config.rs` re-exports `core::fs`).
* `daemon/server.rs:3579` -> `daemon/server/{mod,routes_sessions,routes_chat,routes_misc}.rs` by resource. `router()` stays in `mod`. Accept residual coupling (`session/skills/mcp` imports) — goal is file size, not decoupling.
* `ui/render:4615` -> `ui/views/render_{messages,sidebar,composer}.rs`; `ui/remote:4023` -> `ui/{client,stream}.rs`; `ui/slash:1904` -> `ui/views/slash/{parser,completion}.rs`. Snapshot/golden tests FIRST; no visual change. `App` state already in `app.rs` (Phase 1).
* `llm/sse.rs:2200` -> `llm/transport/sse/{parser,turn}.rs`.
* `mcp/oauth.rs:1771` -> `mcp/oauth/{flow,storage}.rs`.
* `subagent/manager:1834` -> `subagent/manager/{registry,lifecycle}.rs`; `subagent/tools:1790` -> `subagent/tools/{schema,exec}.rs`.
* `agent/loop.rs:2606` -> `agent/turn_loop.rs` (+ `agent/turn/{dispatch,events}.rs` only if still >1200). Keep `process_turn/AgentRuntime` paths via re-export so `crate::agent::r#loop` call sites don't churn. Add one-line boundary docs: `daemon/turn.rs` = HTTP handler, `agent/turn_loop.rs` = state machine — no logic move, just the doc + rename.
* `session/mod.rs remainder` -> `session/store.rs` (`Session/load_*`).
* Each sub-step must keep public paths working via `pub(crate) use` so call sites don't churn.

### Phase 3 — dissolve `core/` (split, NOT wholesale moves)

* `types:624` — SPLIT: wire (`ChatMessage/ToolDefinition/Skill`) -> `protocol/` WITH simultaneous `protocol/{sessions,stream,tools}.rs` split (else 557+624=1181 blows budget); domain (`Plan/PermissionMode`) -> `agent/`/`tools/`, not protocol.
* `format:1955` — SPLIT: pure presentation -> `ui/format.rs`; `bash_context_text` (used by `main.rs`+daemon) + `agent_lifecycle`/`mcp_status_line` headless text -> `runtime/format_runtime.rs`. Wholesale `format->ui/format` breaks daemon/main with a `ui` dep.
* `highlight:1390/markdown:443/palette:70` -> `ui/theme/` ✅. `console:586/logging:291/unwind:36` -> `runtime/` ✅. `fs:92` -> `workspace/` ✅ (already unblocked by Phase 0 inversion).
* Break `tools→agent` upward dep (domain fix, same phase): move `format_plan_snapshot/parse_plan_progress/parse_plan_steps` out of `agent::online_compaction` into `tools/` (or `runtime/format_runtime.rs` if headless-only) and replace `agent::state::{wait_cancelled,CancellationSource}` use in `tools/` with `runtime::{wait_cancelled,CancellationSource}` (moved with `console.rs`). After this `ffgrep 'use crate::agent::' src/tools/` is empty; direction is `agent→tools→workspace/runtime`.
* Delete `src/core/` when empty. Update `use crate::core::...` via compiler-driven rewrites (one commit per target dir).
* This removes the `utils` smell and answers "where does X live?" by bounded context.

### Phase 4 — `agent/` + `llm/` depth normalization (KEEP experiments seam)

* DO NOT merge `online_compaction/evidence_reducer/obs_pack` into `compaction/`. `src/agent/experiments/mod.rs` is an explicit seam: each experiment owns its gate + tool schemas + doctor row; host files (`loop.rs`, `protocol.rs`, `config.rs`) never name an env var. They are gated by `DEX_ONLINE_COMPACTION=1`/`DEX_EVIDENCE_REDUCER=1` with own doctor rows. Folding destroys the seam. Keep `experiments/` as-is.
* Allowed: `compaction.rs:1217` -> `compaction/{mod,deterministic.rs}` ONLY if it clarifies deterministic (`DEX_COMPACTION=llm|jev`) vs experimental compaction. Keep `tokens > contextWindow - reserveTokens` logic intact.
* `agent/router/`: inline `stem.rs:119` into single `router.rs` (872 combined, <800 warn but <1200 hard — acceptable, no single-child dir).
* `experiments/`: KEEP dir (single `mod.rs:196` today, will grow). Do NOT flatten to leaf.
* `llm/` flat 12 -> `llm/{config,transport,schema}/` groups above; `prompt.rs:320` stays minimal (tool behavior lives in `protocol.rs` descriptions per `AGENTS.md`).

### Phase 5 — renames + top-level tidy

* `fff->search` ✅, `loop->turn_loop` ✅ (with `daemon::turn` vs `agent::turn_loop` doc), `mcp.rs->mcp/mod.rs` ✅, `ui.rs->ui/mod.rs` ✅.
* `herdr` KEEP (or `presence.rs` at most) — see §8. Verify with `head`, don't assume.
* `update/usage->telemetry/{self_update,session_plot}` ✅ (NOT `{update,usage}`).
* `protocol/mod.rs:557` stays leaf until >800 LOC, then `protocol/{sessions,stream,tools}.rs` — EXCEPT when `core/types` wire lands (split then, see Phase 3).
* `skills.rs:401` split NOW to `skills/{mod,parse,discovery}.rs` (100 LOC from budget; `parse_skill/parse_frontmatter` vs `discover_*/skill_dirs`) — don't wait for 500.
* Run `cargo fmt`, `ffgrep` for stale `crate::core::` / `crate::mcp::` paths, fix `AGENTS.md` structure list + `README.md` § Configuration if resolution/origins changed.

## 5. Acceptance

* `cargo build --release`, `cargo fmt -- --check`, `cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings` all green on final tip (same as `AGENTS.md` Before submitting).
* No file >1200 LOC except transient; no facade >100 LOC. `llm/config/*` shards each <1200, `LlmConfig`+`Resolved`+unknown-key list stay in `config/mod.rs`.
* No `src/core/`, no `foo.rs`+`foo/` dual-logic, no `allow(dead_code)` at module root to hide dead facades.
* `git diff --stat` shows moves + re-exports, no behavior diff (compaction gates `DEX_COMPACTION`/`DEX_ONLINE_COMPACTION`/`DEX_EVIDENCE_REDUCER`, permissions default `trusted`, `DEX_EXTRA_TOOLS`, sessions journal markers, `tokens > contextWindow - reserveTokens` intact).
* Dependency direction holds: `ffgrep 'use crate::agent::' src/tools/` empty, `ffgrep 'use crate::tools::' src/workspace/ src/runtime/` empty, `ffgrep 'crate::core::' src/` empty. Top-level bounded contexts unchanged (`agent/daemon/client/llm/tools/session/protocol/ui/mcp/extensions/skills` kept — only `core/` dissolved into `runtime/workspace/ui/*` + `telemetry/`).
* `dex doctor` output unchanged (origins/values) unless intentionally migrated with `warn_once` + README update. `config_doctor_snapshot` green; hermetic `XDG_CACHE_HOME`/`DEX_CONFIG` + `EnvRestore` + `TEST_SESSIONS_ENV_LOCK` honored.

## 6. Risks / guards

* Import churn: mitigate with `pub(crate) use` shims per phase, remove after green.
* `llm/config.rs` split risks `doctor`/`from_env` drift — keep single test `config_doctor_snapshot` green; ORDER: `workspace/` (Phase 0) BEFORE `config` split (Phase 2) since `config.rs` re-exports `core::fs`.
* `workspace/` split risks `ToolError` cycle — inverted in Phase 0 (`WorkspaceError`/`std::io::Error`), `tools` maps.
* `tools→agent` inversion risks behavior drift in plan-snapshot rendering — move verbatim first, keep `agent` re-exporting from new home until Phase 4, remove shim after green.
* `format`/`types` splits risk wrong-home regressions — split by consumer (headless vs presentation, wire vs domain), not wholesale; `bash_context_text` has daemon/main consumers, `Plan/PermissionMode` are domain.
* UI split risks render regressions — snapshot/golden tests first, `App` state extracted before views, no visual change in this plan.
* `lib.rs` visibility: `pub(crate)` mods aren't importable from `tests/` — expose `pub mod cli, protocol`, rest `pub(crate)`.
* Worktree hygiene: this plan lives only in `../dex-structure-audit`; main checkout stays clean. Promote via normal PR from feature branch, never push `develop` directly; bump `version` in `Cargo.toml` via PR before `scripts/release.sh` tagging.

## 7. Suggested commit stack (one PR per line, stackable)

1. `refactor: add lib.rs, thin main.rs, invert workspace error`
2. `refactor: thin tools/daemon/session facades`
3. `refactor: thin mcp/ui/extensions facades + ui/app`
4. `refactor: split llm/config + llm/sse`
5. `refactor: split daemon/server + mcp/oauth + subagent/*`
6. `refactor: split ui/render/remote/slash`
7. `refactor: dissolve core/ (split format/types, move theme/runtime/workspace)`
8. `refactor: normalize agent/router, skills/, rename fff/loop, telemetry/`

## 8. Review amendments (Rev 1 -> Rev 2 -> Rev 3, why)

### Rev 2 -> Rev 3 (naming + domain review 2026-09-18)
Top-level domain names VERIFIED correct — `agent/daemon/client/llm/tools/session/protocol/ui/mcp/extensions/skills` all stay. Uniform depth REJECTED: `protocol/mod:557` + `cli.rs:497` stay leaves until >800 LOC; depth follows bounded context + size budget, not symmetry. Leaks fixed by direction, not renames:
11. `tools→agent` upward dep (`tools/mod:43-49` → `agent::online_compaction/state`): move plan helpers + cancel primitives DOWN (`tools/` or `runtime/`), enforce `agent→tools→workspace/runtime` with `ffgrep` guard in acceptance.
12. `daemon::turn` vs `agent::turn_loop` boundary: doc-only (`daemon/turn` = HTTP handler, `agent/turn_loop` = state machine), no logic move.
13. `llm/` flat = 3 domains: subgroup `llm/{config,transport,schema}` (config/routing vs wire vs schema), no top-level rename.
14. `main.rs` app logic: `spend_summary→telemetry/session_plot`, `chat_options_from_args→client/options` (already in Phase 0 — confirmed as domain fix, not just thinning).
15. `tools/sandbox + core/fs → workspace/` + `update/usage → telemetry/` + `ui/remote → ui/{client,stream}` confirmed as misplaced-infra moves (Phase 0/2/5).

### Rev 1 -> Rev 2

1. `ui/herdr.rs -> ui/diff.rs` BLOCKED: `herdr.rs:320` is Herdr pane lifecycle reporting (`HERDR_ENV`, working/blocked/idle), not diff rendering. Rename to `diff.rs` mislabels the bounded context. Keep `herdr.rs` or `presence.rs` at most.
2. `agent/compaction/` merge BLOCKED: `experiments/mod.rs:196` documents the seam — every experiment owns gate+schemas+doctor row; hosts never name env vars. `obs_pack`/`evidence_reducer`/`online_compaction` are opt-in experiments (`DEX_ONLINE_COMPACTION=1`, `DEX_EVIDENCE_REDUCER=1`), not deterministic compaction (`DEX_COMPACTION=llm|jev`). Folding them into `compaction/` breaks gates + doctor rows.
3. `workspace/` extraction BLOCKED as written: `tools/sandbox.rs:96` returns `ToolError`, so `workspace -> tools` import cycles. Invert error type first (Phase 0), then move.
4. `client/http.rs` is already 1349 LOC — `chat_options_from_args` must go to NEW `client/options.rs`, not `http.rs`.
5. `telemetry/{update,usage}` renamed to `{self_update,session_plot}`: `usage.rs:582` is the session-chart plotter, not spend accounting; `usage` collides with `Usage`/`StreamEvent::Usage` in search.
6. `core/format` + `core/types` are NOT wholesale moves: `format` has headless consumers (`main.rs`, daemon), `types` is 624 LOC of mixed wire+domain that overflows `protocol/mod:557`. Split both by consumer.
7. `llm/config` 6-file split was too coarse for 7255 LOC: `catalog_index` (~992-1230) + `doctor` (~3240-3746) + `LlmConfig impl` (~2709+531) each deserve own shard; `Resolved<T>` + unknown-key list + `warn_once` stay in `mod.rs`.
8. Missed god-files added: `llm/sse:2200`, `mcp/oauth:1771`, `subagent/manager:1834`, `subagent/tools:1790`, `ui/slash:1904`. Phase 2 covers each.
9. `skills.rs:401` split now, not at 500 — 100 LOC from budget with clean `parse` vs `discovery` seam.
10. Phase order fixed: `workspace` inversion (Phase 0) before `llm/config` split (Phase 2), since `config.rs` re-exports `core::fs`.
