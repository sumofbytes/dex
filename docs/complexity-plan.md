# Plan — complexity reduction sweep (rev 2, post-review)

Executes the ledger in [complexity-issues.md](complexity-issues.md) (rationale in
[complexity-review.md](complexity-review.md)). 46 issues → 19 PRs in 8 phases.

**Rev 2 incorporates the adversarial review of rev 1:**
- AGT-3 re-specified (derive(Clone) cannot compile — `ProjectionState` holds
  `std::sync::Mutex` fields; verified at obs_pack.rs:328-339) and moved out of PR-2.
- Four silently dropped ledger items restored: AGT-6, CLI-2, MCP-9, MCP-10.
- PR numbering made single-scheme; dependency graph consistent with it.
- Behavior-change and test-edit rules made self-consistent (see exemptions).
- Measurement demoted to one-shot informational; abort triggers made concrete.

## Goals

1. Remove accidental complexity: ~900–1200 LOC and several hundred branch sites, no
   capability or wire change.
2. Fix the two latent MCP bugs (`BUG-1`, `BUG-2`) and land them *first*, unbundled.
3. Single-source the invariants most likely to drift: journal rewrite, tool-name
   grammar, `from_env`/`doctor` precedence, mutex poison-recovery.

## Non-goals

- No new dependencies, no public API changes, no wire-format changes.
- Nothing from the do-not-touch list (complexity-issues.md § Known non-issues).
- Skipped by default: CLI-4, LLM-5 (pull in only if the Phase 8 metrics gate demands).

## Working rules (every PR)

- Three checks green before opening: `cargo fmt -- --check`, `cargo test
  --all-targets`, `cargo clippy --all-targets -- -D warnings`.
- PR body lists the issue IDs it closes. Diffs are pure code motion unless stated.
- **Behavior changes:** none, except the six declared exemptions — BUG-1, BUG-2,
  SES-2 (Interrupted-retry strictness, strict improvement), CLI-5 (async-path warning
  parity with sync path), SRV-6 (tie-order determinism), SRV-8 (error-chain
  preservation in client coercions). Each exemption is named in
  its PR body with a one-line justification and its own test where feasible.
- **Test edits:** existing assertions pass unmodified, with exactly two exceptions:
  PR-11's reworked AGT-3 (mechanical, no assertion involved) and PR-17 (stream.rs
  may convert its *test-only* sync drivers to async entry points — test-only files,
  additive cases allowed anywhere). Additive tests are always allowed.
- **Abort triggers (concrete, per PR):** the PR would need an *unflagged* existing
  assertion edit; effort exceeds 2× estimate; a do-not-touch item starts looking
  tempting; or clippy/tests fail for reasons not obviously attributable to the motion.
  Park the PR, record the reason in the ledger, move on.
- Each PR gets a reviewer subagent pass before merge; PRs are small enough that
  revert is the rollback strategy: every PR is one revertable unit (revert-clean
  commit series); PR-12 lands as stacked individually-green move-verbatim commits;
  the config PRs (PR-16/18/19) are single revertable commits with their named gates
  run before merge. The doctor output gate for PR-18: the existing doctor tests
  (config.rs:4692+) are the baseline; if coverage of exact output is insufficient,
  add an additive byte-for-byte snapshot test in PR-18 *before* any motion.

## Sequencing rationale

- `config.rs` (LLM-1,2 → LLM-3/4/10 → LLM-8 → LLM-9) strictly ordered; LLM-6/7 is a
  different file (`stream.rs`) and runs parallel anywhere after Phase 2.
- `loop.rs` (AGT-1,2 in PR-2 → AGT-3/4/7 in PR-11 → AGT-5 in PR-12);
  `compaction.rs` (AGT-6) follows PR-12.
- `mcp.rs`: bug fixes (PR-1) → dedup (PR-14); `oauth.rs` independent (PR-15).
- `daemon/server.rs`: lock boilerplate (PR-3) → handler dedup (PR-4) → turn pipeline
  (PR-5).
- PR-6/PR-7 both touch `tools/mod.rs`-adjacent display code — CLI-1/CLI-3 (PR-6)
  before CLI-2 (PR-7) to keep diffs reviewable.
- Free-floating after Phase 2: PR-6, PR-7, PR-8, PR-9, PR-10, PR-14, PR-15, PR-17.

## Baseline & measurement (Phase 0, before PR-1)

One-shot, informational (not a per-PR gate — the grep proxy is too crude for that;
abort triggers above are the enforcement):

```sh
for f in src/llm/config.rs src/llm/stream.rs src/daemon/server.rs \
         src/agent/loop.rs src/agent/compaction.rs src/session.rs \
         src/core/format.rs src/tools/mod.rs src/mcp.rs src/mcp/oauth.rs \
         src/client/http.rs; do
  printf '%-28s %5s %6s\n' "$f" \
    "$(grep -cE 'match |if |else if|unwrap_or_else' $f)" "$(wc -l < $f)"
done | tee /tmp/complexity-baseline.txt
```

Record into the table below at Phase 0 (done — see table); re-run once at Phase 8
(closure) and fill the final column. Targets are **net** (deletions minus added
helper/struct/test lines), re-derived per-PR at execution; overall goal 900–1200 net
LOC (production code; PR-17's ~250 deleted driver lines are test code, counted
separately). Do not compare against stale review-era estimates.

| File | Branch sites (measured) | LOC (measured) | Post-sweep |
|---|---|---|---|
| src/llm/config.rs | 303 | 4886 | |
| src/llm/stream.rs | 127 | 2249 | |
| src/daemon/server.rs | 193 | 4425 | |
| src/daemon/mod.rs | 61 | 1219 | |
| src/agent/loop.rs | 77 | 2058 | |
| src/agent/compaction.rs | 90 | 1091 | |
| src/agent/state.rs | 26 | 254 | |
| src/session.rs | 94 | 1804 | |
| src/core/format.rs | 158 | 1953 | |
| src/tools/mod.rs | 139 | 3466 | |
| src/mcp.rs | 105 | 1943 | |
| src/mcp/oauth.rs | 70 | 1715 | |
| src/client/http.rs | 33 | 1411 | |
| src/main.rs | 55 | 565 | |
| src/cli.rs | 29 | 450 | |
| src/skills.rs | 29 | 428 | |

(Grep proxy counts matches inside comments/strings too — acceptable as a consistent
before/after proxy, not a cyclomatic number.)

## Phases & PRs

### Phase 0 — baseline (no code)
Run the metric once, fill the table. Done when recorded.

### Phase 1 — bug fixes first

**PR-1 · BUG-1, BUG-2** — risk Low, effort S
- `redact_line` (mcp.rs:431): `to_lowercase` → `to_ascii_lowercase` (byte-offset
  desync; markers are ASCII, identical behavior on ASCII input).
- `expand_env` (mcp.rs:142): iterate `char_indices`, not `bytes[i] as char`
  (non-ASCII mojibake).
- Additive non-ASCII tests for both (`env_vars_expand` covers ASCII only).
- Commit message flags both as behavior fixes. Reviewer verifies redaction still
  hits secrets on ASCII input unchanged.

### Phase 2 — trivial hygiene (all mechanical, all additive)

**PR-2 · LLM-1, LLM-2, AGT-1, AGT-2, CLI-7** — risk Low, effort S
- `env_parse` (config.rs, 7+ sites), `xdg_path` (config.rs, 5 sites),
  `OVERFLOW_PHRASES` (loop.rs:113-132), dead `0..1_000_000` backstop (loop.rs:362) —
  pinned design: `loop { … }` keeping the trailing `Err` at loop.rs:1032 (the only
  option compatible with unmodified tests; do not clamp the env value),
  `cli.rs` one-arm merge (196-205).
- (AGT-3 was here in rev 1; moved to PR-11 — see review finding below.)

### Phase 3 — daemon

**PR-3 · SRV-1 `lock_map`** — risk Near-zero, effort S
- One `pub(crate) fn lock_map<T>(m: &Mutex<T>) -> MutexGuard<'_, T>` in daemon/mod.rs;
  replace ~45 (server.rs) + ~15 (mod.rs) `lock().unwrap_or_else(into_inner)` sites.
- Verification: `grep -n 'lock()' src/daemon | grep -v lock_map` returns nothing
  outside tests; confirm (by inspection) no locked section spans an `.await` — they
  don't today, and this PR must not create one.

**PR-4 · SRV-3, SRV-4, SRV-5, SRV-6, SRV-7** — risk Low, effort M
- `queue_tx` for steer/followup/recall — status-code order 400→404→409 preserved
  (guards: tests 2467-2558, 2852-2917).
- `journal_event` helper (5 sites, best-effort tolerance kept per site; **no**
  `spawn_blocking` switch in this PR).
- `chat` replay-chain flatten; `steal_wake_and_claim` restructure.
- `list_sessions` single sort (SRV-6; declared exemption: tie-order becomes
  deterministic instead of HashMap-random).
- `as_str()`/`From` impls + `DaemonInfo::default_for`; wire strings byte-identical
  ("read-only"/"ask-writes"/"ask-shell"/"trusted", "once"/"session"/"deny").

**PR-5 · SRV-2 turn-pipeline mechanical cleanup** — risk Med, effort M
- `spawn_queue_forwarder` dedup (−50 lines); `turn_result` as loop-value (kills two
  `unused_assignments` allows); drop always-`Some` Options; `TurnChannels` struct;
  channel pre-creation helper.
- Untouched: terminal-event sequencing block, guard drop ordering, idempotency
  recording. Gate: `client_end_to_end_hits_every_endpoint` + e2e suites.

### Phase 4 — free-floating dedup PRs (order among them is free)

**PR-6 · CLI-1, CLI-3 display/format dedup** — risk Low, effort S-M
- CLI-1, **narrowed per review:** extract only the *clip expression* into
  `fn clip_chars(s: &str, max_chars: usize) -> String` (char-boundary-safe,
  `…`-marked). Per-site limits (120 at format.rs:904, 88 at 937, 72 at 970,
  68 at 1012-1043), wrappers (`· then:`, `{:>3} │`, `+N more`), and line caps stay at
  each site — do NOT unify whole loops. `approval_details` (919-1114): extract the
  two verbatim-identical old/new preview loops (1012-1043) into a local
  `push_preview(&mut out, label, text)` closure; the repeated `if out.is_empty()`
  fallback may become a small local helper, per-arm behavior kept.
  `truncate_chars` (1233-1243) delegates to `clip_chars`; delete it only if
  `render_mcp_panel` allows (otherwise leave a thin alias). `metadata()` const table
  (CLI-3; preserve `chain`'s intentional `requires_shell: true`). Output tests pin
  exact strings (format.rs:1341-1449).

**PR-7 · CLI-2 single-audit dispatch** — risk Med-Low, effort M
- `dispatch_tool(...)` body without audits + single tail audit; preserve
  availability → `enforce_policy` → dispatch order (a denial must never prompt);
  collapse the two `mcp__` checks by deriving from `metadata()` (arm exists at 287);
  verify the two pre-dispatch Err paths keep their audit strings. Separate from PR-6
  to keep the Phase-0 gate path reviewable on its own.

**PR-8 · CLI-5, CLI-6 skills + main plumbing** — risk Low, effort S
- Shared `parse_frontmatter` (declared exemption: async path gains the invalid-name
  warning for parity with sync; note in body). Optional `TimedCache<T>`.
- `open_session` / `run_serve` / `run_or_exit` in main.rs (shell-escape path keeps
  its own persistence semantics).

**PR-9 · SRV-8 client/http.rs plumbing** — risk Low, effort S-M
- `session_url`/`post_json`/`boxed_err`/`lenient_array`; standardize on preserving
  the error chain (callers match message substrings only — note in body). Keep
  sync/async twins and all signatures; leave `SseFramer` alone.

**PR-10 · SES-1, SES-2 session.rs scanners** — risk Low, effort M
- `scan_jsonl_dir` + `sort_newest_first`; `for_each_line` owning the read loop.
- Declared exemption: all four scanners adopt `load_events`' `Interrupted` retry
  (strict improvement; `last_turn_state`/`max_event_seq` currently break on any
  error). Justify + pin with a test if one is missing.
- Prefilters stay in per-scanner closures (load-bearing).

**PR-14 · MCP-2, MCP-3, MCP-4, MCP-5, MCP-6, MCP-8, MCP-9 (extraction), MCP-10
(extraction)** — risk Low, effort M
- mcp.rs: `initialize_params`, `def_belongs_to`/`resource_reader_name`,
  `rpc_error`/`json_arr`/`clamp_output`, `insert_server` (preserve raw-vs-sanitized
  warn names via explicit display arg), `refresh_and_retry`, `extract_reply`,
  `try_snapshot`; plus the MCP-9/10 extraction halves (`secret_value_start`,
  `resolve` closure) — the BUG fixes themselves already shipped in PR-1.
- If review load is high, split mcp.rs/oauth.rs PRs (PR-14a/PR-14b) — nothing below
  depends on their order.

**PR-15 · MCP-1, MCP-7 oauth.rs** — risk Low, effort M
- `post_token_form` (exchange_code/refresh dedup; keep `invalid_grant` sniff),
  `login` → `resolve_client` + `authorize_and_exchange` (keep `invalid_target`
  retry byte-exact; no direct test — text moves only).

**PR-17 · LLM-6, LLM-7 stream.rs dedup** — risk Low-Med, effort M
- Delete the `#[cfg(test)]` sync SSE driver (`feed_raw` 403, `finish_turn` 513,
  `run_sse_lines` 801 — all test-only, verified); convert the ~20 driver tests with
  `#[tokio::test]` (tokio `macros` + `rt` features already in Cargo.toml) calling
  the async entry points — lower-churn than hand-rolled `block_on`.
- **Hang invariant (must be stated in PR body):** the sync path drops sink lines on
  a full channel (`try_send`); the async path back-pressures (`send().await`).
  Every converted test must keep the receiver alive (`_rx` retained) and stay under
  the buffer bound; any test emitting >32 sink lines without draining gets a
  bounded-drain helper instead of a hang (a hang = CI timeout, not a test failure).
- Also convert the printer test (stream.rs:1429) and delete the sync
  `StreamPrinter::feed_line`/`finish` methods (cleaner, more LOC win).
- Declared test-edit exemption: conversions touch test code only; `handle_function_call_item`
  for the added/done duplicate (stream.rs:1033-1055 vs 1087-1107).

### Phase 5 — agent

**PR-11 · AGT-3 (reworked), AGT-4, AGT-7 agent hygiene** — risk Low-Med, effort S-M
- **AGT-3, reworked per review finding 1:** `#[derive(Clone)]` on `ToolState` is a
  compile error — `ProjectionState` holds four `Mutex<…>` fields (obs_pack.rs:328-339).
  Instead: manual `impl Clone for ProjectionState` cloning each mutex's inner data
  (`Mutex::new(self.sends.lock().unwrap_or_else(|e| e.into_inner()).clone())`, ×4),
  then derive Clone on `ToolState`. The loop.rs:982-998 literal becomes
  `let mut to_save = state.clone(); to_save.obs_projection = ProjectionState::new();`
  — compile-enforced field exhaustiveness (the actual goal: state.rs:1's
  `#![allow(dead_code, unused_variables)]` hides forgotten fields), zero hand-written
  field lists. Reviewer checks no other call site starts relying on cloning live
  projection state unintentionally.
- AGT-4: `ToolState` load/save → `spawn_blocking` wrappers with pinned semantics:
  the `DEX_TOOL_CACHE` gate and `dirty` checks stay **outside** the blocking closure
  (else every turn pays a pool round-trip with caching off, and `load_async` sits on
  the daemon hot path, server.rs:1256); the wrapper **awaits** the `spawn_blocking`
  JoinHandle — a detached spawn is forbidden (loop.rs:979-981 comment: teardown can
  drop it). `save_async` has exactly one caller (loop.rs:996) which owns `to_save`,
  so the `'static` move is possible.
- AGT-7: `cut_point_at` + `back_up_over_tools` (guards: `cutoff_*`,
  `emergency_compaction_cuts_below_the_comfort_floor`).

**PR-12 · AGT-5 `process_turn` decomposition** — risk Med, effort L (biggest PR)
- Extract verbatim: `rewrite_session` (3 copies — journal invariant single-sourced),
  `note_sink` generalization (3 emit pairs; stop-reason computed once),
  `drain_steering`/`inject_steering`, `compaction_gate`, `run_tool_batch`,
  `process_tool_result` (small context struct).
- Move-verbatim list: compaction gate math, `archivable_tokens` pre-measurement
  order, one-retry `overflow_retried`.
- Gates: `context_overflow_compacts_and_retries_once`,
  `online_compaction_compacts_at_an_economical_boundary`, `repeated_tool_guard_*`.

**PR-13 · AGT-6 compaction.rs cleanup** — risk Med-Low, effort M
- `llm_summary` (634-716; cancellation string at 652-655 byte-identical —
  `loop.rs:460` matches `e.contains("cancelled")`) + `attach_file_section` tail dedup.
- Preserve the `history_summary.trim()` vs untrimmed asymmetry (694-707). Leave the
  final splice (726-731) untouched.

### Phase 6 — config.rs (strict order: PR-16 → PR-18 → PR-19)

**PR-16 · LLM-3, LLM-4 (+ LLM-10 optional)** — risk Low, effort M
- `resolve_model_cost`; generic `cached_parse` with three thin call sites.
  Guards: config.rs:2700-2860 tier-order tests, hermetic cache tests.

**PR-18 · LLM-8 `from_env`/`doctor` shared resolution-with-origin** — risk Med-Hi,
effort L
- `Resolved { value, origin }` per knob; shared `resolve_*` helpers consumed by both.
  Extraction style (same call sequence), not a rewrite. `warn_once` fire-order is
  contract; doctor output must not change byte-for-byte (doctor snapshot tests gate).
  Own PR, no strays.

**PR-19 · LLM-9 routing classifier** — risk Med-Hi, effort L (last)
- `classify_selection -> SelectionRoute` consumed by `split_selection`,
  `strip_routing_prefixes`, `apply_model`. Only after PR-18 green; gate:
  `apply_model_routes_prefixed_selection_to_endpoint` + prefix tests.

### Phase 8 — closure

- Re-run the metric once; fill the final column; if targets are short by >30%,
  record why and decide on skip-list items (LLM-5, LLM-10, CLI-4).
- Update `docs/complexity-issues.md` checkboxes; archive the metric diff here.

## Dependency graph (PR → must land after)

```
PR-1 → PR-14 (mcp.rs rebases on bug fixes)
PR-2 → PR-11 → PR-12 → PR-13          (agent files)
PR-2 → PR-16 → PR-18 → PR-19          (config.rs chain)
PR-3 → PR-4 → PR-5                    (daemon)
PR-6 → PR-7                           (tools/mod.rs display before dispatch)
everything else (PR-8, PR-9, PR-10, PR-14, PR-15, PR-17) floats after PR-2
```

## Effort summary

| Phase | PRs | Risk | Effort |
|---|---|---|---|
| 1 bugs | PR-1 | Low | S |
| 2 hygiene | PR-2 | Low | S |
| 3 daemon | PR-3, PR-4, PR-5 | Low→Med | S, M, M |
| 4 dedup | PR-6, 7, 8, 9, 10, 14, 15, 17 | Low (PR-7 Med-Low, PR-17 Low-Med) | S–M each |
| 5 agent | PR-11, PR-12, PR-13 | Low-Med→Med | M, L, M |
| 6 config | PR-16, PR-18, PR-19 | Low→Med-Hi | M, L, L |

Definition of done: all 48 ledger items checked or explicitly skipped with reason;
metrics table final column filled; BUG-1/BUG-2 fixed and shipped; zero capability or
wire changes; three checks green at every merge point.
