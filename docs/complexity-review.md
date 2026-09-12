# Complexity review — parallel module audit (2025)

Six parallel reviewer agents evaluated the codebase module-by-module for excessive
cyclomatic complexity and concrete behavior-preserving simplifications. Totals reviewed:
~52k lines across ~30 files.

**Overall verdict:** the codebase is in better shape than raw branch counts suggest.
Most complexity is *inherent* (tool dispatch tables, config precedence, protocol
handling). The real wins are duplicated patterns inside each module — every module
reviewed found the same smell: the same 2–5 shapes copy-pasted rather than factored.
Roughly 8 behavior-preserving refactors are worth doing, plus **2 latent bugs** found
along the way.

| Module | Verdict | Biggest win |
|---|---|---|
| `src/core/format.rs` + `src/tools/` | refactor-worthy | 7+ copies of the same char-clip logic |
| `src/llm/config.rs` | refactor-worthy | `doctor()` re-derives `from_env` precedence (~440 lines) |
| `src/agent/` + `src/session.rs` | refactor-worthy | `process_turn` ~700 lines; 3× duplicated session-rewrite |
| `src/daemon/` | mostly fine, one easy win | Mutex boilerplate repeated ~45× |
| `src/mcp.rs` + `src/mcp/oauth.rs` | mostly fine, 2 bugs | OAuth token-POST flow duplicated |
| `src/protocol/`, `prompt.rs`, `provider.rs` | nothing worth touching | — |

---

## 1. `src/core/format.rs` + `src/tools/` (tools, core, cli, skills)

### 1.1 One `clip_chars` helper instead of seven (highest impact)

At least seven independent implementations of "clip at N chars and append `…`":

- `format.rs:18-24` (`clamp_lines` inner map)
- `format.rs:146-162` (`truncate_cols`)
- `format.rs:1233-1243` (`truncate_chars`)
- `format.rs:904-907` (`then_run_of`)
- `format.rs:937-946` (bash details)
- `format.rs:970-974` (write preview)
- `format.rs:1012-1024` and `1029-1043` (edit old/new preview — two *verbatim-identical*
  15-line loops)
- plus `src/tools/mod.rs:863-869` (`clip_command`)

The `char_indices().nth(n).map(|(i, _)| i).unwrap_or(len)` dance is copy-pasted
everywhere. `approval_details` (`format.rs:919-1114`) is a 195-line match where each arm
re-implements "push optional line, fall back to raw input" (`if out.is_empty() {
vec![input] } else { out }` repeated 6×).

**Simplification:** one `fn clip_chars(s: &str, max_chars: usize) -> String`
(char-boundary-safe, `…`-marked); `truncate_chars` delegates to it (then can be deleted
outright — only used by `render_mcp_panel`); `then_run_of`/`clip_command`/`approval_details`
all call it. Inside `approval_details`, extract the duplicated old/new preview loop into a
local closure `push_preview(&mut out, label, text)`. Optionally split the arms into small
`details_bash/details_write/details_edit` fns so the match becomes a table.

**Risk: low** — pure display code; tests (`format.rs:1341-1449`, `clipped_at_budget`)
pin exact output.

### 1.2 `execute_with_shell`: repeated audit-and-return plumbing (`tools/mod.rs:1597-1763`)

~10 exit points each repeating `audit(name, args, &outcome); return ...`
(lines 1616-1618, 1633-1636, 1654-1656, 1677-1680, 1682-1685, 1687-1693, 1696-1710,
1757-1762).

**Simplification:** rename the body into `dispatch_tool(...) -> Result<String, ToolError>`
with no audit calls, then a single tail:

```rust
let result = dispatch_tool(...).await;
let outcome = match &result { Ok(_) => "ok".into(), Err(e) => e.to_string() };
audit(name, args, &outcome);
result
```

The two `name.starts_with("mcp__")` checks (1638, 1686) can collapse by computing
`requirement` once from `metadata()` (the `mcp__` arm already exists at line 287).

**Risk: medium-low.** Order of availability-check → `enforce_policy` → dispatch must be
preserved exactly (a denial must never prompt). Verify the two pre-dispatch `Err` paths
(`then_run_command` failure, unknown tool) keep their audit strings.

### 1.3 `metadata()`: 13 struct-literal branches (`tools/mod.rs:214-296`)

Eight read-only tools share an identical field set.

**Simplification:** a `const READONLY: ToolMetadata` shared by
`"read" | "grep" | "ffgrep" | "find" | "fffind" | "ls" | "obs_recall" | "update_plan"`;
only `git`, `chain`, `bash`, `write|edit`, `mcp__` deviate. Drops ~70 lines to ~25.

**Risk: low** — `metadata_classifies_tools` test covers the essentials; dispatch has an
`unreachable!` guard keeping metadata/dispatch in sync. Caveat: `chain`'s odd
`requires_shell: true` is intentional (shells out for sub-steps) — preserve it.

### 1.4 `tool_result_summary` + `read_summary`: four copies of the same line-scanning loop

`read_summary` (`format.rs:404-462`), the `find|fffind|ls` arm (537-570), the `chain` arm
(596-632), and `read_preview_lines` (291-319) all walk `text.lines()`, skip
empty/`[...`/trailer lines, and fold `trailer_more_lines` counts into a `(+N more)` tail.

**Simplification:** one `content_lines(text) -> (count, trailer_more)` classifier helper;
each arm reduces over it. **Risk: low-medium** — counting rules differ subtly per tool
(`==>` headers count as files in read/chain but not as entries; `(empty)` skipped only in
find/ls). Keep the classifier per-tool-callable with flags.

### 1.5 `skills.rs` — duplicated frontmatter parser and cache plumbing

- `skills.rs:160-197` (`discover_skills_fresh_async`) re-implements `parse_skill`'s
  frontmatter parsing inline (comment even admits it) and silently drops the
  invalid-name `eprintln!` warning the sync path emits (35-40) — a behavior divergence.
- `skills.rs:68-93` and `229-252` are two identical 10s-cache implementations.

**Simplification:** `fn parse_frontmatter(content) -> Option<(name, description)>` used by
both paths; unify the warning. Optionally a tiny `TimedCache<T>` helper. **Risk: low** —
parity tests (`skills.rs:370-427`) assert async ≡ sync.

### 1.6 `main.rs` — duplicated session-open logic; `main()` Serve arm

- `main.rs:99-107` and `162-184` duplicate the "open `Session::new` vs
  `open_or_continue` based on `--new`/`--session`, warn on failure" ladder → extract
  `open_session(args, cwd)`.
- `main()` is a 200-line match; the `Serve` arm (357-393) is a self-contained
  `fn run_serve(bind)` (~40 lines out of main, isolates socket parsing for unit testing).
- `Mode::Mcp` arm (500-542): three near-identical "print Ok / eprintln+exit / usage on
  None" blocks → `fn run_or_exit(Res<String>, usage)`.

**Risk: low.** The shell-escape path keeps its different persistence semantics (`!!`
exclusion); the shared helper only covers opening.

### 1.7 `cli.rs` — at most one merge

`cli.rs:196-205`: last two match arms produce identical `Mode::OneShot` output; can be one
arm `Some(_)`. Everything else (`check_reattach_mode` whitespace-prompt subtlety at
236-241, `-H` prefix checks) is healthy or load-bearing — **leave it**.

### 1.8 Lower-impact notes

- `apply_edit` (`tools/mod.rs:1087-1181`): two strategies (exact + fuzzy line-window) in
  one function; splitting into `apply_exact`/`apply_fuzzy` would drop depth ~2 — but
  whitespace-inheritance subtleties are thin on tests. Do only with added tests.
- `usage.rs` `chart` (198-313): grid/tick math is genuinely necessary; not worth
  restructuring.
- `update.rs:54-82`: could extract `resolve_tag`, but reads linearly — fine.

---

## 2. `src/llm/` (config, stream, prompt, protocol, providers)

Complexity is concentrated in `config.rs`, and most of it is not essential branching but
duplication of the same 3–4 patterns: file-identity caching, env-var parsing,
catalog-shape scanning, precedence-chain re-derivation in `doctor`.

### 2.1 `doctor()` re-derives the entire `from_env` precedence chain by hand (HIGH impact)

`config.rs:2184-2619` (~440 lines; mirrored chains at 2262-2296 selection/provider
origin, 2324-2353 base_url origin, 2399-2422 protocol origin, 2423-2439 context origin,
2506-2522 effort origin, 2558-2591 headers origin). Every resolution rule exists twice —
including a second hand-rolled `using_builtin_default` five-way condition, the
header-merge layering, and credential/env-var resolution order (2376-2393 explicitly
comments "Mirror `resolve_credentials`"). A change to precedence must be made in two
places or `doctor` starts lying.

**Simplification:** introduce a small `Resolved { value, origin }` per knob and resolve
each knob once in shared helpers used by both `from_env` and `doctor` (e.g.
`resolve_selection(file, model_override) -> (String, &'static str)`,
`resolve_base_url(...)`, `resolve_thinking(...)`). `from_env` keeps the value, `doctor`
prints value+origin. The header layering reuses the same ordered
`[(map, source_label)]` table that `from_env`'s merge loop (1636-1644) walks.

**Risk: medium.** Precedence order and `warn_once` fire-order must be preserved exactly
(deprecated-key warnings fire as a side effect of these chains). Do it as an extraction
(same call sequence) rather than a rewrite; existing doctor tests plus config precedence
tests cover it.

### 2.2 `SseDriver`/`StreamPrinter` sync–async duplication in `stream.rs` (HIGH impact)

`feed_line_inner` (133-164) vs `feed_line_inner_async` (166-197); `feed_raw` (404-456,
`#[cfg(test)]`) vs `feed_raw_async` (461-511); `finish_turn` (513-527, test-only) vs
`finish_turn_async` (529-542); `finish` (200-213) vs `finish_async` (215-230). ~250 lines
where the branch structure is byte-for-byte identical; the sync variant is test-only.

**Simplification:** either delete the `#[cfg(test)]` sync paths and run the parser tests
on a current-thread tokio runtime (`block_on(feed_raw_async(...))`), or keep one
`feed_event(&mut self, event)` containing the match, called from both wrappers (the
wrapper only differs in `try_send` vs `send().await`).

**Risk: low-medium** — sync path is test-only, so behavior unchanged by construction; the
work is converting ~20 driver tests to an async entry point.

### 2.3 File-identity cache pattern triplicated (MEDIUM-HIGH)

`load_config_file` (38-100), `learned_api_map` (578-612), `load_dex_catalog` (942-969),
plus mirrored invalidation (102-106, 641-644). Three independent implementations of the
same stat → `(path, mtime, len)` cache check → read+parse → store → return dance, each
with its own struct.

**Simplification:** one generic
`fn cached_parse<T: Clone>(cache, path, parse: fn(&str) -> Option<T>) -> Option<T>` with
three thin call sites. **Risk: low.**

### 2.4 Repeated env-var parse chains (MEDIUM, cheapest win)

`config.rs:1624-1629`, `1755-1777`, `1778-1785` all do
`env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)` (7+ sites);
`stream.rs:663-667` already has `env_secs()` but private.

**Simplification:** one `fn env_parse<T: FromStr>(name, default) -> T`. Each site drops
from 3 decision points to 1. **Risk: negligible.**

### 2.5 XDG-vs-HOME path resolution duplicated five times (MEDIUM)

`config_file_path` (14-22), `learned_apis_path` (558-564), `thinking_path` (682-688),
`dex_catalog_cache_path` (796-801), `dex_ctx_index_path` (818-823).

**Simplification:** `fn xdg_path(env_var, home_sub, rel) -> Option<PathBuf>`. Note the
three different env vars involved (`DEX_CONFIG`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME`) —
keep the env-var a parameter. **Risk: negligible.**

### 2.6 Catalog dual-shape scanning duplicated across six functions (MEDIUM)

`catalog_has_model` (318-333), `catalog_limit` (975-1009), `build_ctx_map` (838-866),
`catalog_endpoint_for_model` (1051-1057), `reasoning_options_for` (654-664),
`load_dex_models_cache` (1149-1188), plus the CI-lookup written twice inside
`catalog_limit` and again in `catalog_cost` (1086-1091). Each walks "api.json providers
shape" then "catalog.json flat models shape" with nested `get().and_then(as_object)`
chains (~6 branches × 6 functions).

**Simplification:** one iterator helper
`fn catalog_models<'a>(catalog) -> impl Iterator<Item = (&'a str, &'a Value)>` yielding
model entries from both shapes, plus `models_get_ci`. Consumers collapse to a single
loop/filter. **Risk: medium** — preserve (a) providers-shape wins over flat shape,
(b) first matching provider in catalog order, (c) CI matching with exact-key preference
in `catalog_limit`'s `lookup`.

### 2.7 `usage_cost` / `cache_write_read_ratio` share an identical 3-tier cost lookup (MEDIUM)

`usage_cost` (1109-1143) vs `cache_write_read_ratio` (2041-2071): the exact same
`catalog_cost(endpoint-exact).or(provider-keys).or(any)` chain and "missing rate bills at
input" assumption.

**Simplification:** `fn resolve_model_cost<'a>(...) -> Option<&'a Value>` used by both.
**Risk: low** — tests at 2700-2860 pin the tier order.

### 2.8 Routing-prefix logic in three places (MEDIUM, needs care)

`split_selection` (262-286), `strip_routing_prefixes` (1910-1924), `apply_model`
(1933-2022, esp. 1944-1976). All three do "split_once('/'), is the prefix a known
provider/endpoint?" with slightly different outcomes; `apply_model` alone is ~90 lines.

**Simplification:** a pure classifier
`fn classify_selection(sel, known, endpoints) -> SelectionRoute` consumed by all three;
`apply_model` keeps its side effects. Also `output_item.added`/`output_item.done`
function-call handling in `stream.rs:1033-1055` and `1087-1107` is a verbatim ~20-line
duplicate → one `handle_function_call_item`.

**Risk: medium** — routing semantics are the subtlest behavior in the file (explicit
`base_url` pins must never re-route; provider-native slashes like `moonshotai/kimi` must
survive). Do this refactor last, only with `apply_model_routes_prefixed_selection_to_endpoint`
green.

### 2.9 Header-parsing/merge layering (LOW-MEDIUM, modest only)

`parse_headers_str` (1338-1402), `config_headers_map` (1483-1506),
`load_config_headers` (1512-1534), `custom_headers_from_env` (1540-1556), merge loops in
`from_env` (1636-1644) and `doctor` (2560-2581).

**Simplification:** funnel `config_headers_map`'s mapping/text/sequence arms through one
`merge_one(out, &serde_yaml::Value)`; extract `parse_json_headers`. **Don't** unify the
grammars — the JSON-fallback-to-pairs and newline-vs-comma rules (1381-1387) are
load-bearing compat behavior.

### 2.10 Fine as-is

- `from_env` (1618-1861): after 2.1/2.4/2.5 stays ~150 lines of deliberate
  sequential narrative with warn ordering — optional polish only.
- `stream.rs` parsers (`ResponsesParser::feed`, `AnthropicParser::feed`): inherent
  protocol mapping.
- `protocol.rs::tools_schema` (32-240): huge but branch-free data literals.
- `prompt.rs` (54 lines, 2 branches) and `anthropic.rs`: nothing to do.
- `streaming.rs::try_responses_fallback` (64-84): flat guard chain is the *clearest*
  encoding of the fallback gate — do not "simplify".

**Estimated total:** ~300-400 lines and ~60-80 branch sites removed from `config.rs` and
`stream.rs` combined; items 2.4/2.5/2.7/2.3 near-zero risk.

---

## 3. `src/agent/` + `src/session.rs`

### 3.1 `process_turn` is a ~700-line single function (`agent/loop.rs:315-1033`)

One `for _iteration in 0..1_000_000` loop drives everything: persistence cursor,
steering drain, obs-pack projection, compaction gate, LLM call, overflow retry,
stop-reason reporting, tool-batch scheduling, per-result processing, tool budget,
tool-cache write-through. Control flow nests 5–7 deep. Duplication smells:

- **Post-compaction session rewrite, 3 copies** (`loop.rs:437-443`, `509-515`,
  `902-908`): all do `session.clear_messages()?; for message in messages.iter().skip(1)
  { session.append_message(message)?; } persisted_cursor = messages.len();` → extract
  `fn rewrite_session(...)`. This is the one place the incremental-journal invariant
  could drift between copies — single-sourcing it is a *correctness* win.
- **Sink/no-sink emit duplication, 3 copies** (stop-reason notes `535-566`, tool-input
  `681-700`, tool-output `820-850`): each hand-rolls
  `if console.sink().is_some() { emit_async } else { with_console(eprintln) }` with
  duplicated message strings. Generalize `system_note` (`loop.rs:31-37`).
- **Stop-reason mapping** (`535-566`): sink and non-sink arms each re-`match
  turn.stop_reason`; compute one message first, emit once.
- **Steering drain, 2 copies** (`368-385`, `1010-1026`): extract
  `drain_steering(...)` + `inject_steering(...)`.

**Suggested extractions:** `compaction_gate(...)` (the `while compaction_attempts < 3`
block, 411-463), `run_tool_batch(...)` (serial/JoinSet scheduling, 580-638),
`process_tool_result(...)` (per-call cache/guard/reducer/boundary/emit, 655-951), plus
the three helpers above. Each is a pure move of existing code.

**Risk: low-medium.** Move verbatim: the compaction gate (`need_by_tokens = eff >
config.compaction_threshold()`, count fallback `messages.len() > 1 +
KEEP_RECENT_MESSAGES`, `compaction_attempts < 3`), the `archivable_tokens`
pre-measurement ordering (measured *before* `compact_history` rewrites `messages`,
426-428), and the one-retry `overflow_retried` flag. Tests `context_overflow_compacts_and_retries_once`,
`online_compaction_compacts_at_an_economical_boundary`, `repeated_tool_guard_*` catch
drift. `process_tool_result` needs many borrows — pass a small context struct or return
an outcome enum.

### 3.2 `compact_history` — LLM-vs-deterministic and split-turn branches duplicated (`agent/compaction.rs:551-733`)

The `summarize_old_messages` match (636-663) has three arms, two calling the same
`deterministic_summary(...)`; the split-turn block re-does a second LLM call with its own
error fallback (667-690); both paths end with a near-identical `compute_file_lists` +
`format_file_operations` + `format!` tail (692-707).

**Simplification:** extract `async fn llm_summary(...) -> Result<(String, Option<Usage>), String>`
covering 634-716 — keep the cancellation-string check (652-655) byte-identical since
`compact_history`'s `Err(e) if e.contains("cancelled")` consumers (`loop.rs:460`) depend
on that wording. Fold the duplicated "append file section" tail into
`attach_file_section(summary, file_ops)`. The final splice (726-731, "first summary
wins" healing) stays untouched.

**Risk: medium-low.** Output strings must be character-identical (`history_summary.trim()`
vs untrimmed differs between split and non-split arms, 694-707 — preserve that
asymmetry); tests `repeated_compaction_keeps_a_single_summary` /
`stacked_summaries_heal_to_a_single_summary` verify. Emergency flags (570-580) unchanged.

### 3.3 `find_cut_point` — split-turn computation and tool-backup loop duplicated (`agent/compaction.rs:60-164`)

The trailing block (130-157) recomputes exactly what 119-125 already computed
(`is_turn_start_message` + `find_turn_start_index` + `is_split`), and the
`while … role == Role::Tool { idx -= 1 }` backup appears twice (114-116, 137-140).

**Simplification:** extract `cut_point_at(messages, idx, start) -> CutPoint` (114-125 +
constructor) and `back_up_over_tools(messages, idx, start) -> usize`. The function then
reads: compute candidate → maybe adjust for last-user → build `CutPoint` once.

**Risk: low** — tests `cutoff_never_lands_on_a_tool_message`,
`cutoff_preserves_last_user_prompt`, `emergency_compaction_cuts_below_the_comfort_floor`
pin the behavior.

### 3.4 Four near-identical `.jsonl` directory scans (`session.rs:449-468, 472-500, 508-532, 1008-1054`)

`list`, `list_all`, `list_children`, and `list_all_async`'s spawned closure each:
`read_dir` → filter `.jsonl` → `read_first_line` → parse `SessionHeader` → collect →
sort by timestamp desc. The comment "Only the header is needed; session files grow
large" appears twice verbatim.

**Simplification:** one `fn scan_jsonl_dir(dir) -> Vec<(PathBuf, SessionHeader)>` +
`sort_newest_first`; `list` = scan one dir; `list_all` = scan each subdir;
`list_children` = scan + enrich; async path calls the same helper. **Risk: low** —
`list_children_reports_runs_and_interrupted_state`, `parent_listing_and_loader_ignore_child_sessions`
pin it. Collapses ~150 lines of copy-pasted IO scanning (with 3.5).

### 3.5 Three+ hand-rolled streaming line scanners with the same shape (`session.rs:748-847, 1070-1098`)

`load_events`, `max_event_seq`, `last_turn_state`, `load_session_state` each open a file,
`BufReader::read_line` in a `loop { line.clear(); match … }`, substring-prefilter, then
parse and reduce. `load_events` handles `Interrupted`; the other three use lenient
`Ok(0) | Err(_) => break` — an inconsistency worth noting, not necessarily changing.

**Simplification:** `fn for_each_line(path, f: impl FnMut(&str))` owning the loop and
the `Interrupted` retry; each scanner shrinks to filter+fold. Keep the load-bearing
substring prefilters (documented JSON-escaping argument) inside each closure.
**Risk: low** — `events_journal_replays_after_seq_cursor`, `last_turn_state_tracks_terminal_entries`,
`session_state_last_write_wins_and_ignores_other_entries` cover all three.

### 3.6 Manual `ToolState` reconstruction for save (`agent/loop.rs:982-998`)

When `state.dirty`, builds a fresh `ToolState` field-by-field (12 fields), deliberately
substituting `obs_projection: ProjectionState::new()`. `state.rs:1` even disables
`unused_variables`/`dead_code` at module level, so a forgotten field would not warn.

**Simplification:** derive `Clone` on `ToolState`; `let mut to_save = state.clone();
to_save.obs_projection = ProjectionState::new(); to_save.save_async().await;`
**Risk: low.**

### 3.7 Sync/async load+save duplication in `ToolState` (`agent/state.rs:110-186`)

`load`/`load_async` and `save`/`save_async` are two structurally identical pairs
(std vs tokio fs); the `DEX_TOOL_CACHE` gate and best-effort policy restated 4×.

**Simplification:** keep `load`/`save` as the single implementations; implement
`load_async`/`save_async` as thin `spawn_blocking` wrappers (call frequency is once per
turn teardown, blocking is harmless). `cache_fingerprint` (50-70): `let … else` cascade,
cosmetic. **Risk: low** — gate semantics (opt-in, dirty-only writes) must stay.

### 3.8 `is_context_overflow` — 13 chained `contains` calls (`agent/loop.rs:113-132`)

**Simplification:** `const OVERFLOW_PHRASES: &[&str]` iterated with `.any(...)`, keeping
the "reduce the length" + anchor pair as a separate check. Test
`overflow_wording_is_recognized` (1470-1488) pins every phrase including negative cases —
leave the phrase set untouched. **Risk: trivial.**

### 3.9 `for _iteration in 0..1_000_000` — dead bound masquerading as a limit (`loop.rs:362, 1032`)

Real bound is the tool budget (`tool_iterations >= tool_budget` → early return,
960-973); the 1M cap and trailing `Err` at 1032 are effectively dead unless
`DEX_MAX_TOOL_ITERATIONS > 1_000_000`.

**Simplification:** `loop { … }` with the existing budget check (keep the trailing `Err`
for exhaustiveness), or document the backstop. **Risk: trivial** (clamp the env value if
keeping the cap matters).

### Things NOT to touch in agent/session

- **Compaction trigger** (`loop.rs:411-463`): intricate but each piece load-bearing and
  commented. If extracting, move verbatim; do not "simplify" the retry count or the
  `archivable_tokens` pre-measurement ordering.
- **Journal markers**: `Session::turn_event`/`append_line` durability sniffing
  (`session.rs:583-664`) and events-journal `turn_complete`/`turn_failed` sync
  (737-742) are stringly-typed but correct and covered by tests.
- **`repair_dangling_tool_calls`** (`session.rs:937-972`): clever but small, correct,
  triple-tested.
- **`deterministic_summary`** (`compaction.rs:376-500`): long but flat and linear;
  high line count, low cyclomatic risk.

---

## 4. `src/daemon/` + `src/client/` + `src/protocol/`

Verdict: the wire layer is fine; branch density is concentrated in three mechanical
patterns — mutex-lock boilerplate, duplicated per-route plumbing, and two oversized
turn-pipeline functions. ~half of server.rs's ~136 non-test branch sites are removable;
`protocol/mod.rs` has nothing worth touching.

### 4.1 Mutex-lock boilerplate repeated ~45× in server.rs (biggest, cheapest win)

Every access to a `DaemonState` map spells
`state.<map>.lock().unwrap_or_else(|e| e.into_inner())` (representative: server.rs:150-163,
303-306, 546-565, 673-694, 1613-1651, 2120-2124; plus ~15 in `daemon/mod.rs:244-364`).
The poison-recovery closure is a branch site each time — ~⅓ of the module's branch
count, pure repetition.

**Simplification:**

```rust
pub(crate) fn lock_map<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
```

Optionally typed accessors. `timeout_pending` (server.rs:1532-1539) is already exactly
this helper for one map — generalize it.

**Risk: near zero** — mechanical, identical poison-recovery semantics, mutex sections
never span an `.await`. Est. −45 branch sites, −90 lines.

### 4.2 `run_agent_turn` (server.rs:638-865) + `run_turn_inner` (868-1356): ~720 lines, 9/11 params

Mixes six responsibilities. Concrete pieces:

- **Two near-identical spawned forwarder tasks** (`:737-762` steering, `:763-788`
  follow-up): same body, only the `StreamEvent` variant differs (~50 duplicated lines)
  → `spawn_queue_forwarder(tx, state, sid, path, rx, |content| StreamEvent::…)`.
- **`turn_result` declared before the loop, assigned in 3 places, used after** (1264,
  1313, 1327, 1336) — hence `#[allow(unused_assignments)]` twice (867, 1257). Rewrite
  the follow-up chain as `let turn_result = loop { match process_turn(...).await { …
  break Ok/Err } };`.
- **Dead `Option` wrappers:** `steering_accepted_tx`/`followup_accepted_tx` (875-877)
  are always `Some` (single call site 800-812); the `if let Some(tx)` at 1317-1319 is a
  dead branch. Drop the Options.
- **Channel pre-creation/reuse dance** (`chat` 530-568 + fallback 714-731): one helper
  returning `(steering_rx, followup_rx)` instead of inline dual-map locking at both sites.
- **9/11-argument signatures** (`#[allow(clippy::too_many_arguments)]` at 637, 867):
  bundle into a `TurnChannels`/`TurnCtx` struct.

**Risk: medium** — turn lifecycle (terminal-event-must-be-last invariant, guard drop
ordering 820-824, idempotency recording 861-863). Do the mechanical pieces; leave the
sequencing block untouched. e2e tests (`server.rs:3738+`, `client_end_to_end_hits_every_endpoint`) cover it.

### 4.3 `steer`/`followup`/`recall` are three copies of one handler (server.rs:1795-1874)

Identical sequence: trim → empty = 400 → `lookup_entry` = 404 → lock tx map = 409 →
send failure = 409 → ok JSON. ~80 lines for one behavior; deltas are which mutex and
`QueueMsg` variant.

**Simplification:** `fn queue_tx(state, sid, followup: bool) -> Option<Sender<QueueMsg>>`
+ three ≤10-line handlers (recall's `if req.followup` map selection at 1862-1866 shares
it too).

**Risk: very low** — but the **status-code order** (400 before 404 before 409) is the
wire contract; tests at 2467-2558, 2852-2917 assert exactly these codes. Est. −10
branch sites, −45 lines.

### 4.4 Five copies of "journal one event to the session's `.events.jsonl`"

Terminal journaling 843-858; sink bridge 1101-1103, 1191-1193; accepted forwarders
748-758, 774-784; child-approval bridge 1482-1495. Each does
`sessions.lock().get(sid).map(path)` → `Session::from_path` → `journal.append_event(seq, …)`
with best-effort error tolerance.

**Simplification:** `fn journal_event(state, sid, seq, event)` in `daemon/mod.rs` next
to `broadcast_event` (natural sibling: journal + broadcast). One caveat: writes are sync
file IO on async tasks today — preserving that is behavior-preserving; do **not**
silently switch to `spawn_blocking` in the same commit. Est. −35 lines, 5 sites → 1.

### 4.5 `chat` idempotency-replay chain + wake/claim nesting (server.rs:484-611)

**Simplification:** flatten the nested `if let Some(key)` / `if let Some(terminal)`:

```rust
let replay_envelope = idempotency_key.as_deref()
    .and_then(|k| state.idempotent_replay(k, &session_id, request_hash))
    .and_then(|t| serde_json::from_str::<StreamEnvelope>(&t).ok());
```

`steal_wake_and_claim` (1920-1945) restructures into a `match try_claim_slot(...)` with
early returns. **Risk: low** — replay must not claim the turn slot (ordering 521-536
stays). Est. −5 branch sites.

### 4.6 `list_sessions` sorts twice because it built JSON first (server.rs:390-482)

Sorts typed tuples by header timestamp (432), flattens into a `HashMap` (393, 435)
losing order, then re-sorts by re-extracting `created_at` strings out of serialized
JSON (469-480).

**Simplification:** sort the typed tuples once (timestamp desc, tie-break by id for
determinism), then map to `json!`; drop `by_id`; concat the in-memory fallback (450-468)
before the single sort. Output values identical; tie ordering becomes deterministic
instead of HashMap-random. **Risk: low.**

### 4.7 Hand-rolled enum↔string matches (server.rs:190-195, 1712-1725)

4-arm `PermissionMode → &str`, 3-arm protocol→core `ApprovalDecision` immediately
followed by 3-arm core→`&str`, plus a duplicated full `DaemonInfo` literal in the `Err`
fallback (202-226).

**Simplification:** `as_str()` on `PermissionMode` / `From<protocol_::ApprovalDecision>`
in `src/core/types.rs` (snake_case strings are already the serde wire spelling). The
Err fallback shares a `DaemonInfo::default_for(cwd, …)` constructor.
**Risk: low** — strings must match wire values exactly ("read-only"/"ask-writes"/"ask-shell"/"trusted",
"once"/"session"/"deny").

### 4.8 `client/http.rs`: 14 endpoint methods × sync wrappers × three error-coercion spellings

20 copies of the same `self.http.post(format!("{}/api/sessions/{}/{tail}", …)).headers(self.api_headers())…`,
14 `pub fn X { block_on(self.X_async(…)) }` twins, and inconsistent coercions at three
spellings (`map_err(|e| -> Box<dyn Error> { e })` at 351/609 vs
`e.to_string().into()` variants at 498/874) — some methods lose the error chain.
`list_sessions_async`/`list_skills_async` are the same lenient-array extraction twice;
`events_async` re-implements a third.

**Simplification (all wire-identical):**
1. `fn session_url(&self, sid, tail) -> String` + `fn post_json<T: Serialize>(...)`.
2. One `fn boxed_err(e: impl Display) -> Box<dyn Error>`; standardize on preserving the
   error chain (strictly better diagnostics; callers only match on message substrings).
3. `fn lenient_array<T: DeserializeOwned>(v, key) -> Vec<T>`.

The sync/async twin pattern itself is deliberate (CLI/repl sync vs TUI async) — keep it,
stop hand-writing the plumbing. `SseFramer` (146-224) is well-factored — leave it.
Est. −150 lines, zero wire change.

### 4.9 `protocol/mod.rs` — no action

368 lines of serde type definitions; the 17-variant `StreamEvent` enum is the wire
contract, not accidental complexity. `#[serde(default)]` discipline and unknown-variant
tolerance on both ends (server.rs:1969-1971, http.rs:181-198) is the compat mechanism —
already done. Do not "simplify" the enum or non-wildcarded match arms
(`client/repl.rs:54-147`): exhaustiveness is a feature.

### Deliberate non-findings (keep)

- `require_bearer` (server.rs:39-72): hand-rolled constant-time compare; `subtle` would
  be a new dep against project rules.
- `TurnGuard`/`ShellGuard` Drop impls (652-699, 2128-2140): RAII is the simplest correct
  form (panic-safe teardown).
- Sink-bridge coalescing loop (1126-1142): subtle (Thinking joins verbatim, Assistant
  joins with `\n`, foreign line deferred) — touching risks transcript-corruption
  regressions.
- Tests (server.rs:2258-4426) inflate absolute if/match counts; judge density on the
  non-test half.

---

## 5. `src/mcp.rs` + `src/mcp/oauth.rs`

Overall in good shape for its size — config parsing and helpers already factored — but
four duplication clusters plus two high-CC functions that split cleanly, and **two
latent correctness bugs**.

### 5.1 OAuth: `exchange_code` and `refresh_access_token` are the same ~45-line token-POST flow

`oauth.rs:655-701` vs `705-749`. Both: `ensure_https_or_loopback` → build `pairs` →
identical conditional `client_secret` dance (672-676, 717-721) → POST
`application/x-www-form-urlencoded`, `accept: application/json`,
`timeout(DISCOVERY_TIMEOUT_SECS)`, `body(form_body(&pairs))` → status check →
`resp.json()` → `token_from_response`. Only pair lists, context strings, and refresh's
`invalid_grant` special case (733-735) differ.

**Simplification:** extract
`async fn post_token_form(http, endpoint, context, pairs, secret) -> Result<Value, String>`
owning the https check, headers, timeout, send, status→text error, JSON parse; callers
keep their context strings and the `invalid_grant` sniff. `register_client` (572-617)
shares the JSON-POST shape for a later `post_json` sibling. ~30 lines removed.

**Risk: low** — local-mock tests (`oauth.rs:1326-1452`) cover both flows.

### 5.2 `initialize` payload hand-built in three places (drift hazard)

`mcp.rs:670-674` (stdio), `mcp.rs:796-800` (HTTP session-gone re-handshake), and
`oauth.rs:908-914` (`probe_challenge`, which also re-builds the full
`{"jsonrpc","id","method","params"}` envelope that `rpc_request` at `mcp.rs:589-591`
already provides). The `protocolVersion: "2024-11-05"` constant and `clientInfo` live in
three literals; the HTTP re-handshake already differs subtly (ignores the result).

**Simplification:** `fn initialize_params() -> Value` next to `rpc_request`; reuse
`rpc_request(0, "initialize", initialize_params())` in `probe_challenge`.
**Risk: low** — byte-identical JSON out.

### 5.3 "Does this tool belong to server X" filter written three times

`mcp.rs:1254-1257` (`connect_and_cache` retain), `1277-1284` (`reconnect` count),
`1338-1345` (`status_list` count). All re-derive the
`name == "mcp__{server}_read_resource" || name.starts_with("mcp__{server}__")` shape;
the synthetic-name string is constructed in four places (509, 1255, 1281, 1342) and the
`"\0resource"` sentinel in two (227, 1237). The tool-name grammar is the module's core
invariant, spread over 5+ sites.

**Simplification:** `fn def_belongs_to(def_name, server) -> bool` +
`fn resource_reader_name(server) -> String` (reused in `resource_reader_definition` and
`cache_server_into`). **Risk: low** — `tool_names_roundtrip`,
`name_collisions_rename_instead_of_shadow`, `statuses_*` pin behavior.

### 5.4 `McpClient`: `__mcp_error` check + array unwrap + output clamp repeated

Error check duplicated at 989-991 (`list_tools`), 1032-1034 (`call_tool`),
1056-1058 (`read_resource`); the `and_then(Value::as_array)...unwrap_or(&[])` ladder at
993-997 and 1062-1066; the output clamp at 578-581 (`content_to_text`) and 1082-1085
(`read_resource`). The `{"__mcp_error": err}` sentinel from `extract_rpc_result`
(929-934) is only safe if every consumer remembers to check it — a fourth method that
forgets leaks the sentinel to the model.

**Simplification:** `fn rpc_error(v) -> Option<String>` + `fn json_arr<'a>(v, key) -> &'a [Value]`
+ `fn clamp_output(text) -> String`; the sentinel check becomes structurally
unavoidable. **Risk: low.**

### 5.5 `HttpTransport`: reply extraction and inline 401-refresh arm

`request_inner` (786-832, CC ≈ 10) embeds 20 lines of refresh/backoff/clear inline in
the `unauthorized` arm → extract `async fn refresh_and_retry(&self, method, params)`.
Reply parsing (`roundtail`, 906-924): "try whole-body JSON, else scan SSE `data:` lines"
is one concept in two blocks both ending in `extract_rpc_result` → extract
`fn extract_reply(text) -> Option<Value>` (first parseable object with
`result`/`error` wins; `[DONE]`/blank skip preserved). The stdio (717-733) vs HTTP
(834-926) demux split is inherent — leave it.

**Risk: low** (refresh) / low-medium (`extract_reply`).

### 5.6 `load_server_configs` re-implements `parse_mcp_servers`' entry loop

`mcp.rs:370-401` (JSON env path) duplicates `335-350` (YAML path): reserved `__` check →
`parse_server_config` → `active_config` gate → insert sanitized → warn. The JSON path
additionally round-trips each value through `serde_yaml::from_str(&cfg.to_string())`.

**Simplification:** shared
`fn insert_server(out, display, raw: &serde_yaml::Value)`. Preserve the nit: JSON path
warns with the raw name (390), YAML path with the sanitized name (348) — pass the
display name explicitly. **Risk: low.**

### 5.7 `oauth::login` is a 110-line function, CC ≈ 15

`oauth.rs:943-1052`: config lookup → probe → resource metadata → issuer/scope → AS
metadata → known-client resolution (979-1003) → loopback bind → PKCE → `invalid_target`
retry loop (1010-1030) → exchange → save → reconnect formatting (1042-1051).

**Simplification:** extract `resolve_client(cfg, asm, server, http)` (979-1003) and
`authorize_and_exchange(...)` (1005-1040). Keep the
`Err(e) if e.contains("invalid_target")` retry exactly.

**Risk: low** — no direct test on `login` (network/browser); behavior-preserving text
moves only.

### 5.8 `ephemeral_line` vs `cached_statuses`: same triple `try_read` + snapshot

`mcp.rs:1498-1512` vs `1527-1538` → extract
`fn try_snapshot(mgr) -> Option<(clients, tools, down guards)>`. Keep the
`configs.is_empty()` early return in `ephemeral_line`'s caller only (so
`cached_statuses` still returns `Some(vec![])` for zero-config managers).
**Risk: low.**

### 5.9 `redact_line`: CC ≈ 15, byte-scan nesting, and an offset-desync **bug**

`mcp.rs:431-468`: nested `for marker` / `while find` / whitespace-skips / `is_bearer`
special case / `min()` accumulation.

**Simplification:** extract `fn secret_value_start(line, marker) -> Option<usize>`;
`redact_line` becomes `SECRET_MARKERS.iter().filter_map(...).min()` + the tail
`format!("{}[redacted]{}", ...)`; CC of the top function drops to ~4.

**Correctness note:** `let lower = line.to_lowercase();` at 432 is Unicode-aware (e.g.
`İ` expands to 2 chars), desyncing byte offsets computed on `lower` from indexes applied
to `line.as_bytes()`. Use `to_ascii_lowercase` (markers are ASCII; behavior on ASCII
input identical) — the same fix already applied in `oauth.rs:198-200`.

### 5.10 `expand_env`: CC ≈ 12, duplicated arms, and a non-ASCII mojibake **bug**

`mcp.rs:106-146`: the `match std::env::var(key)` → push / `Err => return Err(...)`
block is duplicated verbatim for `${..}` (115-120) and `$NAME` (131-136) → local closure
`resolve`.

**Correctness note:** `out.push(bytes[i] as char);` at 142 treats each raw byte as a
Unicode scalar, so non-ASCII text around `$VAR` gets mojibake'd (`café$X` →
`cafÃ©<value>`). Config values (`env:`, `headers:`, `args:`) pass through this. Fix by
iterating `char_indices` over chars. This is a bug fix, not behavior-preserving; tests
(`env_vars_expand`, 1633-1640) only cover ASCII — add a non-ASCII case.

### 5.11 Transport trait boxed-future boilerplate — leave alone

`mcp.rs:596-602, 692-697, 774-782, 1554-1561`: manual boxing forced by dyn-compat on
edition 2021; a macro saves ~6 lines × 3 but adds indirection. The real transport
duplication (stdio id-loop vs HTTP session/SSE) is inherent and already isolated behind
`McpTransport`.

### Judged fine (no action)

`split_mcp_name` (213-231, CC ≈ 8), `is_bearer_challenge` (`oauth.rs:306-324`),
`parse_resource_metadata_url` (196-229), `host_is_loopback`, `human_expiry`'s span
ladder, `McpClient::call`'s cancel match (964-972), `content_to_text`'s 4-arm content
match (only the trailing clamp should be extracted — see 5.4).

**Estimated total:** ~130 lines of production code removed; worst-CC functions
(`login` 15→~5, `redact_line` 15→~4) with low regression risk.

---

## The two bugs (worth fixing regardless)

1. **`src/mcp.rs` `expand_env`** — `out.push(bytes[i] as char)` mojibakes non-ASCII
   around `$VAR` expansions. Fix: iterate `char_indices`/chars. (§5.10)
2. **`src/mcp.rs:431` `redact_line`** — `to_lowercase()` byte-offset desync can corrupt
   characters around secrets. Fix: `to_ascii_lowercase`. (§5.9)

## Do NOT touch (explicit no-go list)

- `resolve_workspace_path` (`tools/mod.rs:65-89`) + `lexical_normalize_fallback`
  (110-137) — symlink-confinement boundary; "obvious" simplifications break new-file
  creation or reintroduce symlink escape via unresolved `..`.
- `enforce_policy` (`tools/mod.rs:1457-1539`) — the match/early-return ladder *is* the
  documented permission matrix.
- `atomic_write` (`tools/mod.rs:929-971`) — every branch is a durability requirement.
- `compaction_threshold()` / `tokens > contextWindow - reserveTokens`
  (`llm/config.rs:2026`) — the compaction trigger math in `loop.rs:411-463`
  (`compaction_attempts < 3`, `archivable_tokens` pre-measurement order, one-retry
  `overflow_retried`).
- Config precedence semantics and `warn_once` fire-order (`llm/config.rs`).
- Journal marker semantics (`session.rs:583-664`, 737-742).
- `parse_headers_str` grammar branches (`llm/config.rs:1381-1387`).
- `streaming.rs::try_responses_fallback`, `require_bearer`, `TurnGuard`/`ShellGuard`
  Drop impls, sink-bridge coalescing loop, `SseFramer`.
- `protocol/mod.rs` `StreamEvent` enum and non-wildcarded event matches.

## Suggested attack order (behavior risk ascending)

1. **Trivial helpers:** 2.4 env-parse, 2.5 XDG-path, 4.1 `lock_map`, 3.8
   `OVERFLOW_PHRASES`, 3.9 loop bound, 3.6 `Clone` on `ToolState`.
2. **Pure-dedup refactors:** 1.1 `clip_chars`, 1.5 skills parser, 2.7 cost lookup,
   2.3 cache pattern, 3.4/3.5 session scans, 5.2/5.3/5.4 MCP helpers, 5.6, 4.3 queue
   handlers, 4.7 enum↔string impls, 4.6 list_sessions single sort, 4.5 replay chain,
   4.4 `journal_event`.
3. **Mechanical but guarded:** 1.2 single-audit dispatch, 1.3 metadata table, 2.2
   stream sync/async dedup, 3.6/3.7 `ToolState` plumbing, 3.3 `cut_point_at`,
   4.2 forwarder dedupe + `TurnCtx` struct.
4. **The two bug fixes (own PR, called out in the commit message):** 5.9
   `redact_line` (+`secret_value_start` extraction), 5.10 `expand_env` (+`resolve`
   closure, non-ASCII test).
5. **Agent-loop decomposition:** 3.1 (`process_turn` → orchestrator + ~5 helpers),
   3.2 (`llm_summary` + `attach_file_section`).
6. **Highest value, most careful review:** 2.1 `from_env`/`doctor` shared
   resolution-with-origin.
7. **Last, subtlest:** 2.8 routing classifier (only with
   `apply_model_routes_prefixed_selection_to_endpoint` green).

Skip unless metrics are a hard gate: 1.4 (`content_lines`), 1.7 CLI merge, 2.9 header
funnel, 1.8 `apply_edit` split, 5.11 transport macro.

## Verification

For any of the above:

```sh
cargo fmt -- --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

Key guards per area: HTTP e2e (`src/client/http.rs:1271`) exercises every route's status
codes; `server.rs:2409-2435`/`2467-2558` pin the 400/404/409 ordering (findings 4.3,
4.5); compaction/overflow tests in `agent/loop.rs` and `agent/compaction.rs` pin
findings 3.1-3.3; config precedence + doctor tests pin 2.1; format/output tests pin
1.1/1.4; `oauth.rs` local-mock tests pin 5.1/5.7.

**Net estimate across all items:** roughly 900–1200 lines and several hundred branch
sites removed, no capability or wire change, two real bugs fixed on the way.
