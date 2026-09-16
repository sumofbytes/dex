# Twelve Days of `dex`: How a 2,191-Line God File Became a Terminal Coding Agent

> Every coding agent you admire was, at some point, one giant `main.rs` and a
> hardcoded API key. This is the story of twelve days, 140 commits, two
> renames, one architectural epiphany, a written maturity board, a dependency
> purge, and a cold start that went from 780ms to 160ms — and how a weekend
> scratch project called `ak` became `dex`.

---

## Cold open

The first commit in the repository is not "initial commit." It is:

```
03793cb  Fix multiline editor viewport behavior
```

The repo was born mid-project. No README, no tests, no architecture doc. Four
files: a `Cargo.lock` (1,853 lines), a ten-line `Cargo.toml`, a
`config.sample.json` pointing at Moonshot's Kimi endpoint, and the star of the
show:

```
src/main.rs    2,191 lines
```

The crate was called `ak`. The default model was `kimi-k2.7-code`. The render
pipeline was one dependency: `termimad`. That was the whole world — one file,
one model, one renderer, and a TUI bug being fixed before the repo even had a
name.

Twelve days later the numbers look like this:

| | Day 1 | Day 12 |
|---|---|---|
| Rust files | 1 | 39 |
| Lines of Rust | 2,191 | 24,299 |
| Tests | 0 | 220 |
| Merged PRs | 0 | 28 |
| Lockfile deps | — | 348 (down from 399) |
| Releases | 0 | v0.1.1 **and** v0.2.0 — same day |
| TUI cold start | — | 780ms → 160ms |

This post is the play-by-play: what changed each day, which decisions mattered,
and what I'd tell anyone building a terminal coding agent in 2026.

---

## Act 0 — The god file (Aug 25–27)

Day one was already a working TUI agent. The interesting part is what showed up
in the next 48 hours, before any refactor: the instincts that would define the
whole project.

**Day 2 — the agent learns where it is.** Before tools, before providers,
before settings screens, two commits landed:

- `4bb63af` — *feat: include AGENTS.md/CLAUDE.md as project context*
- `3394e11` — *skills discovery*

A coding agent's first instinct shouldn't be more tools. It should be knowing
which repo it's standing in, and reading the conventions the humans left
behind. `AGENTS.md` / `CLAUDE.md` get appended to the system prompt (nearest
parent wins), and a skill became a directory with a `SKILL.md` frontmatter —
`name`, `description`, discovered from the repo and `$XDG_CONFIG_HOME`.

**Day 3 — stop hardcoding providers.** Three commits that sound boring and
were anything but:

- `801a2ba` — *generic Responses API transport* (OpenAI Responses protocol,
  not just Chat Completions)
- `876c2ab` — *fix Codex tool-call streaming* (the protocol quirks begin)
- `3c55848` — *tree-sitter syntax highlighting in the TUI*

By day three the agent spoke two wire protocols and had real highlighting.
The god file was getting heavy — which set up the next day perfectly.

---

## Act 1 — The refactor day (Aug 28)

Seven commits, one wound. The message that names the act:

```
4d6d05e  Refactor god-file main.rs into layered modular architecture
```

Then the interesting details, because "modularize" is where most projects
stop:

- `18e3a66` — *split `ui.rs` into event and slash-command modules* — the TUI
  state machine was already complex enough to deserve its own boundary.
- `220f6b8` — *thread a `Console` through `process_turn` instead of console
  globals.* No singleton. The turn loop received its I/O explicitly, which is
  the only reason the later client/daemon split was even possible.
- `f81e1cf` — the agent loop (`process_turn`) became injectable — a function
  you can call with a fake model, which is what the first tests tested.

LOC after the refactor day: **6,152 across 29 files.** The god file died four
days after the repo did. If you take one scheduling lesson from this story:
**the cost of modularizing is lowest in week one.** Every day you wait, the
god file grows another ligament.

---

## Act 2 — The epiphany, and a crate that couldn't keep a name (Aug 29–31)

### The client/daemon split

Aug 29 is the pivot of the entire project. Three commits, in order:

```
43c07da  Add client-server architecture foundation
f4b0d4a  Add approval flow to the daemon
6e33fb9  Refactor TUI to be pure HTTP client (OpenCode 2 model)
```

The insight, borrowed openly from OpenCode's architecture: **the TUI is not
the app. The TUI is a view.** The agent's brain — turn loop, tool dispatch,
approvals, session journaling — moves into an **axum daemon** that owns state;
the terminal UI becomes a dumb client speaking HTTP + SSE.

Every good thing in the rest of this story is downstream of that one commit:

- **Approvals** became a daemon-owned state machine, so any client (TUI,
  one-shot, future IDE plugin) gets the same permission gate.
- **Reattach**: the daemon holds the session; a killed TUI is just a
  reconnect, not a lost conversation.
- **Crash-safe journaling**: `turn_start` / `turn_complete` / `turn_failed`
  markers hit the JSONL incrementally — a crash loses at most the in-flight
  event.
- **Priced spend**: the daemon knows token counts, so the status bar can show
  real cost regardless of UI.

### The rename saga

```
4e4143a  Rename to oye, unify TUI state, per-session cancellation, native text selection   (Aug 30)
f22da7e  Rename to dex: unified CLI, docs, env prefix                                      (Aug 31)
```

Two renames in 48 hours: `ak` → `oye` → `dex`. Naming is the one refactor that
gets *more* expensive the longer you wait, because the name leaks into env
vars (`DEX_*`), paths (`$XDG_DATA_HOME/dex/`), and habits. Also note what rode
along in `4e4143a`: native text selection. Terminal craft was never a "later"
ticket in this repo.

### And the first PR

Aug 31 also merged **PR #1** — *"Client-server TUI, tool previews, and block
transcript with render-time gutters"* — 301 lines of `HARNESS.md` and 167
lines of `PLAN.md` arrived in the same merge. Which brings us to the part of
the story I care about most.

---

## Act 3 — Write the map, then walk it (Aug 31–Sep 1)

Most side projects die of vagueness. `dex` tried to die of vagueness and was
saved by two files. `HARNESS.md` opened like this:

> The complete map of what a best-in-class coding-agent harness must own. This
> is the product view: every aspect of agent quality, how it is covered today,
> and what world-class looks like — so each can be built up deliberately and
> tracked over time. **Execution order lives in `PLAN.md`; this file is the
> board, not the backlog.**

The board: **20 areas** across five pillars — Cognition, Action, Trust,
Interface, Platform — each scored **0 (absent) to 3 (world-class)** on the day
it was written. The honest scores are the fun part. Verification: **0**.
Goal & task state: **0**. Turn loop: **2**. Terminal UX: **2**.

`PLAN.md` was the execution order, and its principles are worth stealing:

> **Injection over state machine.** Do not rewrite the loop as an FSM. New
> behavior arrives as system-role messages injected at existing points (the
> `WRAP_UP_THRESHOLD` nudge in `agent/loop.rs` is the template).

> **State lives in the daemon, persists via sessions.**

> **One phase ships before the next starts.**

Then the repo simply executed the plan, and — this is the part I keep
re-reading — **the commits carry the phase numbers**:

```
427cda0  harness: P0-P6 intelligence + permission ceiling
0c1d0bc  harness: P5 hardening — compaction token accounting + deterministic fallback
0605298  harness: P8/P9/P10 + task budget — change control, durability, verification gate, traces, protocol+reattach
```

Phase 0 is my favorite because it's the most embarrassing: `Session::set_state()`
was already being called by `/model` and `/provider`, but `load_session_state()`
was sitting behind an `#[allow(dead_code)]`. **State was written and never
restored.** The plan's first checkbox: call it on `/resume`, remove the allow,
and accept the test "resume a session that switched models → model is
restored." That's what a maturity board is for: it turns "it mostly works"
into a list of specific lies you're currently telling yourself.

(Honest epilogue: once the board had been walked and harvested, `a46834f`
deleted `HARNESS.md` and `PLAN.md` as "updated the harness to be free." Maps
are scaffolding. The building is the code.)

---

## Act 4 — Context engineering, the part nobody demos (Sep 2–3)

Sep 2 was a breather — two commits. Sep 3 was 26 commits, and they're almost
all about the one thing that actually separates agents: **what's in the
context window, and when.**

**The search engine.** `3d0233b` promoted `fff` from a side quest to
first-class tools (`src/tools/fff.rs`) — fuzzy file search and fast content
grep that respect `.gitignore`, batched fan-out, truncation caps. Tool output
is where agent contexts go to die; the fix is engine-level, not prompt-level.

**The compaction rewrite.** `f48eb28` says it all:

```
feat: pi-like compaction (keepRecentTokens, split-turn, structured summary,
file ops, last-user pin)
```

Deterministic by default — no extra LLM call to summarize, because the
summarizer is the same model that's already confused. Keep the recent N
tokens verbatim, split the turn cleanly, pin the last user message, structure
the summary, and only fall back to LLM compaction if you ask for it
(`DEX_COMPACTION_LLM=1`). The invariant stayed sacred: compact when
`tokens > contextWindow − reserveTokens`, and never otherwise.

**The budget and the nudge.** `474d30a`:

```
feat: budget 60 + nudge, pi-like contextWindow, models.dev cache, XDG cache
```

The loop knows the iteration budget; before exhausting it, it injects a
system nudge telling the model to wrap up and report. And `contextWindow` is
no longer guessed or configured — it's looked up from the **models.dev
catalog**, cached under XDG, refreshed in the background. One source of truth
for the number that everything else is derived from.

**Cost as a first-class UI element.** `e0c0602` threaded `cached_tokens` from
the provider response all the way to the TUI; `a0d4b81` moved spend pricing
into the daemon so every client sees the same number.

**The quiet breakthrough.** `a1dddb4` — *replay reasoning across tool calls.*
When a model reasons before each tool call, keeping that reasoning attached
across the tool loop keeps the model coherent instead of re-deriving its plan
every step. Nobody demos this in a screenshot. It's the difference between an
agent that wanders and one that walks.

Also on Sep 3, the approval UX grew up: `c1c1b5d` made approval prompts show
the **actual shell command** (readable, quoted), added a modal, and made
approvals persist per-session so you don't re-bless the same command every
turn. And `4f7732c` went env-only for config with `DEX_MODEL_APIS` selecting
per-model wire protocols — config as environment, like everything else in a
terminal-first tool.

---

## Act 5 — The performance war (Sep 4–5)

Sep 4 is 31 commits, and it opens with a confession: **the profiling data was
committed.** `scripts/profile-parca.sh`, the parca runner, and the resulting
CSVs live in the repo under `parca-profiles/`. Not "we made it faster" —
*here are the flamegraphs.*

Then the purge. `3cd3b49` is the most satisfying commit in the repo:

```
build: prune deps; replace dead weight with stdlib/native

Remove tui-textarea, tower-http (0 usages); drop rand (uuid v4 already
present), dirs (12-line cache_dir), async-stream (20-line ReceiverStream),
futures -> futures-core. Slim tokio features to actual usage. ratatui-markdown:
default-features=false ... Replace termimad (49-crate subtree, one print call)
with a small ANSI markdown renderer in core/highlight.rs.

Lockfile: 399 -> 348 entries.
```

Read that again: **`termimad` cost 49 crates and was used for one print
call.** It was replaced by a 111-line ANSI markdown renderer in
`src/core/highlight.rs`. Five whole dependencies were deleted because their
jobs could be done by `std` in a dozen lines each. This is the discipline most
projects skip: dependencies are not features, they're futures you're renting.

The same day, `eca4163` attacked the hot paths with the profile data in hand:
fsync only at turn/effect boundaries, journal envelopes composed in place
instead of re-serializing per streamed delta, consecutive thinking deltas
coalesced into one SSE event per drain window, markdown rendered at most once
per throttle window.

Sep 5 — the two PRs that put numbers on the war:

```
54daa93 (PR #24)  perf(daemon,llm): cut TUI cold start ~780ms to ~160ms
e9b13a7 (PR #25)  perf(warm-start): cache config/catalog/skills/git, share http client, parallelize tui startup
```

The cold-start PR parallelized the startup path. The warm-start PR cached
`config.yaml` + `learned-apis.json` by `(path, mtime, len)`, built a slim
cross-process context index (`models.ctx.json`) that **skips the 4MB
models.dev parse on warm launches**, cached skills discovery for 10s and git
status for 5s per cwd, shared one blocking HTTP client process-wide, and runs
`create_session`/`reattach` and `list_skills` concurrently.

And then the part I love most: `75051eb` — *feat(tui): show ready-in launch
time, millis precision below 1s.* The status bar displays its own cold start.
The perf work had to confess its numbers forever.

Sandwiched between the perf commits, the TUI got the polish users actually
feel: drag-select transcript text (`39103c8`), double-click word selection
(`2c36696`), triple-click lines, composer cursor visibility (`06b96f5`), an
animated thinking indicator, the DEX art banner (`d8e6077`), and honest stop
reasons surfaced from the stream instead of silent truncation.

---

## Act 6 — Ship day (Sep 5, 37 commits)

The last day is release engineering, and it's a checklist you can steal:

1. **Tag-triggered CI builds** (PR #26): musl static Linux via
   `cargo-zigbuild`, macOS x86_64 + aarch64, Windows MSVC — binaries attached
   to the GitHub release with `SHA256SUMS`.
2. **The installer**: `40ab888`, a curl-based `scripts/install.sh` —
   ```
   curl -fsSL https://raw.githubusercontent.com/sumofbytes/dex/HEAD/scripts/install.sh | sh
   ```
   with version pinning and `DEX_INSTALL_DIR` support.
3. **The first fire drill**: `81cd090` — *fix(release): zig install broke
   musl builds — use setup-zig + source-built cargo-zigbuild.* Every release
   pipeline has one; better to have it on day one.
4. **Portability gates**: a Windows-check CI job so cfg-gate rot gets caught
   at PR time, not from a bug report.
5. **Two releases, hours apart.** `6e95f4d` cut **v0.1.1** with the release
   pipeline. Then the provider layer landed as an intentional breaking change
   — `2174691` *generic provider layer with per-provider key deposit*,
   `daddb2d` *feat!: per-provider key deposits replace the OPENAI_API_KEY
   legacy*, `0bb2077` *model selection auto-routes base_url and protocol*,
   `e111605` *per-model thinking effort with a `/thinking` command* — and
   `c462a69` cut **v0.2.0** the same day.

The provider layer is the quiet maturity marker of the whole project. Day one
had a Moonshot URL hardcoded in a sample config. Day twelve has provider
bundles (auth + protocol + endpoints), per-provider key deposits so env
deposits don't cross-contaminate, model ids qualified by endpoint, and
auto-routing from a model name to the right `base_url` and wire protocol.

The repo closed with the work nobody puts on a roadmap: MIT/Apache dual
license, Code of Conduct, CONTRIBUTING, SECURITY — plus 220 tests, clippy at
`-D warnings`, and a CI cache. And one line in the README that should be in
every agent's README:

> **No telemetry** — dex makes network calls only to the LLM providers you
> configure.

---

## The cadence, because it tells its own story

| Day | Date | Commits | What happened |
|---|---|---|---|
| 1 | Aug 25 | 1 | God file born mid-project |
| 2 | Aug 26 | 1 | Project context + skills |
| 3 | Aug 27 | 4 | Steering, Responses transport, Codex fix, tree-sitter |
| 4 | Aug 28 | 7 | **The refactor day** — god file dies |
| 5 | Aug 29 | 7 | **The epiphany** — TUI becomes an HTTP client |
| 6 | Aug 30 | 10 | Rename to `oye`, selection, themes, CI |
| 7 | Aug 31 | 7 | HARNESS.md + PLAN.md, rename to `dex`, PR #1, P0–P6 |
| 8 | Sep 1 | 7 | P5/P8/P9/P10 hardening, reattach, durability |
| 9 | Sep 2 | 2 | fff tools, multi-endpoint config (a breather) |
| 10 | Sep 3 | 26 | Context engineering: compaction, budgets, models.dev, spend |
| 11 | Sep 4 | 31 | **The purge**: deps pruned, hot paths, profiles committed |
| 12 | Sep 5 | 37 | **Ship day**: releases, installer, provider layer, 2 versions |

1, 1, 4, 7, 7, 10, 7, 7, 2, then **26, 31, 37.** The last three days carry
more than half the work — but they were only possible because days 4–8 laid
the architecture the sprint could land on.

---

## What I'd tell you if you're building one

**1. Ship the god file first.** The 2,191-line `main.rs` with a Kimi URL in a
sample config was a *working agent* on day one. You cannot refactor what you
haven't built, and you can't learn what to build from a design doc.

**2. Refactor in week one, not month six.** The refactor day cost seven
commits because the codebase was four days old. Wait a quarter and the same
surgery costs a quarter.

**3. Split the brain from the interface.** The TUI-as-pure-HTTP-client
decision (OpenCode-style client/daemon) is the single highest-leverage move
in the timeline. Approvals, reattach, journaling, pricing — all trivially
correct afterward, all impossible before.

**4. Write the maturity board before you build the features.** "0–3, the
board not the backlog" converts vibes into numbered commits. The embarrassing
scores (Verification: 0) are the roadmap. And when the map is walked, delete
it.

**5. Context engineering beats prompting.** Deterministic compaction, budget
+ nudge, one source of truth for context windows, reasoning replay across
tool calls. None of it demos well. All of it is the product.

**6. Profile with receipts.** Committing the parca CSVs did two things: it
made the optimization claims falsifiable, and it found `termimad`'s 49-crate
subtree hiding behind one print call. Deps are rented futures; `std` is
already installed.

**7. Do the boring work last, but do it.** License, CoC, SECURITY,
Windows gates, tests. Nobody tweets about CONTRIBUTING.md. Everybody's trust
routes through it.

**8. Two renames in 48 hours beats one rename in six months.** The name leaks
into env vars and paths within a week. Change it while the blast radius is a
search-and-replace.

---

## Coda

The crate that began as `ak`, spent a day as `oye`, and landed as `dex` now
installs like this:

```sh
curl -fsSL https://raw.githubusercontent.com/sumofbytes/dex/HEAD/scripts/install.sh | sh
```

Twelve days. 140 commits. Two versions. One god file, cremated. The whole
story is in the commit log — which is the actual point. **If your commit
messages can't tell this story, you're writing them for the wrong reader.**
Write them for the person who will need to reconstruct what happened in three
years. That person is usually you.

---

---

# Appendix — Repurpose kit

*Not part of the post. Raw material for the tweet storm, threads, HN/lobsters
comments, and talk notes.*

## Pull-quotes (one per act)

1. *"The repo was born mid-project — the first commit is a bug fix for a text
   editor that didn't have a repository yet."*
2. *"The god file died four days after the repo did."*
3. *"The TUI is not the app. The TUI is a view."*
4. *"Execution order lives in PLAN.md; this file is the board, not the
   backlog."*
5. *"State was written and never restored — a maturity board is a list of
   specific lies you're currently telling yourself."*
6. *"Dependencies are not features, they're futures you're renting."*
7. *"termimad cost 49 crates and was used for one print call."*
8. *"The perf work had to confess its own numbers — the flamegraphs are
   committed."*
9. *"v0.1.1 and v0.2.0 shipped the same day, because one of them was the
   release pipeline and the other was the provider layer it deserved."*
10. *"If your commit messages can't tell this story, you're writing them for
    the wrong reader."*

## Stats one-liners

- 2,191 → 24,299 lines of Rust in 12 days (1 file → 39).
- 0 → 220 tests, clippy at `-D warnings`, from day one of the modular split.
- Lockfile 399 → 348 entries after the purge; `termimad`'s 49-crate subtree
  replaced by a 111-line renderer.
- TUI cold start 780ms → 160ms; warm start skips a 4MB models.dev parse.
- Cadence: 1, 1, 4, 7, 7, 10, 7, 7, 2, 26, 31, 37 — the last three days carry
  over half the repo.
- 28 merged PRs, two releases (v0.1.1, v0.2.0) on the final day.

## Tweet-storm skeleton (12 tweets)

1. **Hook + numbers.** Twelve days ago: one 2,191-line main.rs called "ak,"
   a Kimi API key in a sample config. Today: 39 files, 24,299 lines of Rust,
   220 tests, releases for Linux/macOS/Windows. How a coding agent gets built
   in 12 days. 🧵
2. **First commit.** "Fix multiline editor viewport behavior." No README. The
   repo was born mid-project, already obsessing over text-editor physics.
3. **Day 2.** Before tools: the agent learns to read AGENTS.md/CLAUDE.md and
   load skills. A coding agent's first instinct shouldn't be tools — it
   should be knowing which repo it's standing in.
4. **The refactor day.** Seven commits, one wound: main.rs → 29 files.
   Modularize in week one; every day you wait, the god file grows a ligament.
5. **The epiphany.** "Refactor TUI to be pure HTTP client (OpenCode 2
   model)." Brain → axum daemon; TUI → dumb client over HTTP+SSE. One
   decision; approvals, reattach, crash-safe journaling all fall out free.
6. **The rename saga.** ak → oye → dex, two renames in 48 hours. Naming is
   the only refactor that gets MORE expensive the longer you wait.
7. **The board.** I stopped coding and wrote HARNESS.md: 20 areas, five
   pillars, each scored 0–3 against "world-class." Not a backlog. A board.
8. **Walking the board.** The commits carry the phase numbers: P0–P6, P5
   hardening, P8–P10. Plan Phase 0 was embarrassing: state was written and
   never restored (`#[allow(dead_code)]`). Boards turn "it mostly works"
   into a list of specific lies.
9. **Context engineering.** Deterministic compaction (no LLM call), budget-60
   + wrap-up nudge, models.dev as the single source of truth for context
   windows, reasoning replayed across tool calls. The loop should be boring.
   The context should be surgical.
10. **The perf war.** Committed my parca flamegraphs. Deleted termimad's
    49-crate subtree for a 111-line renderer, dropped tui-textarea,
    tower-http, rand, dirs, async-stream. Lockfile 399 → 348. Cold start
    780ms → 160ms. The status bar now shows "ready-in."
11. **Ship day.** Tag → CI builds musl Linux (zigbuild), mac, Windows +
    SHA256SUMS + curl installer. One manual step. v0.1.1 and v0.2.0 the same
    day — the provider layer refactor (per-provider key deposits, model
    auto-routing) earned its own breaking release.
12. **Lessons.** Ship the god file first. Refactor in week one. Split brain
    from interface. Write the board before the features. Profile with
    receipts. Do the license/CoC work last — but do it. That's dex. 🧵 done.

## Thread/comment angles

- **HN/lobsters opener:** "A maturity board for coding agents (0–3 per area,
  20 areas) is more useful than a roadmap — here's the one that drove 12 days
  of `dex`."
- **Controversy bait (use sparingly):** "Most agent repos are 80% prompt
  engineering and 0% context engineering. The demo is the prompt; the product
  is the compaction."
- **Talk outline:** cold open (god file) → three decisions that mattered
  (refactor day, client/daemon split, the board) → perf war with flamegraph
  screenshot → live demo of `ready-in` → lessons.
- **Screenshot list:** first-commit `Cargo.toml` (`name = "ak"`);
  HARNESS.md board table; `3cd3b49` diff stat; status bar with `ready-in` and
  spend; `scripts/install.sh` one-liner.
