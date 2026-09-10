# Dex Sub-Agent Architecture — Implementation Plan

Status: planned (rev 5). Rev 2 revised after a design review against prior art —
pi's subagent design, Claude Code's task/agent system, Amp's oracle, and
Codex's sandbox-boundary model — and a source audit of dex's actual
integration points. Rev 3 verified the prior-art claims against primary
sources over the network: the Claude Code docs
(`code.claude.com/docs/en/sub-agents`, `/agents`, `/tools-reference`,
fetched live) and the `openai/codex` repo (`codex-rs/core/src/agent/*`,
`codex-rs/core/src/tools/handlers/multi_agents*`,
`codex-rs/core/src/context/subagent_notification.rs`). Rev 4 made background
execution the V1 mode (dropping the rev 2–3 foreground detour). Rev 5 is a
correctness pass after a second source audit found three load-bearing errors
in rev 4: (1) the approval gate rev 4 built on does not enforce anything at
dispatch — see §3/§12; (2) `delegate_output`'s `select!` on steering is
unimplementable inside a tool call — see §10; (3) wake turns, heartbeat, and
first-class event variants are a second feature bundled into V1 — split into
V1b, see §10b/§15/§21. Conventions from `AGENTS.md` apply: minimal diffs,
reuse existing helpers, no new deps without need, `cargo fmt --check`,
`cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`.

V1 ships in two slices. **V1a** (delegation): same-runtime children,
`delegate`/`delegate_output`/`delegate_stop`, tool-subset enforcement,
detached auto-deny (§12), notices drained at real turn boundaries only,
child lifecycle as journaled `System` text lines, child JSONL. **V1b**
(delivery polish): idle wake turns + `last_client_seen` gating, labeled
approval prompts surfacing in the parent session, first-class event
variants. V1a is independently useful (explorer/reviewer run prompt-free;
tester runs read-only steps and auto-denies the rest when detached) and
every V1b item lands on a pre-committed seam without rework.

## 0. TL;DR

| Decision | Choice | Why (prior art) |
|---|---|---|
| Runtime | One agent loop for parent and children (`process_turn` is already generic over client/cancel) | pi: "a subagent is not special" |
| Result | `AgentResult { status, summary, error }` — the child's final message; synthesized from partial text + error when the child ends without one (§6) | pi, Claude Code, Amp all return final text; no forced JSON across heterogeneous providers |
| Execution mode | **Background-first**: `delegate` spawns and returns an `AgentId` immediately; the parent keeps working or ends its turn; completions are announced at turn boundaries; a bounded `delegate_output` poll-wait exists for the rare "I need this before I continue" case | CC defaults to background; Codex `spawn_agent` is non-blocking and `wait_agent` is bounded. Verified convergence — this *is* the mainstream shape. Dex's follow-up chaining (`src/daemon/mod.rs:159-162`) is the hard part, already built; the approval gate is **not** — it is Phase 0, see §3/§12 |
| Definitions | Markdown + frontmatter, discovered from `.dex/agents/`-style dirs (mirrors `skill_dirs()` in `src/skills.rs`); built-ins embedded in code | pi and Claude Code converged on this independently |
| Boundaries | Enforced at dispatch in `src/tools/mod.rs` via an explicit `ToolFilter` parameter (§8, §11): child tools ⊆ parent tools, child permission ⊆ parent policy (once Phase 0 builds the gate), **children cannot delegate** | Codex's core lesson: runtime-enforced subset, never prompt-enforced |
| Child transcript | Own JSONL under the parent's session dir, inspectable | pi/CC both make child transcripts inspectable; dex's crash-recovery convention requires it |
| Events (V1a) | Child lifecycle as journaled `System(...)` text lines in seq'd `StreamEnvelope`s — parseable by every existing client, zero wire change | per-tool-call events across SSE drown the TUI; the journal already survives reconnects |
| Events (V1b) | First-class `AgentSpawned/Progress/Completed` variants — an explicit wire bump with client fallback | typed events only once a consumer needs them |
| What V1 does **not** have | agent-to-agent messaging, DAGs, recursion, agent memory, user-defined agent files, idle wake turns (V1b), labeled child approval prompts (V1b) | see §20 |

---

# 1. Design Principles

### Keep the core small

```text
AgentDefinition   what an agent is (declarative, no runtime state)
AgentInstance     one execution of a definition
AgentManager      registry + lifecycle owner + notification queue (struct, not a trait)
AgentContext      the isolated input a child is given
AgentResult       status + final text returned to the parent
```

### Separate definition from execution

An `AgentDefinition` describes what an agent is. An `AgentInstance` is one
run. Never mix configuration with runtime state.

### Reuse the existing agent runtime

There is exactly one agent loop. Do not implement `run_main_agent()` and
`run_sub_agent()` as separate loops; do not fork `process_turn`. The
refactor (§8) is parameter-bundling plus capability flags, not a rewrite.

### Isolate context

A child never receives the parent's transcript. It gets the delegated task,
optionally file hints and a parent summary (§5). Child tool calls, steering,
and journaling stay isolated from the parent's.

### Return compressed results

The tool result the parent consumes is the child's final assistant message
plus a status. No structured extraction, no transcript injection. When the
parent is not blocked on `delegate_output`, the same text arrives as a
completion notice at the next turn boundary.

### Enforce boundaries in code

Tool allowlists, permission inheritance, and the no-delegation rule are
enforced at dispatch time in runtime code. System prompts are instructions,
not security boundaries. Note honestly in docs: dex's enforcement is
in-process (like every in-process design; Codex's OS sandbox is out of
scope and must not be reimplemented). And honestly about sequencing: until
Phase 0 (§12) lands, the permission half of the subset rule has no gate to
inherit — V1a ships the tool half plus detached auto-deny, not the full
subset claim.

### Results land at turn boundaries, never mid-stream

Completion notices are injected as messages at the start of a parent turn —
the same discipline CC (notification in a later turn) and Codex
(`<subagent_notification>` user fragment) both follow. The steering channel
stays user-owned and mid-turn; agent notices never steal it mid-turn. In
V1a the boundaries are real turns only (chained follow-up, next user chat);
idle wake turns are V1b (§10b).

### One knob per concern

No second turn-budget mechanism, no second model resolver, no second
permission engine. Sub-agents feed the existing knobs.

### Avoid premature orchestration

V1 is parent → child → result, depth 1. No workflow engine.

---

# 2. Prior-Art Positions (what we adopt, and from whom)

| From | Adopt |
|---|---|
| **pi** | Result = final text. Subagent = same loop + different prompt/tools/model. No manager abstraction beyond a registry. Child transcripts remain inspectable. |
| **Claude Code** (verified against live docs at rev 3; re-verify version-specific claims before relying on them) | Markdown + frontmatter definitions discovered from directories. Subagents run in the background **by default** in interactive sessions (foreground only when the model needs the result first; `CLAUDE_CODE_DISABLE_BACKGROUND_TASKS=1` forces foreground); spawn returns immediately; results reach the parent as a **completion notification in a later turn**; retrieve via the task's output file (`TaskOutput` deprecated in favor of `Read` on that path); stop via `TaskStop`; and since v2.1.186 a background child's permission prompts **surface in the main session, naming the child** (before that: auto-deny). Background children run a reduced built-in tool whitelist + MCP tools. Nesting is allowed, default 3 layers (`CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH`), Agent tool withheld at the limit. `maxTurns` caps turns → result marked partial, child resumable. |
| **Codex** (verified against `openai/codex` main at rev 3; re-verify before relying on internals) | The subset rule: a child can never exceed the parent's tool/permission policy, enforced by the runtime. (Codex pairs an OS sandbox with a "guardian" auto-authorization for workers — bounded root evidence, no interactive prompts; dex's analogue is dispatch-time checks — do not claim more.) Non-blocking `spawn_agent` (returns agent id + nickname immediately); blocking `wait_agent` with a timeout (default 30 s, min 10 s, max 1 h) that returns a *summary* of which agents have updates and **ends early on child activity or steered user input**; concurrency capped at 4 slots per session ("including you"), completed agents count until `close_agent`; completion/final status injected as a `<subagent_notification>` user fragment — the same notification-into-next-turn-context pattern as CC; and `DEFAULT_AGENT_MAX_DEPTH = 1` — no nested spawning by default, matching dex V1's no-nesting rule. |
| **Amp (oracle)** | Few, hand-picked, purpose-shaped built-in agents beat a framework for authoring arbitrary ones. Three built-ins, no user-defined agents required. |

Where both systems converge, trust it — and rev 5 adopts all of it across
V1a/V1b: spawn is non-blocking and returns an id immediately; blocking
exists only as an explicit bounded wait (Codex `wait_agent`, dex
`delegate_output` — rev 5 corrects the interrupt mechanism to
cancel-polling, §10); results/notifications land at turn boundaries, never
mid-tool streaming; concurrency slots are capped; and child approvals
resolve in the parent's session (auto-deny when detached in V1a; surfaced
prompts naming the child in V1b), not in a separate UI.

---

# 3. Repository Facts (Phase 1 pre-done, corrected rev 5)

The architecture note Phase 1 would have produced, grounded in source.
Rev 5 corrects the rev 4 audit where re-reading the code proved it wrong —
the corrections are the point of this section:

```text
Agent loop        src/agent/loop.rs:223  process_turn(config, messages, state,
                  steering_rx/tx, session, client, cancel, console) — already
                  generic over ModelClient + CancellationSource; 9 args to bundle
Turn budget       src/agent/loop.rs:237  tool_budget = max_tool_iterations();
                  also the 1_000_000-iteration guard
Tool dispatch     src/tools/mod.rs:1250  execute(name, args, cancel) —
                  read/bash/write/edit/grep|ffgrep|find|fffind/git/chain;
                  dynamic MCP tools arrive via mcp.rs; parallel fan-out via
                  the turn's JoinSet (docs/mcp-plan.md:28).
                  NOTE: execute takes NO filter/identity parameter today —
                  §8/§11 must add one; "enforced at dispatch" is new code.
Permissions       HONEST STATUS (rev 5 correction): the approval channel
                  EXISTS but NOTHING ENFORCES IT. Console::daemon
                  (src/daemon/server.rs:993; loop.rs:852 is the test-only
                  mirror) creates sink_tx + approval_tx; the daemon parks
                  ApprovalRequest in state.pending_approvals
                  (src/daemon/mod.rs:143, key = request_id) and emits
                  ApprovalRequired{request_id, name, input}
                  (src/protocol/mod.rs:157-162); the client resolves via
                  POST /approve {request_id, decision} (protocol/mod.rs:110).
                  BUT: nothing in src/ ever sends on the approval channel;
                  execute() never consults PermissionMode or
                  PermissionRequirement (the only consumer is the chain
                  read-only gate, tools/mod.rs:1158); and the e2e test
                  asserts a write under DEX_PERMISSION=ask-writes produces
                  ZERO ApprovalRequired events
                  ("Default is trusted, so write succeeds without approval",
                  src/daemon/server.rs:2482,2530-2538). The gate is Phase 0
                  (§12) — a prerequisite, not a reuse.
Turn chaining     follow-up queue consumed by run_agent_turn's outer loop,
                  which "chains a new process_turn iteration without a new
                  HTTP request" (src/daemon/mod.rs:159-162) — the existing
                  seam for V1a notification drain
Concurrency guard one turn at a time per session: chat POST while
                  active_turns holds the session → 409 CONFLICT
                  (src/daemon/server.rs:508-510)
Approval teardown a turn-end guard Denies still-pending approvals for the
                  SESSION when a turn ends (src/daemon/server.rs:658-674 —
                  note: all of the session's, not "that turn's"); child
                  approvals must be exempted once they exist (§12)
Event journal     StreamEnvelope {seq, event} persisted per session;
                  GET /api/sessions/{id}/events?since=<seq> replays after a
                  cursor (src/daemon/server.rs:1490); event_seqs seeded from
                  disk at startup (src/daemon/mod.rs:165) — V1a child
                  lifecycle rides this as System text lines; NO new event
                  variants in V1a (rev 4's "no new wire surface" claim
                  contradicted its own §15 — corrected here)
Cancellation      per-turn CancellationToken, Clone + owned
                  (src/core/console.rs:79-85,114) + GlobalCancellation;
                  manager-owned child tokens are feasible with no new type
Async             one tokio runtime at the daemon edge; turn loop is
                  thread-bridged (docs/async-plan.md) — children spawn as
                  tasks on the existing runtime, no new pool
Sessions          src/session.rs  append-only JSONL under
                  $XDG_DATA_HOME/dex/sessions/<slug>/ — single file per
                  session today (from_path/append_message/turn_event assume
                  it); agents/ subdir support is new code (§16)
Definition-file   src/skills.rs skill_dirs() — cwd .dex/skills,
  discovery       .agents/skills, $XDG_CONFIG_HOME/dex/skills; sorted,
                  first name wins, duplicates warned — reuse this pattern
Compaction        src/agent/compaction.rs — deterministic by default;
                  tokens > contextWindow - reserveTokens logic must stay intact
Config surface    src/llm/config.rs — one model knob (provider/model vs
                  models.dev catalog); no "fast" alias exists today
Modes             Serve/Connect (daemon + TUI client), OneShot, RunTool —
                  OneShot has no daemon: background delegation is impossible
                  there regardless of V1 scope. NOTE: tools_schema() is
                  global (src/llm/protocol.rs:32; consumed at client.rs:324,
                  anthropic.rs:249) with no per-mode parameter — unregistering
                  delegate in OneShot needs plumbing (§10, §21)
```

No new abstraction where an existing one serves — but no claiming one
serves where the code proves it doesn't.

---

# 4. Core Domain Types

```text
src/agent/subagent/
  mod.rs          module glue, built-in definitions
  definition.rs   AgentDefinition + frontmatter parsing
  instance.rs     AgentInstance, AgentState, AgentId
  context.rs      AgentContext / ContextSeed
  result.rs       AgentResult
  manager.rs      AgentManager (registry, spawn, bounded wait, status,
                  cancel, notification queue)
```

## AgentDefinition

```rust
struct AgentDefinition {
    name: String,
    description: String,          // shown to the parent model for routing
    prompt: String,               // body: the child's system-prompt persona
    model: Option<String>,        // same single knob: "provider/model";
                                  // None = inherit parent's resolved model
    tools: BTreeSet<String>,      // allowlist against the real registry
    permissions: PermissionInherit, // subset clamp of the parent policy
    max_tool_iterations: Option<u32>, // feeds max_tool_iterations(); None = default
    timeout: Duration,            // default shared constant, per-def override
}
```

Rules:

- `model` goes through the existing resolver (`src/llm/config.rs`). No
  aliases the catalog doesn't have. No per-provider key logic here.
- `max_tool_iterations` maps onto the existing budget
  (`src/agent/loop.rs:237`) — it is the same knob re-parameterized, not a
  second counter.
- Declarative only; no runtime state.

## AgentInstance / lifecycle

```rust
enum AgentState { Pending, Running, Completed, Failed, Cancelled, TimedOut }
```

`AgentInstance { id: AgentId, definition, parent_id: Option<AgentId>,
context, state }`. `AgentId` is a short unique id (counter + session slug);
it appears in events, session paths, results, and approval labels.

---

# 5. AgentContext

```rust
struct ContextSeed {
    task: String,                        // required
    file_hints: Vec<PathBuf>,            // optional, workspace-relative
    parent_summary: Option<String>,      // optional, model-written
}
```

- No `working_directory` field: the child inherits the parent's workspace
  root. This keeps `resolve_workspace_path` confinement intact — a
  delegation request must never be able to move a child outside the
  workspace. Caveat (rev 5): tools resolve against the *process* cwd
  (`workspace_root() = current_dir()`, `tools/mod.rs:48`) while sessions
  carry a `cwd` string — whatever per-session isolation exists today,
  children inherit its ambiguity. One test must verify parent and child
  resolve the same workspace root (§17).
- The parent's transcript is never copied. If the parent wants to pass
  context, it writes it into `task` or `parent_summary`.
- Child system prompt = definition `prompt` body, plus the standard
  structure dex already builds (`src/llm/prompt.rs`, incl.
  `project_context()` / AGENTS.md auto-append), with the child's restricted
  tool list rendered instead of the parent's.

---

# 6. AgentResult

```rust
struct AgentResult {
    status: AgentState,        // Completed | Failed | Cancelled | TimedOut
    summary: String,           // the child's final assistant message, verbatim
    error: Option<String>,     // actionable message for Failed/Cancelled/TimedOut
}
```

The summary is what the parent consumes — as a `delegate_output` tool result,
or verbatim inside the completion notice at the next turn boundary. Findings,
file paths, risks, and recommendations live in the prose the child writes —
this is what pi, Claude Code, and Amp's oracle all ship. No forced JSON:
dex's per-endpoint protocol variance is precisely why a cross-provider
structured schema is a liability. If a child wants to hand the parent
structured data, it points at files it read/wrote; the parent reads them with
its own tools.

When the child ends without a final message (budget exhaustion returns
`Err`, cancel lands mid-turn, timeout fires mid-LLM-call), there is no final
text to copy. The manager then synthesizes: `summary` = last partial
assistant text if any, else `""`; `error` always set (`"tool budget
exhausted after N iterations; partial progress: …"`, `"cancelled while …"`,
`"timed out after …"`). Every terminal state yields an `AgentResult`; only
`Completed` guarantees a non-empty `summary` (§22-J).

---

# 7. AgentManager

A struct owned by the daemon — **one per session, stored on `DaemonState`
as `agents: Mutex<HashMap<SessionId, AgentManager>>`** (new slot; the
registry **must** outlive individual parent turns since background children
run across turn boundaries), not a trait — introduce an abstraction only
when a second implementation exists.

```rust
impl AgentManager {
    async fn spawn(&self, def: &AgentDefinition, seed: ContextSeed) -> Result<AgentId, SpawnError>;
    async fn wait(&self, id: AgentId, timeout: Duration) -> WaitOutcome;  // bounded
    fn status(&self, id: AgentId) -> Option<AgentState>;
    async fn cancel(&self, id: AgentId);             // + propagate to child token
    async fn drain_notices(&self) -> Vec<AgentNotice>;  // take pending completions
}
```

Owns: the active-agent registry (id → running task + state + handle),
cancellation sources, timeouts, cleanup, and the **notification queue** —
completed children push an `AgentNotice { agent_id, name, status }`; drains
happen only at the delivery points in §10b. Does not contain model or
provider logic — children resolve their model through the existing config
path.

Lifecycle (rev 5 — previously unspecified): the entry is created lazily on
first `delegate` for a session and removed when the session is deleted/reset
(which cancels running children first, then drops the entry); daemon
shutdown cancels all managers' children and joins the tasks (§14). After a
daemon restart the map is empty by design (in-process); the parent
transcript's `delegate` call plus the child JSONL on disk are the record,
and resume surfaces those children as interrupted (§14, §16). No orphaned
tokio task survives any exit path.

Caps concurrent children (default 4, per session) to protect provider rate
limits. `spawn` at capacity rejects with the running agent list — the model
self-corrects (stop one, or wait). No hidden queue: an invisible backlog is
how waits go unbounded.

---

# 8. Runtime Refactor — what children get and don't get

`process_turn` already takes the client and cancellation as traits; the
extraction is bundling its nine parameters into a per-agent capability
bundle:

```rust
struct AgentRuntime<'a> {
    config: &LlmConfig,          // child's resolved model
    messages: &mut Vec<ChatMessage>,  // child's own history
    client: &dyn ModelClient,    // same trait, mockable (see §17)
    cancel: CancellationSource,  // owned by the manager, NOT the parent turn
    session: Option<&mut Session>,    // child's own JSONL (§16), not the parent's
    console: Console,            // child's journal sink + approval channel (§12)
    steering: None,              // children never get parent steering
    tool_filter: Option<&'a ToolFilter>, // None = unfiltered (parent);
                                 // Some = child allowlist, enforced at dispatch
}
```

`ToolFilter` threading (rev 5 — previously unspecified): today
`execute_tool_call → execute_outcome → execute(name, args, cancel)` takes no
identity, and callers include the agent loop, `chain` steps, the `!` shell
route, and `dex run`. The filter travels as an explicit parameter down that
chain (`execute(..., filter: Option<&ToolFilter>)`); `None` preserves
current behavior for every existing caller, `Some` enforces §11 including
the no-delegation rule. No ambient task-local, no hidden state — the
test-hostile option is rejected. MCP dynamic tools match by prefix or
explicit name at the same check.

Children explicitly do **not** inherit:

```text
parent transcript           (§5)
parent steering channel     user input goes to the parent only
parent Session/journal      child gets its own file (§16)
parent tool allowlist       subset-clamped (§11)
parent model                only if the definition doesn't override
parent turn's cancel token  a child outlives the turn that spawned it (§14)
```

Children explicitly **do** get: compaction (a child task can overflow —
same deterministic path), the existing turn budget, overflow-retry
semantics, and `project_context()`.

Exit condition for this phase: the main agent's behavior is byte-identical
(diff is a parameter bundle + two flags), and a child can run the same loop
with an empty history plus a seed.

---

# 9. Reusable Agent Runtime

```text
Agent Runtime (process_turn)
    ├── Main Agent Context      (steering, parent session, full toolset)
    └── Sub-Agent Context       (no steering, child session, filtered tools)
```

The runtime owns LLM requests, tool calls/execution/results, turn limits,
compaction, termination. It does not know whether the caller is main or
sub — the capability bundle decides.

---

# 10. Delegation Tools

Three model-facing tools. Background is the only spawn mode — no
`run_in_background` flag to forget.

```text
delegate(agent, task, file_hints?) -> { agent_id, state: "running" }
delegate_output(agent_id, wait_seconds?) -> AgentResult | { state, progress }
delegate_stop(agent_id)                    -> AgentResult | { state }
```

Registration: behind `DEX_SUBAGENTS=0` the three tools are unregistered and
the module is dead code (§19). `delegate` is additionally unregistered in
OneShot/no-daemon modes — rev 5 note: `tools_schema()` is global
(`src/llm/protocol.rs:32`, consumed at `client.rs:324`,
`anthropic.rs:249`) with no per-mode parameter today, so this needs a
registration-time filter argument threaded from `Mode` to schema
construction, not a prompt hack (§21, Phase 5).

Semantics:

1. `delegate` resolves the definition by name; unknown names fail with the
   list of available agents (clean rejection, no fallback). It builds the
   child context (§5) from the tool arguments — the parent model writes the
   task text itself; dex never auto-copies transcript — submits to
   `AgentManager`, and returns the `AgentId` immediately. It never executes
   a child inline; it is a thin client of the manager.
2. `delegate_output` is the *bounded* poll-wait (Codex `wait_agent` shape):
   default `wait_seconds = 0` (poll), maximum 120. It returns terminal
   `AgentResult`s for finished agents immediately, otherwise the current
   state plus a short progress tail. Implementation (rev 5 correction): the
   wait loop **polls the child handle with short sleeps and checks the
   parent turn's cancel token between sleeps** — it returns early on
   completion, timeout, or parent cancel. It does NOT `select!` on the
   steering channel: steering is `&mut`-borrowed by the `process_turn`
   loop and unreachable from inside a tool call, so rev 4's `select! {
   child_done, steering_msg, timeout }` was unimplementable. User steering
   is therefore noticed at the next loop iteration after the wait returns
   (bounded by the sleep interval, ~250 ms), never wedging the conversation
   longer than that — document the latency in the tool description rather
   than claiming an interrupt that cannot exist.
3. `delegate_stop` cancels the child; the tool result carries the resulting
   `AgentResult` (status `Cancelled`).
4. Completion notices: every terminal child pushes an `AgentNotice` into the
   manager's queue. Delivery happens at turn boundaries (§10b) — the model
   never has to poll to *learn* a child finished; polling is only for
   *fetching* a result it needs right now.

Usage guidance lives in the tool descriptions (protocol layer, per
`AGENTS.md` — not `prompt.rs`): "delegate returns immediately; use
delegate_output when you need the result before continuing; otherwise end
your turn — completions are announced automatically."

---

# 10b. Turn Boundaries, Wake, and Delivery

## V1a (ships): drain at real turn boundaries only

`AgentNotice`s accumulate in the manager (per session). Bounded at 32;
overflow folds into a single summary notice ("N agents finished;
delegate_output for details") — no unbounded growth, no lost terminal
statuses.

The single V1a drain point: inside `run_agent_turn`'s outer loop — the same
place follow-ups chain the next `process_turn`
(`src/daemon/mod.rs:159-162`) — drain pending notices and prepend them as
`ChatMessage::user_named(text, "agent-notifications")` to the next
iteration. No new turn is invented; the loop already exists. Notices for a
parent whose turn has fully ended wait for its next user chat, where the
same drain runs at turn start. Steering stays user-owned throughout: agent
notices never use it mid-turn; they wait for the boundary — the
convergence rule from §2, kept intact.

## V1b (deferred): idle wake + presence gating

Rev 4 specified a wake turn (a daemon task spawning a full LLM turn to
deliver a notice to an idle session) with `last_client_seen` heartbeating
off `GET /events` and a 409-race client retry. Rev 5 defers all of it, for
three reasons: (a) a wake needs a synthesized `ChatRequest` (which model?
which permission? whose headers?), usage accounting, and error handling —
none of which exists request-less today; (b) it spends tokens to deliver
what the TUI can render as a toast from its existing event poll for free;
(c) the gating as specified rarely gates (any open TUI polling `GET
/events` every 2 s keeps `last_client_seen` fresh, so the 30 s window is
near-always open) while adding a mutex write to the hottest read path, and
the "client retries 409 once" logic does not exist anywhere.

When V1b lands, it keeps these constraints: wake fires only when
`active_turns` is empty **and** a real presence signal (designed then, not
assumed now) confirms an audience; a wake never contends with a user's own
chat POST — chat wins, wake skips (no user-visible 409 may ever lose a race
with a background notice); reattach-with-backlog wakes at most once. The
V1a notice queue, drain seam, and `System`-line events are the unchanged
foundation it builds on.

**OneShot / no-daemon modes:** `delegate` is not registered — there is no
daemon to run children in, background or otherwise.

---

# 11. Tool Policy

Allowlist per definition, matched against the real registry — `read`,
`bash`, `write`, `edit`, `grep`/`ffgrep`/`find`/`fffind`, `git`, `chain`
(`src/tools/mod.rs:1250`) — plus dynamic MCP tool names (prefix or
explicit-name matching; the policy layer must not assume a closed enum).

Enforcement, at dispatch, in code, via the `ToolFilter` parameter from §8:

```text
child_allowed(t) = def.tools.contains(t) ∧ parent_allowed(t)
                   ∧ t ∉ {delegate, delegate_output, delegate_stop}
```

A denied call is rejected with a tool error naming the policy — the model
self-corrects; the runtime never executes it. `parent_allowed(t)` is the
parent turn's own filter (today: everything except the delegation tools in
modes where they are unregistered; `git`/`chain` behind `DEX_EXTRA_TOOLS`).
This is the dex analogue of Codex's sandbox: not an OS boundary, and docs
must not claim otherwise.

---

# 12. Permission Policy

Availability (tool policy, §11) and permission (approval) stay separate —
with the subset rule Codex teaches:

```text
child permission policy ⊆ parent permission policy
```

**Phase 0 (prerequisite — rev 5): build the gate.** Rev 4 presented the
approval system as existing machinery ("parks ApprovalRequest…", "the
approval channel"). The code proves otherwise (§3): the channel is created,
parked requests can be resolved, but nothing ever *sends* — `execute()`
never consults any policy, and the suite asserts no prompt appears under
`ask-writes`. Before any child-permission claim is meaningful, dispatch
must consult the policy: `execute()` (and its callers per §8) checks
`PermissionRequirement` against the turn's `PermissionMode` (+ live
`session_approvals`), parks + emits `ApprovalRequired` on a mutating call
that needs it, and blocks for the verdict. Phase 0's exit condition is a
failing-today test turned green: `write` under `ask-writes` with a live
client produces exactly one parked, labeled-or-unlabeled prompt and blocks
until resolved. Without Phase 0, the rest of this section is untestable.

**V1a rule (ships on the new gate): detached auto-deny only.** When no
client is fresh, a child's mutating call **auto-denies immediately** and the
denial is recorded in the child's result — a background child can never
block silently on an absent user (CC's pre-2.1.186 fallback, and the only
safe default). Attached-session child prompts in V1a resolve through the
same path as the parent's own prompts; no child-specific labeling yet. Net
effect on built-ins: `explorer`/`reviewer` never prompt (prompt-free tools);
`tester`'s `bash` prompts when attached, auto-denies when detached.

**V1b (deferred): labeled prompts + guard fix.** A child's approval sender
parks in the same `pending_approvals` map and emits `ApprovalRequired {
request_id, name, input, agent: Some("explorer") }` — the `agent` field a
new, optional, serde-defaulted wire addition (older clients ignore it); the
TUI renders "explorer wants to run bash: …" (CC's v2.1.186 pattern, to be
re-verified at implementation time). Session-level approvals apply to
children, consulted live from `session_approvals` since children outlive
the turn that restored them. Prompt timeout: a parked child approval
unanswered for 5 minutes denies and records. Turn-end guard fix: the guard
that Denies parked approvals when a turn ends
(`src/daemon/server.rs:658-674`) must skip entries with `agent.is_some()` —
children outlive the parent turn; their parked approvals belong to the
session, not the turn.

No second permission engine; `PermissionRequirement`
(`src/tools/mod.rs:185`) is the single source of truth throughout.

---

# 13. Model Selection

- Per-definition `model` uses the existing one-knob string
  (`provider/model` against the models.dev catalog). No `fast`/`strong`
  aliases until the catalog supports aliases; until then definitions name
  real models or inherit the parent's.
- Provider keys/endpoints resolve exactly as today — sub-agent code
  contains zero provider-specific logic.
- When per-agent model config becomes user-facing it follows the
  `AGENTS.md` config surface in full: origin row in `dex doctor`,
  unknown-key handling, README env table, no write-back of deprecated keys.

---

# 14. Cancellation, Timeout, Cleanup

- **Esc ≠ child cancel.** Cancelling the parent turn (Ctrl+C, client
  disconnect) stops the parent loop and any in-flight `delegate_output`
  wait — it does **not** touch running children. That asymmetry is the
  point of background: the child survives the turn that spawned it. The
  parent's cancellation token is no longer an ancestor of child tokens;
  child tokens are owned by the manager (`CancellationToken` is `Clone` +
  owned, `src/core/console.rs:79-85` — no new type needed).
- Children are cancelled by: `delegate_stop`, per-definition timeout
  (default 10 min → `TimedOut` with a synthesized partial-status error,
  §6), session deletion/reset, or daemon shutdown. On any of these the
  child's `turn_failed`-equivalent markers land in its JSONL first — crash
  loses at most the in-flight event.
- `Completed`/`Failed`/`Cancelled`/`TimedOut` all push the completion
  notice, drop the registry entry, and join the task — no orphaned tokio
  task survives any exit path, including daemon shutdown. (TUI disconnect
  ≠ session end: children keep running; notice delivery simply waits for
  the next real turn boundary, §10b.)
- After a daemon restart children are gone (in-process by design); the
  parent transcript's `delegate` call and the child JSONL are the record —
  resume shows them as interrupted, state recoverable from disk. The
  manager map (§7) starts empty; nothing is re-spawned implicitly.

---

# 15. Events

## V1a (ships): `System` text lines, zero wire change

Child lifecycle is journaled through the existing seq-numbered envelope
journal as `System(...)` text lines with a stable prefix the TUI matches
on (`[agent explorer:<id>] started|progress <tool>|finished <status>`):

```text
System "[agent explorer:ab12] started"
System "[agent explorer:ab12] progress bash"
System "[agent explorer:ab12] finished completed"
```

No new `StreamEvent` variant, no per-turn SSE coupling, parseable by every
existing client — rev 4's contradiction (new variants under "no new wire
surface") is resolved by not adding variants. No per-tool-call events; the
progress line is a label the child updates. (Per-call streaming across SSE
drowns the TUI — pi renders nested transcripts on demand instead.)

**Delivery to the client:** during the parent's own turn the TUI is on the
turn SSE; child lines arrive via the existing `GET /events?since=<seq>`
poll the client already performs (no new 2 s idle poller in V1a — the
client learns "children may be running" from the parent transcript's
`delegate` result and polls `?since=` at its current cadence). Journaled
lines replay through the same client handler as turn-stream events,
deduped by `seq` (the cursor the reattach path already maintains).
The TUI consumes lines only; it never inspects agent internals.
The runtime is fully usable without the TUI (events are optional sinks).

## V1b (deferred): first-class variants

`AgentSpawned { agent_id, name }` / `AgentProgress { agent_id, state,
current_tool }` / `AgentCompleted { agent_id, status }` as real
`StreamEvent` variants — declared then as an explicit wire bump with a
client-fallback rule for old clients (ignore unknown `type`, keep the
`seq` cursor moving). Only build this once a consumer needs typed fields
the `System` prefix cannot carry.

---

# 16. Sessions and Journaling

Each child journals to its own append-only JSONL beside the parent's:

```text
$XDG_DATA_HOME/dex/sessions/<slug>/agents/<agent_id>-<name>.jsonl
```

- `Session` today assumes one file per session — `agents/` support is new
  code: a constructor taking the explicit child path (same marker
  discipline: `turn_start`/`turn_complete`/`turn_failed`, so a crash loses
  at most the in-flight event), session listing that surfaces `agents/` as
  attached runs (running, completed, interrupted-after-restart), and a
  hard rule that `load_llm_messages_from_session` (and resume path
  reconstruction) never ingests `agents/*` into the parent transcript.
- The parent's transcript records only the `delegate` call, any
  `delegate_output` result, and the completion notice — never the child
  transcript.
- This is what makes observability and debugging real (pi/CC both keep
  child transcripts inspectable) and satisfies the crash-recovery
  convention.

---

# 17. Testing Strategy

All child runs in tests use a mock `ModelClient` (the trait already
exists — script the child's turns; no real API calls; hermetic
`XDG_CACHE_HOME`/`DEX_CONFIG` + `EnvRestore` per AGENTS.md test
conventions).

- **Phase 0:** `write` under `ask-writes` with a live client parks exactly
  one prompt and blocks until resolved; deny/allow both honored; cancel
  Denies parked approvals (existing `server.rs:1378` behavior, now
  reachable).
- **Definitions:** frontmatter parses; invalid/unknown names fail with
  available-agent list; built-ins load.
- **Isolation:** child receives task + file hints; parent transcript never
  present in child messages; child writes only to its own JSONL (parent
  loader ignores `agents/*`); child tool calls never enter parent history;
  parent and child resolve the same workspace root.
- **Tool policy:** allowed tools run; denied calls rejected at dispatch
  with a policy error; `delegate`/`delegate_output`/`delegate_stop` denied
  for every child (depth 1 at the dispatch layer, not the prompt).
- **Permissions (V1a):** detached child auto-denies mutating calls and
  records the denial; attached child prompts through the Phase 0 gate;
  `explorer`/`reviewer` never prompt.
- **Permissions (V1b):** prompt carries the agent label; `AllowSession`
  applies to children; 5-minute prompt timeout denies + records; turn-end
  guard spares `agent: Some` entries.
- **Lifecycle:** spawn → running → completed / failed / cancelled /
  timed out, each asserted via `AgentState`; completion notice queued on
  every terminal path; `TimedOut`/budget-exhaustion yields a synthesized
  `AgentResult` with `error` set (§6).
- **Concurrency:** two+ children concurrently; independent results
  matched to ids; one failure doesn't poison siblings; spawn-at-capacity
  rejects cleanly; registry stays consistent (no torn state under
  concurrent completion).
- **Turn boundaries (V1a):** notice drains into the next chained iteration
  (followup path) and into the next user chat's turn start; parent cancel
  mid-`delegate_output` yields `Cancelled` for the wait but leaves the
  child running (assert child still completes and its notice drains
  later); steering arriving during a wait is acted on at most one sleep
  interval after the wait returns; no orphan task remains (manager
  active-count reaches zero).
- **Wake (V1b):** fires only when idle ∧ real presence; skipped when a
  turn holds the session (notice survives); never steals a race with a
  user chat POST.
- **Timeout:** expiry produces `TimedOut` + cleanup + notice.
- **Integration:** main → delegate → child (mock client) → child tool call
  → result → notice → main continues on its next turn; plus a no-TUI run
  (console sink absent).
- **Regression:** existing suite passes; single-agent path unchanged;
  `DEX_SUBAGENTS=0` leaves the binary materially single-agent.

---

# 18. Observability

Per child, via the existing usage/trace infrastructure (`record_usage`,
provider logs — no new metrics system):

```text
agent_id, parent_agent_id, name, model, state transitions,
start/end/duration, turn_count, tool_call count, tokens in/out
```

Not exposed wholesale in the TUI in V1a (§15's progress label only). Logs
never dump child contexts. Child usage events are journaled like child
lifecycle lines so client-side spend accumulation stays honest (deduped by
`seq` on replay).

---

# 19. Configuration and Built-ins

V1a ships three built-ins, zero required configuration:

| Agent | Purpose | Tools | Notes |
|---|---|---|---|
| `explorer` | understand code without modifying it | `read`, `ffgrep`, `fffind` | read-only, never prompts |
| `reviewer` | review a diff for correctness/regressions | `read`, `ffgrep`, `fffind`, `git` | `git` currently requires `DEX_EXTRA_TOOLS=1`; degrade gracefully to the read-only trio when absent |
| `tester` | investigate and run relevant tests | `read`, `ffgrep`, `fffind`, `bash` | `bash` is mutating → §12: prompts when attached (Phase 0 gate), auto-denies when detached |

No separate implementation agent: the main agent implements.

Kill switch: `DEX_SUBAGENTS=0` unregisters the delegation tools and the
module — the single-agent path stays materially unchanged (§22-A).

User-defined agents (post-V1): markdown + frontmatter discovered from
`.dex/agents`, `.agents/agents`, `$XDG_CONFIG_HOME/dex/agents` — mirroring
`skill_dirs()` (first `name` wins, duplicates warned). When this lands it
follows the config-surface rules (doctor origin rows, unknown-key list,
README). Configuration is never a prerequisite for V1.

---

# 20. V1 Explicitly Excludes

```text
user-defined agent files          (post-V1, shape pre-committed in §19)
agent-to-agent messaging          shared mutable context
recursion / depth > 1             children cannot delegate (§11)
foreground-only spawn mode        delegate+wait covers it; two spawn paths would be two paths to test
agent-authored agent definitions complex DAG workflows
persistent/cross-session agents   distributed execution
agent memory                      YAML config as a prerequisite
idle wake turns                   V1b (§10b) — V1a drains at real boundaries
labeled child approval prompts    V1b (§12) — V1a has detached auto-deny on the Phase 0 gate
first-class agent event variants  V1b (§15) — V1a uses System text lines
```

Each is a possible extension; none may complicate the V1a core.

---

# 21. Implementation Sequence

| Phase | Content | Exit condition |
|---|---|---|
| 0 | **Approval gate (prerequisite):** dispatch consults policy, parks + emits `ApprovalRequired`, blocks for verdict | `write` under `ask-writes` parks one prompt and blocks; deny/allow honored; §17 Phase 0 tests green |
| 1 | Architecture note — done (§3) | integration points named with file:line; §3's honesty corrections stand |
| 2 | Runtime extraction: capability bundle around `process_turn` incl. `tool_filter: Option<&ToolFilter>` threaded through `execute_tool_call → execute_outcome → execute` | main-agent behavior byte-identical (`None` path); child runs same loop with own seed/session/console |
| 3 | Core types + definition parsing + built-ins | definitions load; unknown names reject cleanly |
| 4 | AgentManager: `DaemonState.agents` slot + lifecycle (lazy create, session-delete/shutdown cancel+join, restart-empty), registry, spawn (cap, reject-at-capacity), bounded wait, cancel, notification queue | lifecycle unit tests pass; no orphans on any exit path |
| 5 | `delegate` / `delegate_output` / `delegate_stop` tools: immediate spawn, cancel-polling bounded wait (≤120 s, ~250 ms sleep), stop cancels; registration-time filter arg so `delegate` unregisters in OneShot/no-daemon and all three honor `DEX_SUBAGENTS=0` | spawn returns immediately; wait ends early on completion/timeout/cancel; steering acted on within one sleep interval after return; stop cancels |
| 6 | Notification queue + drain at real turn boundaries (chained follow-up + next user chat, §10b V1a) | notice text lands in the next chained `process_turn` and in the next chat's turn start |
| 7 | Policies: tool filter incl. no-delegation at dispatch, permission subset on the Phase 0 gate + detached auto-deny, model override, turn budget, timeout, `AgentResult` synthesis (§6) | denial tests pass; no escalation path; every terminal state yields a result |
| 8 | V1a events (System lines) + child sessions (§16: explicit-path constructor, listing, loader exclusion) + observability (§18) | child JSONL written with markers; resume shows children/interrupted; `seq` replay dedupes |
| 9 | Full V1a test pass + docs | §22 V1a boxes ticked; three checks green |
| 10 | **V1b:** presence-gated wake task, labeled approval prompts + turn-end guard fix + 5-min timeout, first-class event variants as an explicit wire bump | §22 V1b boxes ticked; no user-visible 409 from a wake race, old clients keep cursors moving |

---

# 22. Acceptance Criteria

## A. Core architecture
- [ ] Sub-agents use the same loop/runtime as the main agent; no duplicated LLM/tool loop.
- [ ] Definition and execution state are separate types.
- [ ] Sub-agent code sits behind one small module; removing/disabling it (`DEX_SUBAGENTS=0`) leaves the single-agent path materially unchanged.
- [ ] No new dependency; existing abstractions reused.

## B. Context isolation
- [ ] Independent context per child: task in, transcript never inherited.
- [ ] File hints and parent summary pass only when explicitly provided.
- [ ] Child working directory = parent workspace root; `resolve_workspace_path` confinement holds for every child call; parent/child resolve the same root.
- [ ] Child tool calls/messages never appear in parent history; parent loader ignores `agents/*`.

## C. Delegation (background) — V1a
- [ ] `delegate(agent, task)` returns an `AgentId` immediately; the parent turn can end while the child runs.
- [ ] `delegate_output(agent_id, wait_seconds?)` is bounded (≤120 s), polls with ~250 ms sleeps, returns early on completion/timeout/parent-cancel, and returns a structured `AgentResult`; steering is acted on within one sleep interval after the wait returns.
- [ ] `delegate_stop(agent_id)` cancels and returns the `Cancelled` result.
- [ ] Unknown names reject cleanly with the available-agent list.
- [ ] `delegate` is unregistered in OneShot/no-daemon modes via a registration-time filter (not a prompt hack).
- [ ] Multiple children spawnable from one parent turn; parallel via fan-out; cap enforced with clean rejection.

## D. Turn boundaries (V1a) and wake (V1b)
- [ ] V1a: completion notices drain only at real turn starts (chained follow-up, next user chat) — never mid-turn, never via the steering channel.
- [ ] V1a: notice queue bounded at 32; overflow folds into a summary notice.
- [ ] V1b: wake fires only when the session is idle and a real presence signal confirms an audience; never steals a race with a user chat POST (chat wins, wake skips — no user-visible 409); reattach wakes at most once.

## E. Lifecycle
- [ ] Well-defined states; `Completed`/`Failed`/`Cancelled`/`TimedOut` all reachable and tested; each queues its completion notice.
- [ ] Registry cleaned up on every terminal state; no orphaned tasks after cancel, timeout, session deletion, or daemon shutdown; `DaemonState.agents` lifecycle tested incl. restart-empty.
- [ ] TUI disconnect does not kill children; daemon restart marks them interrupted and state is recoverable from the session dir.

## F. Tool and permission isolation
- [ ] Phase 0: dispatch consults policy; mutating calls park + emit `ApprovalRequired` and block for the verdict.
- [ ] Allowlist enforced at dispatch via the explicit `ToolFilter` parameter; denied calls error with the policy reason.
- [ ] Child tools ⊆ parent tools; child permissions ⊆ parent policy on the Phase 0 gate; no escalation path.
- [ ] `delegate`/`delegate_output`/`delegate_stop` never available to children (no recursion, depth 1).
- [ ] `explorer` read-only; `reviewer` has no write/edit/bash.
- [ ] V1a: detached children auto-deny mutating calls and record the denial.
- [ ] V1b: child approval prompts surface labeled in the parent session; `AllowSession` applies to children; 5-minute timeout; the parent-turn-end guard leaves child approvals parked.

## G. Model configuration
- [ ] Per-definition model through the existing resolver; no provider-specific code in the sub-agent module.
- [ ] Works with zero user configuration (built-ins inherit or name catalog models).

## H. Concurrency
- [ ] Two+ children run concurrently; results map to correct ids; one failure doesn't terminate siblings; no shared-state corruption.

## I. Cancellation and timeout
- [ ] Parent turn cancel stops the parent (and any `delegate_output` wait) without touching children; `delegate_stop` yields `Cancelled`; timeout yields `TimedOut`; daemon shutdown cancels all; no orphans in any path.

## J. Result quality
- [ ] Every terminal state yields an `AgentResult`; `Completed` carries the child's final message; non-message endings synthesize `summary` + set actionable `error`.
- [ ] Parent consumes results and continues; child transcripts never auto-injected.

## K. Events and TUI
- [ ] V1a: lifecycle lines are seq-journaled `System` envelopes; TUI matches the stable prefix via existing poll + replay, deduped by `seq`; zero wire change.
- [ ] V1b: typed variants ship as a declared wire bump with old-client fallback.
- [ ] Core runtime runs with no TUI attached.

## L. Built-ins
- [ ] `explorer`/`reviewer`/`tester` exist with the tool sets in §19, all on the shared runtime; adding a definition requires no runtime change.

## M. Testing
- [ ] Unit: Phase 0 gate, definitions, isolation, tool policy, permissions (V1a auto-deny; V1b labels), lifecycle, cancel, timeout, notice drain at both real boundaries.
- [ ] Concurrency + parent-cancellation + child-survival tests with the mock client.
- [ ] Integration: main → delegate → child → tool → result → notice → main continues on its next turn.
- [ ] Existing suite passes; single-agent behavior has no regressions.

## N. Observability
- [ ] Unique ids; parent/child linkage recorded; durations, turns, tool-call counts, tokens recorded via existing infra; failures identify the agent; no context dumps in logs.
- [ ] Child transcripts inspectable from the session dir.

## O. Simplicity
- [ ] No second execution engine, tool framework, permission engine, or model resolver.
- [ ] No mandatory external service, user configuration, agent-to-agent messaging, shared mutable state, DAG framework, idle wake, or wire bump in V1a.
- [ ] No abstraction kept solely for hypothetical futures (manager is a struct; trait only if a second implementation appears).

---

# 23. Definition of Done

```text
User
 ▼
Dex Main Agent
 │ delegate("explorer", "Investigate authentication")   [returns id immediately]
 ▼                          parent turn ends / parent keeps working
AgentManager ── AgentInstance (explorer) ── runs across turn boundaries
 │   isolated context · read/ffgrep/fffind only · same runtime
 │   journal sink · manager-owned cancel token · detached auto-deny
 ▼
AgentResult { status: Completed, summary: "...findings in prose..." }
 ▼
notice queued → drained at the next real turn boundary (V1a; idle wake in V1b)
 ▼
Main Agent turn: "explorer finished — findings: …" → implements the fix
 │ delegate("reviewer", "Review the change")
 ▼
notice → Main Agent addresses findings → runs tests → Done
```

Success is not the number of multi-agent features. It is: dex gains real,
non-blocking delegation while keeping a simple single-agent core, isolated
contexts, dispatch-enforced tool boundaries with detached auto-deny (full
permission subset once Phase 0 + V1b land), inspectable child transcripts,
completions announced at real turn boundaries without polling, and one
obvious extension point (the manager + definition files) where user-defined
agents, labeled prompts, wake turns, and eventually deeper orchestration
can land without rework.
