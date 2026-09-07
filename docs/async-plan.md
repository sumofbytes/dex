# Async plan — where `dex` should be async, and in what order

Status: implemented (`feat/async-runtime` branch — Phases 1–6 done).
Kept as a record of what was changed and why; the phase details below are
historical. Conventions from `AGENTS.md` apply:
minimal diffs, reuse helpers, no new deps without need, `cargo fmt --check`,
`cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`.

## 0. TL;DR

Make I/O-bound work async; keep CPU/render work sync.

| Priority | Area | Files | Win (latency + resources) |
|---|---|---|---|
| Phase 1 | Provider HTTP + SSE stream | `src/llm/client.rs`, `src/llm/stream.rs`, `src/llm/streaming.rs`, `src/llm/provider.rs`, `src/llm/config.rs` (`LlmConfig.client`, `refresh_models_cache`) | Latency: ~0 median (LLM-bound), cancel 50ms quanta → ~10ms. Resources: biggest — 1 parked thread per LLM call (minutes) freed; one shared async `Client` replaces per-turn `builder()` + blocking pool |
| Phase 2 | Agent turn loop | `src/agent/loop.rs` (`call_client_cancellable`, `process_turn`, tool fan-out, compaction call) | Latency: ~0 (already parallel via threads). Resources: 1 LLM thread + N tool threads per batch → `JoinSet` tasks; no `messages.to_vec` thread handoff |
| Phase 3 | Tools | `src/tools/mod.rs`, `src/tools/fff.rs` | Latency: only real per-turn win — `fanout_read` 10×5ms serial (~50ms) → ~5-10ms concurrent; `bash` 25ms poll avg +12ms → ~10ms. Resources: 2 reader threads per `bash` freed |
| Phase 4 | Daemon turn bridges | `src/daemon/server.rs` (`chat`, `run_agent_turn`, `run_turn_inner`, sink/approval/steering bridges), `src/daemon/mod.rs` (`rebuild`) | Latency: cold-start `rebuild`/`list_sessions` N×10ms serial (50 sess. ~500ms) → ~50ms parallel; cancel 50ms → ~10ms. Resources: 4 bridge threads per turn → tasks |
| Phase 5 | Client + TUI workers | `src/client/http.rs`, `src/client/repl.rs`, `src/ui/remote.rs` (`submit_prompt`, `poll_git_status`, startup), `src/main.rs` (`start_daemon_background`) | Latency: startup already overlapped via thread (~0 further gain). Resources: small — worker thread → task, git poll off UI thread |
| Phase 6 | Journal + config hot path | `src/session.rs`, `src/skills.rs`, `src/agent/state.rs`, `src/core/console.rs` (`TraceWriter`), `src/llm/client.rs` (`provider_log`), `src/tools/mod.rs` (`audit`), `src/core/format.rs` (`git_context`) | Latency: config/catalog miss ~180ms stays (parse cost); per-delta `writeln!` off turn thread. Resources: no thread parks removed, only shorter holds |

Do NOT make async: `src/llm/prompt.rs`, `src/agent/tokens.rs`,
`src/core/format.rs` (pure formatting), `src/ui/view`/`render` (ratatui draw),
`src/cli.rs`, approval-key hashing, diff rendering (`similar`), token estimation,
cut-point search. Marking CPU work async only adds state machines and `Send`
fights for zero latency gain.

## 1. Current state (measured from source)

Runtime today: exactly one tokio runtime (daemon HTTP edge), everything under
it is synchronous:

- `src/main.rs:156-177` — `start_daemon_background` binds std listener, spawns
  a thread that builds a second tokio runtime, then polls `wait_until_ready`
  with blocking HTTP + `thread::sleep(50ms)`.
- `src/daemon/server.rs:479-498` — `chat` moves the whole turn to
  `spawn_blocking(run_agent_turn)`. Comment is honest: session IO, config,
  LLM, tools are all sync and must not run on a worker. That is the thing to
  fix, not the `spawn_blocking` itself.
- `run_agent_turn:538` + `run_turn_inner:740` — fully blocking; per turn spawns
  2 steering/follow-up threads (`636,662`), 1 sink bridge thread (`892`), 1
  approval bridge thread (`1001`), all on `std_mpsc` + `blocking_send`
  (`476,648,674,736,988,1028`) + `recv_timeout(50ms)` (`902`). That is 4
  threads per turn plus whatever the agent loop spawns.
- `src/agent/loop.rs:82-123` — `call_client_cancellable` clones all messages,
  spawns an OS thread per LLM call, polls `recv_timeout(50ms)`. Cancel latency
  is quantized to 50 ms and a thread is parked for the whole TTFT + stream
  (minutes).
- `src/agent/loop.rs:273-313` — tool batches fan out with `thread::spawn` per
  call + `join`. With the 10-file `read` fan-out plus a `bash`, one turn can
  hold 10+ OS threads just waiting on disk.
- `src/llm/client.rs:98-176` — `post_with_retry` uses
  `reqwest::blocking::Response`, `.send()`, `thread::sleep` backoff. Holds its
  worker thread across connect + headers + full stream. `LlmConfig.client` is
  `reqwest::blocking::Client` (`src/llm/config.rs:1204`), built with
  `reqwest::blocking::Client::builder()` (`1351`), which is why
  `get_config`/`get_git`/`list_skills` need `spawn_blocking` wrappers
  (`src/daemon/server.rs:163,186,205`) and the client needs a shared blocking
  pool (`src/client/http.rs:43-55`).
- `src/llm/stream.rs:180-275` — `run_sse` blocks on `BufReader::read_line`
  over the socket. Cancel is checked only between lines via
  `cancel.take_cancelled()`; a stalled chunk stalls cancel too.
- `src/tools/mod.rs:340-399` — `run_bash_with_limits` polls
  `child.try_wait()` + `thread::sleep(25ms)`, plus 2 reader threads per shell
  command. `tool_read`/`tool_write`/`tool_edit`/`fanout_read`/`expand_glob`
  are `std::fs` + blocking `run_bash`. `audit` and `provider_log` do
  sync open+write per tool call.
- `src/tools/fff.rs:25-94` — fff picker init blocks 30 s (`wait_for_scan`),
  `with_picker` holds a sync lock across `grep`/`fuzzy_search` (CPU-bound,
  10 s budget). Correct home is `spawn_blocking`, never inline in an async fn.
- `src/client/http.rs:68-84,178-253` — `wait_until_ready` sleeps 50 ms between
  polls; `chat` blocks its caller thread on `BufReader::read_line` for the
  whole turn and does a nested blocking `approve()` POST from inside the event
  callback.
- `src/ui/remote.rs:225,1390` — startup already overlaps
  `create_session || list_skills` with a manual `thread::spawn`; each turn
  spawns a worker thread that blocks in `client.chat` + `decision_rx.recv()`.
  `poll_git_status:86` runs a blocking `get_git()` (HTTP + 2 git spawns) on
  the UI thread every 2 s — skipped while busy only by convention (`489`).
- `src/session.rs:148-158,508-530,575-610` — append journals keep a cached
  `File` handle (good) but every `append_message`/`append_event` (one line per
  stream delta on the hot path) is a blocking `writeln!` (+ occasional
  `sync_data`) on the turn thread. `list_all`/`list`/`load_messages`/
  `last_turn_state`/`max_event_seq` are sync scans; `rebuild`
  (`src/daemon/mod.rs:185`) runs them serially on one `spawn_blocking`.
- `src/skills.rs:63-94`, `src/llm/config.rs:36-70,330-397,451-492,557-686` —
  `from_env` runs per chat turn (`server.rs:797`) and per `/api/config`; file
  + catalog reads are cached by identity (good) but the misses are sync reads
  of a 4 MB catalog (~180 ms noted in comments). `thinking_map` has no cache
  at all. `ToolState::load/save` (`src/agent/state.rs:83-126`) and
  `cache_fingerprint` (`37-57`) do sync metadata/read per tool result.
- `src/core/console.rs:372-409` — spinner is a thread + sleep; fine, UI-only,
  daemon passes a sink so it is a no-op there. Leave it.
- `docs/mcp-plan.md:98-119` already calls this out: a future MCP HTTP
  transport wants async reqwest; the sync agent loop is the blocker.

 Net cost of staying sync: thread-per-turn + threads-per-tool + threads-per-
 bridge. At 1 idle turn with a 4-tool batch: ~1 (spawn_blocking turn) + 1 (LLM
 worker) + 4 (tools) + 2× readers per bash + 4 (bridges) ≈ 10 OS threads
 parked, all to wait on fds. Cancel/progress granularity is 25–50 ms polls.
 Tail latency adds: sequential startup RTTs, sequential catalog/config reads,
 UI-thread git poll.

 ### Serial vs parallel inventory (the complete list — nothing else is serial)

 | # | Location | Serial today | Parallel shape | Latency saving | Resource saving | Phase |
 |---|---|---|---|---|---|---|
 | S1 | `src/tools/mod.rs:581-599` `fanout_read` | `for path in &paths` → `read_file_numbered` serial, ≤10 files, shared `used` budget with early break | `JoinSet` ≤10 concurrent reads, join + sort to input order, enforce budget on join | Only per-turn latency win: 10×~5ms ≈50ms → ~5-10ms (~40ms best case, SSD/NFS colder = more) | 0 threads today (runs inside one tool thread); no thread saved, just faster | 3 |
 | S2 | `src/daemon/mod.rs:185-202` `rebuild` + `src/daemon/server.rs:1337-1475` `list_sessions`/`session_events` readers | `for (path, header) in list_all()` → `seed_seq` + `last_turn_state` scan serial; same linear scan in list/events/undo/waive/name readers | `JoinSet` (`spawn_blocking` per file, join, sort) under background task; keep `or_insert` merge + live-turn skip | Cold-start only: 50 sess. × ~10ms ≈500ms → ~50ms; steady-state 0 | Shorter `spawn_blocking` hold; no extra threads | 4 |
 | S3 | `src/agent/loop.rs:273-313` tool batch | Already parallel (`thread::spawn` per call + `join`); serial only on `tool_calls_conflict` same-path edits (correct) | `JoinSet` + `spawn_blocking(execute)` (Ph.2), async tools (Ph.3); keep conflict-serialize path byte-identical | ~0 (thread spawn ≈50µs) | N threads → N tasks; the resource win, not latency | 2 |
 | S4 | `src/ui/remote.rs:217-253` startup | Already overlapped (`thread::spawn(list_skills)` \|\| `create_session`); `get_config` before both (dependent on `cwd`, must stay first) | `tokio::join!(create_session, list_skills, events-prefetch)` | ~0 further | 1 thread → task | 5 |
 | S5 | `src/tools/mod.rs:340-399` `run_bash_with_limits` | `try_wait` + `sleep(25ms)` poll + 2 reader threads; `expand_glob:625` shells `find` through it | `tokio::process` + async pipe drain + `select!(wait, timeout, cancelled)` | avg ~12ms per short command; timeout/cancel exact instead of 25ms-quantized | 2 threads per `bash` freed | 3 |
 | S6 | `src/agent/loop.rs:82-123` `call_client_cancellable`, `src/daemon/server.rs:892-928` sink bridge, `src/client/http.rs:68-84` `wait_until_ready` | `recv_timeout(50ms)` / `sleep(50ms)` poll loops around blocking LLM/SSE/socket | `select!(cancelled, complete().await)` / `send().await` / `sleep().await` | cancel/ready granularity 25-50ms → ~10ms (UX only, invisible in turn time) | 1 parked thread per wait freed | 1/2/4/5 |
 | S7 | `src/llm/config.rs:36-70,330-397` catalog + `src/skills.rs:63-94` discovery | Sync reads, cached by identity (hits = mutex bump); miss parses 4MB catalog ~180ms | `from_env_async` + async dir scans; `spawn_blocking` on miss only | Miss latency unchanged (parse is CPU); hits already ~0 | No thread saved on hits; miss off worker | 6 |

 What is NOT a serial/parallel issue (do not touch for perf): provider
 TTFT+stream (`client.rs:98-176`, `stream.rs:180-275`) — `async` vs `blocking`
 reqwest is identical wire time; fff search / `similar` diffs / prompt build /
 token estimate / ratatui draw — CPU-bound, parallelism adds overhead.

 ### Perf budget: latency vs resources

 - Latency: median turn is LLM-bound (seconds–minutes). The sum of all
   parallelizable serial work above is ~50ms per turn + ~450ms cold start.
   Full async buys no median-turn speedup beyond S1+S2+S5 (~50ms + cold
   start); S3/S4/S6 are thread→task swaps with ~0 time delta. If latency
   alone is the goal, do S1+S2 with `std::thread::scope` and stop.
 - Resources: ~10 parked threads per active turn (2MB stack ≈20MB virtual +
   kernel tasks + blocking-pool slots; tokio blocking pool caps at 512).
   Async collapses this to tasks (KBs) on the existing runtime. This is the
   real reason for Phases 1/2/4: concurrent turns and long-streaming turns
   stop scaling threads with fds. Order by resource ROI: Ph.1 (longest park:
   whole LLM stream) → Ph.2 (N tool threads) → Ph.4 (4 bridges/turn) →
   Ph.3 (2 readers/bash) → Ph.5/6 (short holds).

## 2. Target architecture

One tokio multi-thread runtime per process that needs one (daemon already has
it; TUI/one-shot get a small shared one — Phase 5). Rules:

1. Async owns sockets, timers, and fan-out (`reqwest`, SSE bytes, `tokio::fs`,
   `tokio::process`, `tokio::sync::mpsc`, `JoinSet`).
2. `spawn_blocking` owns CPU and legacy blocking: fff search, `similar` diffs,
   catalog parse, sync session scans during migration.
3. Never hold a `std::Mutex` guard across `.await` (daemon state guards are
   short today — keep them short; the one `#[allow(clippy::await_holding_lock)]`
   test guard stays test-only).
4. `CancellationToken` (atomic bool today, `src/core/console.rs:79`) gains an
   async wait: add an `Arc<tokio::sync::Notify>` field (keeps `Clone`);
   `cancel()` stores true + `notify_waiters()`. Sync `is_cancelled`/
   `take_cancelled` keep working for tools; async code uses
   `select!(cancel.cancelled() => …, …)`.
5. Channels: `std::mpsc` → `tokio::sync::mpsc` for sink (`SinkLine`), approval
   (`ApprovalRequest`), steering/follow-up (`String`), worker→UI
   (`WorkerMessage`). Keep bounded capacities (current 256 for SSE is fine);
   `blocking_send` → `send().await`.
6. No new crates. `Cargo.toml` today already has `tokio {rt-multi-thread,
   macros, net, sync}` and `reqwest {blocking, json, stream,
   rustls-tls-native-roots}`. Needed: remove `"blocking"` (keep `"json"`,
   `"stream"`), add tokio `fs, process, time, io-util` (and `"signal"` only
   if the SIGINT handler is ever unified — today `console.rs` uses raw
   `sigaction`; leave it). `futures-core` is already there for the existing
   SSE `Stream` impl (`server.rs:514-533`, already over the tokio mpsc
   receiver — keep `ReceiverStream` as-is; only the senders change from
   `blocking_send` to `send().await`). No `async-trait`:
   use native `async fn` in traits (verified: rustc 1.98.1, edition 2021;
   stable since 1.75) or `-> impl Future + Send` where `dyn` is needed.

## 3. Phased plan

Each phase is independently shippable, tested, and revertible. Order matters:
Phase 1 unlocks Phase 2; Phase 2 unlocks Phases 3/4; Phases 5/6 can overlap
after Phase 1 lands.

### Phase 0 — baseline (no behavior change)

- Add a `docs/async-plan.md` pointer in `AGENTS.md`? No — keep prompt minimal
  per conventions; this file is enough.
- Record baseline: `cargo build --release`, cold-start `ready in …` line,
  `/health` latency, a 10-file `read paths=` batch time, `cargo test` time.
- Add (or reuse) a fake-provider SSE test harness like
  `src/daemon/server.rs:1973+` (`client_denies_write_then_turn_completes`)
  as the regression net for every later phase.

Accept: three checks green (`fmt --check`, `test --all-targets`,
`clippy --all-targets -- -D warnings`).

### Phase 1 — provider I/O goes async (biggest win)

Files: `src/llm/config.rs:1204,1351,912`, `src/llm/client.rs:39-176,178-246`,
`src/llm/stream.rs:180-293`, `src/llm/streaming.rs:68-134`,
`src/llm/provider.rs:110-165`.

- `LlmConfig.client: reqwest::Client` (async). Update the 4 construction
  sites + tests (`config.rs:1818`, `loop.rs:506`, `ui.rs:1022`,
  `render.rs:1482`) and `provider.rs` test helper. The async client is
  `Clone + Send + Sync`: build it once (process-wide shared client like
  `client/http.rs:43-55` today, not one `Client::builder()` per `from_env`
  as `config.rs:1351` does now) and clone per `LlmConfig` — clone is an
  atomic bump, TLS init happens once.
- `post_with_retry` → `async fn`: `.send().await`, `resp.text().await`,
  `tokio::time::sleep` backoff. Keep exact retry semantics (3 retries,
  500 ms × 2^attempt, 401 refresh via `resolve_credentials`, retryable 408 /
  429 / 5xx, `/responses` → completions hint). `provider_log` stays sync
  fire-and-forget (or `tokio::task::spawn_blocking` if it ever shows in
  profiles — it does one small append; leave it).
- `run_sse` → async over `response.bytes_stream()`: buffer bytes, split on
  `\n`, feed the existing `StreamParser`s unchanged (they are pure functions
  over `&str` — no rewrite). Cancel via
  `tokio::select! { _ = cancel.cancelled() => …, chunk = stream.next() => … }`.
  Preserve `MidStreamError` semantics exactly (only mark after output flowed).
- `call_chat_completions` / `call_responses` / `streaming::complete` → async.
  Keep the responses→completions fallback gate (`is_mid_stream`,
  `try_responses_fallback`, `probed_apis`, `remember_learned_api`) identical.
- `refresh_models_cache` (blocking GET today, `config.rs:912`) → async GET.
  `detect_verify_command`, `usage_cost`, catalog parse stay sync (CPU/once).
- Tests: port `stream.rs:632` `sse_response` helper from
  `http::Response → blocking::Response` to `reqwest::Response` (async
  constructor) or feed parser + driver separately so parser tests need no
  HTTP at all. Keep every existing parser test expectation; add one test that
  a stalled stream + `cancel()` resolves without waiting for the next line.

Accept: fake-provider e2e passes; cancel-before-first-delta still falls back
protocol; mid-stream failure still does not duplicate transcript.

### Phase 2 — agent loop goes async

Files: `src/agent/loop.rs`, `src/agent/compaction.rs:300-352,543-701`.

- `ModelClient::complete` → `async fn` (native async-in-trait). `MockModel`,
  `ToolThenAnswer`, and the `LlmConfig` impl follow. No `async-trait` crate.
- Delete `call_client_cancellable`'s thread + `recv_timeout` loop; replace
  with `tokio::select! { _ = cancel.cancelled() => Err("cancelled"), r =
  client.complete(…) => r }`. No message `to_vec` clone beyond what the call
  needs (today's comment at `loop.rs:228` stays true).
- `process_turn` → `async fn`. Steering drains (`try_recv` → `try_recv` on
  tokio mpsc — still non-blocking, same spots). `persist_pending` stays sync
  in this phase (Phase 6 moves it); it is one `writeln!` per message, not per
  token.
- Tool fan-out: keep `tool_calls_conflict` serialize path. Replace
  `thread::spawn + join` with `tokio::task::JoinSet`, each member running
  `spawn_blocking(execute_tool_call)` in this phase (tools go async in
  Phase 3; the JoinSet already removes thread-per-tool management and
  propagates panic as a tool error like today). `TOOL_MUTATION_LOCK` is a
  `std::Mutex`: do NOT hold its guard across `.await` — scope it around the
  dispatch/join only (or switch that one lock to `tokio::sync::Mutex` if the
  serialize path itself awaits). Preserve the 6-entry `last_tools` repeat
  guard, cacheable set, and `state.clear()` on write/edit exactly.
- Compaction: `summarize_old_messages` / prefix summary call sites become
  `.await`; deterministic fallback unchanged. `DEX_COMPACTION_LLM=1` path
  keeps the dead-drop sink pattern (dropped receiver → sends fail silently).
  `compact_history` itself can stay sync except the two `call_llm` awaits —
  make it async for that reason only.

Accept: `process_turn_completes_with_injected_client` + tool-streaming tests
pass as async tests; conflict-serialize test (same-path edits) still serial.

### Phase 3 — tools go async (thin wrappers, sync cores where CPU-bound)

Files: `src/tools/mod.rs`, `src/tools/fff.rs`.

- `execute` / `execute_outcome` → `async fn`. Dispatch:
  - `read`/`ls`/`write`/`edit`/`hash_file`/`change_diff`: `tokio::fs`
    (`read`, `write`, `create_dir_all`, `metadata`). `fanout_read` reads its
    ≤10 files concurrently with a `JoinSet` under the existing shared byte
    budget; per-file errors stay isolated; glob cap (8) unchanged.
    `expand_glob`'s `find` shell-out becomes the async `run_bash` below.
  - `bash`/`git`/`chain`: `tokio::process::Command` with piped stdio,
    async drain of stdout/stderr (replaces the 2 reader threads),
    `tokio::select!` over `child.wait()`, deadline
    (`tokio::time::timeout`), and `cancel.cancelled()`. Keep process-group
    kill semantics (`setsid`/`killpg` on unix). Keep caps (120 s default,
    1 MiB capture, 400-line/32 KiB clamp) and `[exit N]` reporting.
  - `ffgrep`/`fffind`: keep `with_picker` logic, run inside
    `spawn_blocking` (fff owns its threads + lock; 10 s grep budget stays).
    `rescan` test helper unchanged.
  - `audit`: keep sync gated append (already behind `DEX_AUDIT=1`), or move
    to the Phase 6 writer if it shows up in profiles. Do not build an audit
    pipeline speculatively.
- `CancellationSource` stays a sync trait (`is_cancelled`/`take_cancelled`);
  async tools check it before/after awaits and inside the bash `select!`.
  No `async fn` in that trait.

Accept: shell timeout/cancel tests (10 ms kill, exit-code, stderr label),
read pagination/binary-refusal, fan-out isolation, `chain` refusal of
mutating steps — all green as async tests.

### Phase 4 — daemon bridges go async

Files: `src/daemon/server.rs:382-737,740-1115`, `src/daemon/mod.rs:175-236`.

- `chat`: keep the `active_turns`/`cancel_tokens`/steering registration
  exactly, then `tokio::spawn(run_agent_turn(...))` instead of
  `spawn_blocking`. `run_agent_turn`/`run_turn_inner` become async. The two
  steering/follow-up forwarder threads → tasks forwarding
  `tokio::mpsc::Receiver<String>` → `StreamEvent::{Steering,Followup}Accepted`
  + journal. The sink bridge thread (`recv_timeout` + coalesce + journal +
  `blocking_send`) → task on `tokio::mpsc::Receiver<SinkLine>` with the same
  Thinking/Assistant coalescing. The approval bridge (`approval_rx.recv` +
  park in `pending_approvals` + emit `ApprovalRequired`) → task. All emits
  `send().await`.
- Inside `run_turn_inner`: `LlmConfig::from_env`, `discover_skills`,
  `Session::from_path/new`, `load_messages_from_session`,
  `TraceWriter::open` become either async (Phase 6) or short `spawn_blocking`
  calls. Rule: `from_env` cache hits are a mutex bump — call inline; on miss
  it parses the 4 MB catalog — `spawn_blocking` that call only. Session open
  + history load: one `spawn_blocking` each (they stream the file; fast).
  Per-delta journal stays on the Phase 6 writer; turn-boundary markers
  (`turn_start`/`turn_complete`/`turn_failed`) stay synchronous-in-spirit
  (await the writer's flush) so crash semantics do not change: at most the
  in-flight delta is lost, per `AGENTS.md`.
- `get_config`/`get_git`/`list_skills`/`load_skill`: delete the
  `spawn_blocking` wrappers once `from_env`/`git_context`/skills are async
  or proven-cheap; `git_context` (`core/format.rs:870`) itself becomes
  `tokio::process` git spawns under the existing 5 s cache.
  `create_session`/`list_sessions` and the already-`spawn_blocking` readers
  (`session_events`/`session_trace`/`session_undo`/`session_waive`/
  `session_name` at `server.rs:1337-1475`): move the `Session::new`/
  `list_all` + per-session `load_messages`/`last_turn_state` scans into a
  `JoinSet` (`spawn_blocking` per file, join, sort) — fixes the linear scan
  that motivated the background rebuild.
- `rebuild` (`daemon/mod.rs:185`): keep the background task, parallelize the
  per-session `seed_seq` + `last_turn_state` + `turn_failed` marking with a
  `JoinSet`; keep `or_insert` merge + live-turn skip exactly.
- State locks (`sessions`, `pending_approvals`, `active_turns`, …) stay
  `std::Mutex` — sections are short and never cross `.await` (the struct doc
  already promises this; enforce in review, add `clippy::await_holding_lock`
  denies where missing).

Accept: concurrent-turn rejection (`409 CONFLICT`) test, idempotent-replay
test, approval round-trip e2e (`client_denies_write…`), rebuild tests —
unchanged expectations.

### Phase 5 — client + TUI

Files: `src/client/http.rs`, `src/client/repl.rs`, `src/ui/remote.rs`,
`src/main.rs:156-177,243-261`.

- Add an async client alongside the blocking one: `DaemonClient` keeps its
  sync methods (one-shot CLI + repl use them; `client.rs` comment stays true
  for those paths), new `async fn`s (`get_config_async`, `chat_async` as a
  `Stream<Item=StreamEvent>`, etc.) share one `reqwest::Client`. No behavior
  change for sync callers.
- `run_ratatui_repl_with_remote` startup: replace the manual
  `thread::spawn(list_skills)` with `tokio::join!(create_session,
  list_skills, events-replay-prefetch)` on a small runtime handle owned by
  the TUI process (e.g. `OnceLock<tokio::runtime::Runtime>` with 2 workers,
  or reuse the daemon-background runtime thread). Keep the detach-on-error
  semantics (failed launch must not wait for the skills scan).
- `submit_prompt`: the worker thread becomes a task driving `chat_async`;
  approval overlay resolves via `tokio::sync::oneshot`/`mpsc` instead of
  `decision_rx.recv()`; cancel sets `cancel_flag` + POSTs `/cancel` as today.
  `WorkerMessage` crosses a tokio→UI bridge (UI loop stays sync crossterm;
  only the worker side is async).
- `poll_git_status`: move off the UI thread — background task polls every
  2 s, pushes into the existing worker channel; UI loop only applies. The
  daemon's 5 s cache stays; per-frame cost goes to zero even when busy.
- `main.rs`: `start_daemon_background` binds the std listener (no-steal
  preserved), hands it to the runtime thread, awaits health with async sleep
  instead of `thread::sleep`. `Mode::Connect` one-shot keeps the sync client.
- TUI event loop, ratatui draw, slash handling, selection/clipboard: untouched
  and sync.

Accept: TUI cold-start `ready in …` not regressed (target: improves by the
overlapped RTTs); git poll invisible in flamegraph; approval-after-cancel
still auto-denies.

### Phase 6 — journal + config hot path (async persistence)

Files: `src/session.rs`, `src/skills.rs`, `src/agent/state.rs`,
`src/core/console.rs:162-184`, `src/core/format.rs:870`.

- Session journals: introduce a per-session async writer task owned by the
  daemon turn (or a `SessionWriter` handle): `append_message`/`append_event`/
  `turn_event`/`set_state` become `async` (or `send().await` + flush at turn
  boundaries). Keep the wire format byte-identical, keep `sync_data` exactly
  where it is today (turn/effect/terminal lines, `DEX_DURABLE=1`), keep the
  `events.jsonl` vs main-journal split so reattach replay is unaffected.
  Readers (`load_messages_from_session`, `load_events`, `has_messages`,
  `last_turn_state`, `max_event_seq`, `read_first_line`) become async file
  streams or short `spawn_blocking` — pick per call site by size (header-only
  reads: async; full-history loads: `spawn_blocking`).
- Skills: `discover_skills_fresh` → async dir scans (`tokio::fs::read_dir`,
  concurrent `SKILL.md` reads), keep 10 s cache + sort + first-wins + dup
  warning. Explicit `/skill` loads keep bypassing the cache.
- `ToolState::load/save`, `cache_fingerprint`: async fs or `spawn_blocking`;
  `save` stays best-effort, call it from a `spawn` so turn teardown never
  waits on it (today it already does not fail the turn; keep that).
- `TraceWriter::record`, `provider_log`, `audit`: leave sync (single small
  append, off the token path) unless profiles say otherwise. `git_context`:
  async git spawns under its cache (Phase 4).
- `LlmConfig::from_env`: keep the signature for CLI/one-shot; add
  `from_env_async` used by the daemon turn (async file/catalog/learned-API/
  thinking-map reads, same precedence, same errors). Sync wrapper = block on
  the shared handle for the CLI paths that stay sync.

Accept: session round-trip tests (reasoning fields, clear marker,
effect journal, undo ledger) green; crash-recovery test (kill mid-turn →
`turn_failed`, at most in-flight delta lost) passes.

## 4. Cargo + compat notes

- `Cargo.toml`: `reqwest`: remove `"blocking"`, keep `"json"`, `"stream"`,
  `"rustls-tls-native-roots"`. `tokio`: add `"fs"`, `"process"`, `"time"`,
  `"io-util"` (and `"signal"` only if the SIGINT handler is ever unified —
  today `console.rs` uses raw `sigaction`; leave it). No `async-trait`,
  no `async-stream` (the 15-line `ReceiverStream` stays, now over a tokio
  receiver), no new search/fs crates.
- MSRV: native `async fn` in traits needs a modern toolchain; if CI pins older,
  fall back to `fn complete(…) -> impl Future<Output = …> + Send` (RPITIT,
  no crate needed). Either way, no `async-trait` dependency.
- `reqwest::Client` is `Clone`; `LlmConfig` stays `Clone` (client clone is
  cheap). `AuthScheme::apply` (`provider.rs:110`) switches from
  `blocking::RequestBuilder` to async `RequestBuilder` — same header logic.
- Test-only `reqwest::blocking::Client::new()` occurrences (`ui.rs`,
  `render.rs`, `loop.rs` tests, `provider.rs` test) switch to the async client
  or to `display_config`-style fakes; they never touch the network.
- `docs/mcp-plan.md` stays valid: MCP HTTP transport reuses the async client
  from Phase 1; approval elicitation reuses the Phase 4 approval bridge.

## 5. What stays sync (explicit non-goals)

- Ratatui render + crossterm event loop, prompt building, token estimation,
  cut-point search, `similar` diffs, approval-key hashing, YAML/JSON parsing
  of small files, spinner, SIGINT handler.
- `dex run <tool>` / `dex --tool` raw paths: they can `block_on` the async
  tools via the shared handle; no async CLI plumbing needed.
- Audit strictness (`DEX_AUDIT`, `DEX_DURABLE`) semantics: unchanged, only the
  transport may move.

## 6. Verification per phase (all phases)

1. `cargo fmt -- --check`
2. `cargo test --all-targets`
3. `cargo clippy --all-targets -- -D warnings`
4. Fake-provider e2e + cancel + fallback + rebuild tests green.
5. Manual: cold start `ready in …`, one 10-file read batch, one bash-heavy
   turn, `/cancel` mid-stream, reattach replay — compare against Phase-0
   baseline before merging.

Suggested landing order for reviewability: Phase 1 (client/stream) →
Phase 2 agent loop → Phase 3 tools → Phase 4 daemon bridges → Phase 5
client/TUI → Phase 6 journal/config. Phase 1 + Phase 2 without tools already
removes the two longest thread parks (LLM stream, tool batch join) while
tools stay `spawn_blocking`-wrapped.
