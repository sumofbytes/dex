# Complexity issues — actionable ledger

Companion to [complexity-review.md](complexity-review.md) (full rationale, file:line
details, do-not-touch list). This file lists every issue found by the six parallel
module reviews as discrete, plannable items. Checkboxes track status.

ID scheme: `LLM-`, `AGT-`, `SES-`, `SRV-`, `CLI-`, `MCP-`, `BUG-`.

Legend: Risk = chance of behavior drift (Low / Med / Hi). Effort = S (<1h) / M (hours) / L (>1d).

## Index

| ID | Issue | Area | Risk | Effort | Est. win |
|---|---|---|---|---|---|
| LLM-1 | `env_parse` helper for 7+ repeated env-var parse chains | llm/config.rs | Low | S | ~20 branch pts |
| LLM-2 | `xdg_path` helper for 5 duplicated XDG-vs-HOME resolutions | llm/config.rs | Low | S | ~15 branch pts |
| LLM-3 | Shared `resolve_model_cost` for `usage_cost` + `cache_write_read_ratio` | llm/config.rs | Low | S | ~10 branch pts |
| LLM-4 | Generic `cached_parse` for 3 file-identity caches | llm/config.rs | Low | M | ~25 branch pts |
| LLM-5 | `catalog_models` iterator for 6 dual-shape scans | llm/config.rs | Med | M | ~30 branch pts |
| LLM-6 | Delete test-only sync SSE driver; or single `feed_event` match | llm/stream.rs | Low-Med | M | ~80 branch pts |
| LLM-7 | `handle_function_call_item` for added/done duplicates | llm/stream.rs | Low | S | ~20 lines |
| LLM-8 | Shared resolution-with-origin for `from_env` + `doctor` | llm/config.rs | Med-Hi | L | ~440 lines, whole file's core |
| LLM-9 | `classify_selection` routing classifier (3 sites) | llm/config.rs | Med-Hi | L | ~90 lines |
| LLM-10 | Header merge funnels (modest) | llm/config.rs | Low | S | ~15 lines |
| AGT-1 | `OVERFLOW_PHRASES` const for 13 chained `contains` | agent/loop.rs | Low | S | ~10 branch pts |
| AGT-2 | Dead `for _iteration in 0..1_000_000` backstop cleanup | agent/loop.rs | Low | S | clarity |
| AGT-3 | Derive `Clone` on `ToolState` (save-path struct literal) | agent/loop.rs | Low | S | 12-field hazard |
| AGT-4 | `ToolState` load/save sync-async dedup via `spawn_blocking` | agent/state.rs | Low | S | ~80 lines |
| AGT-5 | Decompose `process_turn` (~700 lines) into ~5 helpers | agent/loop.rs | Med | L | biggest single win |
| AGT-6 | `compact_history`: extract `llm_summary` + `attach_file_section` | agent/compaction.rs | Med-Low | M | ~60 lines |
| AGT-7 | `find_cut_point`: `cut_point_at` + `back_up_over_tools` | agent/compaction.rs | Low | M | dedup of semantic core |
| CLI-1 | `clip_chars` helper replacing 7+ copies | core/format.rs, tools/mod.rs | Low | S-M | ~120 lines |
| CLI-2 | Single-audit `execute_with_shell` via `dispatch_tool` | tools/mod.rs | Med-Low | M | ~10 exit pts → 1 |
| CLI-3 | `metadata()` const table | tools/mod.rs | Low | S | ~45 lines |
| CLI-4 | `content_lines` classifier for 4 summary loops | core/format.rs | Low-Med | M | optional |
| CLI-5 | `skills.rs`: shared `parse_frontmatter` + optional `TimedCache` | skills.rs | Low | S | ~50 lines, fixes warning drift |
| CLI-6 | `main.rs`: `open_session`, `run_serve`, `run_or_exit` | main.rs | Low | S | ~90 lines |
| CLI-7 | `cli.rs` one-arm merge | cli.rs | Low | S | tiny, optional |
| SES-1 | `scan_jsonl_dir` + `sort_newest_first` (4 scans) | session.rs | Low | S | ~80 lines |
| SES-2 | `for_each_line` scanner loop (4 scanners) | session.rs | Low | M | ~60 lines |
| SRV-1 | `lock_map` mutex helper (~45× boilerplate) | daemon/server.rs, mod.rs | Near-zero | S | −45 branch pts, −90 lines |
| SRV-2 | `run_agent_turn`/`run_turn_inner` mechanical cleanup | daemon/server.rs | Med | M | forwarders, dead Options, TurnCtx |
| SRV-3 | `queue_tx` for steer/followup/recall triplication | daemon/server.rs | Very low | S | −10 branch pts, −45 lines |
| SRV-4 | `journal_event` helper (5 sites) | daemon/ | Low | S | −35 lines |
| SRV-5 | `chat` replay-chain flatten + `steal_wake_and_claim` restructure | daemon/server.rs | Low | S | −5 branch pts |
| SRV-6 | `list_sessions` single sort, drop `by_id` map | daemon/server.rs | Low | S | −15 lines, deterministic ties |
| SRV-7 | `as_str()`/`From` impls + `DaemonInfo::default_for` | daemon/, core/types.rs | Low | S | −8 branch pts |
| SRV-8 | `client/http.rs`: `session_url`/`post_json`/`boxed_err`/`lenient_array` | client/http.rs | Low | M | −150 lines |
| MCP-1 | `post_token_form` for exchange_code + refresh_access_token | mcp/oauth.rs | Low | S | ~30 lines |
| MCP-2 | Single `initialize_params()` (3 hand-built payloads) | mcp.rs, oauth.rs | Low | S | drift hazard |
| MCP-3 | `def_belongs_to` + `resource_reader_name` (3 filters, 4 name builds) | mcp.rs | Low | S | grammar single-source |
| MCP-4 | `rpc_error`/`json_arr`/`clamp_output` for McpClient | mcp.rs | Low | S | sentinel-safety |
| MCP-5 | `refresh_and_retry` + `extract_reply` in HttpTransport | mcp.rs | Low | S-M | CC 10-13 → small |
| MCP-6 | Shared `insert_server` for JSON/YAML config paths | mcp.rs | Low | S | dedup loop |
| MCP-7 | `login` decomposition (CC 15 → ~5) | mcp/oauth.rs | Low | M | ~60 lines |
| MCP-8 | `try_snapshot` for ephemeral_line/cached_statuses | mcp.rs | Low | S | dedup |
| MCP-9 | `redact_line`: extract `secret_value_start` (CC 15 → ~4) | mcp.rs | Low | S | + bug fix |
| MCP-10 | `expand_env`: `resolve` closure (CC 12 → ~5) | mcp.rs | Low | S | + bug fix |
| BUG-1 | `redact_line` to_lowercase byte-offset desync | mcp.rs:431 | bug fix | S | correctness |
| BUG-2 | `expand_env` non-ASCII mojibake (`bytes[i] as char`) | mcp.rs:142 | bug fix | S | correctness |

## Issue details

### LLM-1 — `env_parse` helper
`env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)` repeated 7+ times
(config.rs:1624-1629, 1755-1777, 1778-1785); `stream.rs:663-667` already has the same
helper (`env_secs`) but private. Add one `fn env_parse<T: FromStr>(name, default) -> T`,
use everywhere. Negligible risk.

### LLM-2 — `xdg_path` helper
Five path resolvers duplicate `env XDG_* > HOME/.…/rel` (config.rs:14-22, 558-564,
682-688, 796-801, 818-823). One helper with the env-var as parameter (three different
vars are in play: `DEX_CONFIG`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`). Negligible risk.

### LLM-3 — shared `resolve_model_cost`
`usage_cost` (config.rs:1109-1143) and `cache_write_read_ratio` (2041-2071) run the
identical 3-tier `catalog_cost` chain (`endpoint-exact` → `provider-keys` → `any`) and
the same "missing rate bills at input" assumption. Extract one lookup fn; tests at
2700-2860 pin tier order.

### LLM-4 — generic `cached_parse`
Three implementations of stat → `(path, mtime, len)` cache → read+parse → store:
`load_config_file` (38-100), `learned_api_map` (578-612), `load_dex_catalog` (942-969),
plus mirrored invalidation (102-106, 641-644). One generic helper with thin call sites;
preserve mtime+len identity and poisoned-mutex recovery semantics.

### LLM-5 — `catalog_models` iterator
Six functions each walk "api.json providers shape" then "catalog.json flat shape" with
nested `get().and_then(as_object)` chains (~6 branches × 6). One iterator helper +
`models_get_ci`. Must preserve: providers-shape wins, first provider in catalog order,
CI matching with exact-key preference (`catalog_limit.lookup`). Tests: pricing/ctx.

### LLM-6 — stream.rs sync/async dedup
`feed_line_inner` vs `_async`, `feed_raw` (test-only) vs `_async`, `finish_turn`/`finish`
pairs: ~250 lines byte-identical. Either delete the `#[cfg(test)]` sync paths and run
parser tests via `block_on(feed_raw_async(...))` on a current-thread runtime, or keep one
`feed_event(&mut self, event)` match called from both wrappers (wrappers differ only in
`try_send` vs `send().await`). Work: converting ~20 driver tests.

### LLM-7 — `handle_function_call_item`
`output_item.added` / `output_item.done` function-call handling (stream.rs:1033-1055,
1087-1107) is a verbatim ~20-line duplicate → one method.

### LLM-8 — from_env/doctor shared resolution (highest value in repo)
`doctor()` (config.rs:2184-2619, ~440 lines) re-derives every precedence chain that
`from_env` (1618-1861) already implements — including a second hand-rolled
`using_builtin_default` condition, header layering, and credential order ("Mirror
`resolve_credentials`" comment at 2376-2393). Resolution rules exist twice; doctor lies
if one drifts. Introduce `Resolved { value, origin }` per knob; shared helpers consumed
by both. As extraction (same call sequence), not rewrite. Precedence and `warn_once`
fire-order must be identical. Do as its own PR after 1-4 are in.

### LLM-9 — routing classifier (do last)
`split_selection` (262-286), `strip_routing_prefixes` (1910-1924), `apply_model`
(1933-2022) all classify `provider/model` prefixes with different outcomes. Extract pure
`classify_selection -> SelectionRoute`; `apply_model` keeps side effects. Subtlest
semantics in the file (explicit `base_url` pins never re-route; `moonshotai/kimi`
survives). Only with `apply_model_routes_prefixed_selection_to_endpoint` green.

### LLM-10 — header merge funnels (modest)
`config_headers_map`'s mapping/text/sequence arms → one `merge_one(out, &Value)`;
extract `parse_json_headers`. Do NOT unify `parse_headers_str` grammars
(1381-1387 compat behavior is load-bearing).

### AGT-1 — `OVERFLOW_PHRASES` const
`is_context_overflow` (loop.rs:113-132) is 13 chained `contains`. Convert to
`const OVERFLOW_PHRASES: &[&str]` + `.any(...)`, keep the "reduce the length" + anchor
pair separate. Test `overflow_wording_is_recognized` pins every phrase — leave set
untouched.

### AGT-2 — loop bound cleanup
`for _iteration in 0..1_000_000` (loop.rs:362, trailing Err at 1032) is a dead backstop
vs the real tool budget (960-973). Use `loop { … }` with the budget check, keep the
trailing `Err` for exhaustiveness, or document the backstop / clamp the env value.

### AGT-3 — `Clone` on `ToolState`
Save path (loop.rs:982-998) hand-writes a 12-field struct literal (deliberately
substituting `obs_projection: ProjectionState::new()`); `state.rs:1` disables
dead_code/unused lints, so a forgotten field would not warn. Derive `Clone`:
`let mut to_save = state.clone(); to_save.obs_projection = ProjectionState::new();`.

### AGT-4 — ToolState sync/async dedup
`load`/`load_async` and `save`/`save_async` (state.rs:110-186) are two identical pairs
(std vs tokio fs); `DEX_TOOL_CACHE` gate + best-effort policy restated 4×. Keep
sync as the implementation, async as thin `spawn_blocking` wrappers (once per turn
teardown — blocking is harmless). Check `save_async` callers first.

### AGT-5 — decompose `process_turn` (loop.rs:315-1033, ~700 lines)
Extractions, each a pure move of existing code:
- `rewrite_session(session, messages) -> io::Result<()>` — dedups the post-compaction
  rewrite **3 copies** (437-443, 509-515, 902-908). Journal-invariant single-sourcing:
  correctness win.
- generalized `system_note`/`note_sink` — dedups 3 sink/no-sink emit pairs (535-566,
  681-700, 820-850); compute stop-reason message once, emit once.
- `drain_steering(...)` + `inject_steering(...)` — 2 copies (368-385, 1010-1026).
- `compaction_gate(...)` — the `while compaction_attempts < 3` block (411-463).
- `run_tool_batch(...)` — serial/JoinSet scheduling (580-638).
- `process_tool_result(...)` — per-call cache/guard/reducer/boundary/emit (655-951);
  needs a small context struct or outcome enum (many borrows).
Move verbatim: `need_by_tokens` / count fallback / `compaction_attempts < 3`;
`archivable_tokens` measured *before* `compact_history` rewrites (426-428);
one-retry `overflow_retried`. Guards: `context_overflow_compacts_and_retries_once`,
`online_compaction_compacts_at_an_economical_boundary`, `repeated_tool_guard_*`.

### AGT-6 — `compact_history` extraction (compaction.rs:551-733)
Extract `async fn llm_summary(...) -> Result<(String, Option<Usage>), String>`
(634-716) — cancellation string check (652-655) must stay byte-identical (`loop.rs:460`
matches `e.contains("cancelled")`). Fold duplicated file-section tail into
`attach_file_section`. Preserve the `history_summary.trim()` vs untrimmed asymmetry
between split and non-split arms (694-707). Leave the final splice (726-731) untouched.

### AGT-7 — `find_cut_point` extraction (compaction.rs:60-164)
Trailing block (130-157) recomputes what 119-125 already computed; tool-backup `while`
duplicated (114-116, 137-140). Extract `cut_point_at(messages, idx, start) -> CutPoint`
and `back_up_over_tools(messages, idx, start) -> usize`. Tests: `cutoff_*`,
`emergency_compaction_cuts_below_the_comfort_floor`.

### CLI-1 — `clip_chars` helper
Seven+ copies of char-clip+`…` (format.rs:18-24, 146-162, 1233-1243, 904-907, 937-946,
970-974, and two verbatim-identical 15-line loops at 1012-1043; tools/mod.rs:863-869).
Plus `approval_details` (919-1114, 195-line match): extract `push_preview` closure for
the 6× repeated `if out.is_empty()` fallback; optionally split arms into
`details_bash/details_write/details_edit`. `truncate_chars` (1233) can be deleted once
it delegates (only used by `render_mcp_panel`). Tests pin exact output (1341-1449).

### CLI-2 — single-audit dispatch (tools/mod.rs:1597-1763)
~10 exit points each repeating `audit(name, args, &outcome); return ...`. Rename body to
`dispatch_tool(...)` (no audits) + one tail audit. Preserve order: availability →
`enforce_policy` → dispatch (a denial must never prompt). Collapse the two
`name.starts_with("mcp__")` checks (1638, 1686) by deriving from `metadata()` (arm
exists at 287). Verify the two pre-dispatch Err paths keep audit strings.

### CLI-3 — `metadata()` table (tools/mod.rs:214-296)
Shared `const READONLY: ToolMetadata` for the 8 identical read-only tools; only `git`,
`chain`, `bash`, `write|edit`, `mcp__` deviate. Preserve `chain`'s intentional
`requires_shell: true`. Dispatch's `unreachable!` (1723) keeps sync failures loud.

### CLI-4 — `content_lines` classifier (optional)
Four copies of the summary line-scan loop (format.rs:404-462 read_summary, 537-570
find/fffind/ls arm, 596-632 chain arm, 291-319 read_preview_lines). Counting rules
differ subtly per tool — classifier needs per-tool flags. Skip unless metrics gate.

### CLI-5 — skills.rs dedup
`discover_skills_fresh_async` (160-197) re-implements `parse_skill`'s frontmatter
parsing inline and silently drops the invalid-name warning the sync path emits (35-40)
— behavior divergence. Extract `parse_frontmatter` used by both; unify the warning. Two
identical 10s-caches (68-93, 229-252): optional `TimedCache<T>`. Parity tests (370-427)
verify.

### CLI-6 — main.rs extraction
`open_session(args, cwd)` for the duplicated session-open ladder (99-107, 162-184);
`fn run_serve(bind)` out of the 200-line `main()` match (Serve arm 357-393) — isolates
socket parsing for unit testing; `run_or_exit(Res<String>, usage)` for the three
identical Ok/print-Err blocks in the Mcp arm (500-542). Shell-escape path keeps its own
persistence semantics (`!!` exclusion).

### CLI-7 — cli.rs one-arm merge (optional)
`cli.rs:196-205`: last two arms identical → one `Some(_)` arm. The whitespace-prompt
subtlety at 236-241 and `-H` prefix checks stay as-is.

### SES-1 — `scan_jsonl_dir` (session.rs:449-468, 472-500, 508-532, 1008-1054)
`list`, `list_all`, `list_children`, `list_all_async`'s closure duplicate
read-dir/filter-jsonl/first-line/parse/sort; "Only the header is needed…" comment
appears twice verbatim. One helper + `sort_newest_first`; `list_children` enriches with
`last_turn_state`. Guards: `list_children_reports_runs_and_interrupted_state`,
`parent_listing_and_loader_ignore_child_sessions`.

### SES-2 — `for_each_line` (session.rs:748-847, 1070-1098)
`load_events`, `max_event_seq`, `last_turn_state`, `load_session_state` each hand-roll
the read-line loop. One `for_each_line(path, f)` owning the `Interrupted` retry (use
`load_events`'s strict behavior — strict improvement for the others, or keep both via a
flag if strictness matters). Substring prefilters are load-bearing — keep in closures.
Note `max_event_seq` adds a third pattern variant (`"seq":` precheck).

### SRV-1 — `lock_map` helper (near-zero risk, biggest count win)
`m.lock().unwrap_or_else(|e| e.into_inner())` ~45× in server.rs + ~15 in daemon/mod.rs
(244-364). One `pub(crate) fn lock_map<T>(m: &Mutex<T>) -> MutexGuard<'_, T>`;
`timeout_pending` (server.rs:1532-1539) is already this helper for one map — generalize.
Mechanical; mutex sections never span `.await`.

### SRV-2 — turn-pipeline mechanical cleanup (server.rs:638-1356)
- One `spawn_queue_forwarder` for the duplicated steering/follow-up forwarder tasks
  (737-762, 763-788; ~50 duplicated lines).
- `turn_result` as `let turn_result = loop { match … break Ok/Err } }` — removes two
  `#[allow(unused_assignments)]` (867, 1257) and 3 assignment sites (1264, 1313, 1327,
  1336).
- Drop always-`Some` Options `steering_accepted_tx`/`followup_accepted_tx` (875-877;
  dead branch at 1317-1319).
- One helper for the channel pre-creation/reuse dance (`chat` 530-568 + fallback
  714-731) returning `(steering_rx, followup_rx)`.
- Bundle the 9/11-arg signatures into `TurnChannels`/`TurnCtx` (kills two
  `too_many_arguments` allows at 637, 867).
Leave untouched: terminal-event sequencing block, guard drop ordering (820-824),
idempotency recording (861-863).

### SRV-3 — `queue_tx` (server.rs:1795-1874)
`steer`/`followup`/`recall` are three copies of one handler. One map-selector helper +
three ≤10-line handlers; recall's followup selection (1862-1866) shares it. Preserve
per-route status-code order 400 → 404 → 409 (tests 2467-2558, 2852-2917).

### SRV-4 — `journal_event` (5 sites)
Terminal journaling 843-858; sink bridge 1101-1103, 1191-1193; forwarders 748-758,
774-784; child-approval bridge 1482-1495. One helper in daemon/mod.rs next to
`broadcast_event`. Keep best-effort tolerance per site; do NOT switch to
`spawn_blocking` in the same commit.

### SRV-5 — chat replay chain (server.rs:484-611)
Flatten nested `if let Some(key) { if let Some(terminal) { … } }` (513-518) into
`and_then` chain. `steal_wake_and_claim` (1920-1945) → `match try_claim_slot` with
early returns. Replay must not claim the turn slot (ordering 521-536 stays).

### SRV-6 — `list_sessions` single sort (server.rs:390-482)
Sorts typed tuples, flattens to HashMap (loses order), re-sorts by re-parsing
`created_at` strings out of serialized JSON (469-480). Sort typed tuples once (timestamp
desc, tie-break by id), then map to `json!`; concat in-memory fallback before the sort.
Tie ordering becomes deterministic instead of HashMap-random.

### SRV-7 — enum↔string impls
`PermissionMode → &str` (190-195); protocol→core→`&str` double match for
`ApprovalDecision` (1712-1725); duplicated `DaemonInfo` literal in Err fallback
(202-226). Add `as_str()`/`From` in `src/core/types.rs` (snake_case already the serde
wire spelling) and `DaemonInfo::default_for(cwd, …)`. Strings must match wire exactly.

### SRV-8 — client/http.rs plumbing
`session_url` + `post_json` (20 copies of the post+headers boilerplate); one
`boxed_err` (three coercion spellings today, some losing the error chain — standardize
on preserving it); `lenient_array<T>` for sessions/skills/events rows. Keep the
deliberate sync/async twin pattern and signatures; leave `SseFramer` (146-224) alone.

### MCP-1 — `post_token_form` (oauth.rs:655-749)
`exchange_code` and `refresh_access_token` are the same ~45-line ladder. Extract the
shared POST flow; callers keep context strings and refresh's `invalid_grant` sniff
(733-735). `register_client` (572-617) can reuse a `post_json` sibling later.

### MCP-2 — `initialize_params()` (drift hazard)
`protocolVersion: "2024-11-05"` + `clientInfo` hand-built in 3 places (mcp.rs:670-674,
796-800; oauth.rs:908-914 — which also re-builds the envelope `rpc_request` provides).
One helper + reuse `rpc_request(0, "initialize", initialize_params())` in
`probe_challenge`.

### MCP-3 — tool-name grammar single-source
The "belongs to server" filter written 3× (mcp.rs:1254-1257, 1277-1284, 1338-1345);
synthetic name built in 4 places (509, 1255, 1281, 1342); `"\0resource"` sentinel in 2
(227, 1237). Add `def_belongs_to(def_name, server)` + `resource_reader_name(server)`,
reuse in `resource_reader_definition` + `cache_server_into`.

### MCP-4 — McpClient result plumbing
`__mcp_error` check duplicated 3× (989-991, 1032-1034, 1056-1058); array-unwrap ladder
2× (993-997, 1062-1066); output clamp 2× (578-581, 1082-1085). Extract `rpc_error`,
`json_arr`, `clamp_output` — makes the sentinel check structurally unavoidable.

### MCP-5 — HttpTransport splits
Extract `refresh_and_retry` (the 20-line inline 401-refresh arm in `request_inner`,
786-832) and `extract_reply` (whole-JSON-then-SSE-`data:` scan, 906-924). The stdio vs
HTTP demux split is inherent — leave it.

### MCP-6 — `insert_server`
JSON env path (370-401) duplicates the YAML path entry loop (335-350). Shared
`insert_server(out, display, raw)`; preserve the raw-vs-sanitized name difference in
warnings (390 vs 348) by passing display explicitly.

### MCP-7 — `login` decomposition (oauth.rs:943-1052, CC ≈ 15)
Extract `resolve_client(...)` (979-1003) and `authorize_and_exchange(...)` (1005-1040);
`login` becomes a pipeline. Keep the `invalid_target` retry exactly. No direct test on
`login` — behavior-preserving text moves only.

### MCP-8 — `try_snapshot`
`ephemeral_line` (1498-1512) vs `cached_statuses` (1527-1538): same triple `try_read` +
`status_list` snapshot. Keep the `configs.is_empty()` early return in `ephemeral_line`'s
caller only.

### MCP-9 — `redact_line` split + BUG-1
Extract `secret_value_start(line, marker)` (inner `while`, 431-468); top function drops
to CC ≈ 4. **Bug:** `line.to_lowercase()` at 432 is Unicode-aware — byte offsets
desync from `line.as_bytes()`; use `to_ascii_lowercase` (markers ASCII; same behavior
on ASCII input). Same fix already exists in oauth.rs:198-200.

### MCP-10 — `expand_env` closure + BUG-2
Dedup the `match std::env::var(key)` push/Err block (115-120 vs 131-136) via a local
`resolve` closure (CC 12 → ~5). **Bug:** `out.push(bytes[i] as char)` at 142 mojibakes
non-ASCII (`café$X` → `cafÃ©…`); iterate chars instead. Add a non-ASCII test
(`env_vars_expand` only covers ASCII today). This is a behavior fix — call out in the
commit message.

## Known non-issues (do not plan)

- `resolve_workspace_path` + `lexical_normalize_fallback`, `enforce_policy`,
  `atomic_write` (tools) — security/durability boundaries.
- Compaction trigger math + `archivable_tokens` ordering + one-retry overflow (agent).
- `warn_once` fire-order and precedence semantics (llm/config.rs).
- Journal marker / durability sniffing (session.rs).
- `parse_headers_str` grammars; `try_responses_fallback`; `SseFramer`;
  `require_bearer`; `TurnGuard`/`ShellGuard` Drop impls; sink-bridge coalescing.
- `protocol/mod.rs` `StreamEvent` enum and non-wildcarded event matches (exhaustiveness
  is a feature).
- Transport trait boxed-future boilerplate (mcp.rs) — macro saves ~18 lines, adds
  indirection.
- `usage.rs chart`, `update.rs:54-82` — read fine as-is.

## Suggested phases (for planning)

1. **Phase 1 — trivial helpers (parallel-safe, near-zero risk):**
   LLM-1, LLM-2, SRV-1, AGT-1, AGT-2, AGT-3, CLI-7.
2. **Phase 2 — pure dedup refactors:** LLM-3, LLM-4, SES-1, SES-2, CLI-1, CLI-5,
   CLI-6, SRV-3, SRV-4, SRV-5, SRV-6, SRV-7, SRV-8, MCP-2, MCP-3, MCP-4, MCP-6, MCP-8.
3. **Phase 3 — mechanical but test-guarded:** CLI-2, CLI-3, LLM-6, LLM-7, AGT-4,
   AGT-7, SRV-2, MCP-1, MCP-5, MCP-7, MCP-9 (with BUG-1).
4. **Phase 4 — bug-fix PR (own PR, flagged in commit):** BUG-1+MCP-9, BUG-2+MCP-10.
5. **Phase 5 — agent decomposition:** AGT-5, AGT-6.
6. **Phase 6 — config unification:** LLM-8, then LLM-9 (last, subtlest).
7. **Skip unless gated:** CLI-4, LLM-5, LLM-10 (LLM-5 medium risk/value).

Verification per phase: `cargo fmt -- --check && cargo test --all-targets && cargo
clippy --all-targets -- -D warnings`. Key guards: HTTP e2e (`client/http.rs:1271`),
status-code order tests (`server.rs:2409-2435`, `2467-2558`), compaction tests
(`agent/loop.rs`, `agent/compaction.rs`), config precedence/doctor tests,
format/output tests, oauth local-mock tests.
