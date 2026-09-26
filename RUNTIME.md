# Runtime composability

Dex is a Rust execution kernel plus a **runtime-defined harness graph**. The
kernel (workspace confinement, credential handling, sessions, cancellation) is
fixed; the harness — the set of components that make per-turn decisions — is
composable. You can replace, wrap, and select major harness parts at runtime,
in config or in Lua, without recompiling dex.

This document is the user-facing guide. The design background lives in the
repository's internal spec; the config surface below is authoritative for
usage. See also the [README](README.md) (§ Lua extensions) for the full hook
payload reference.

## The harness snapshot

Every turn resolves one immutable harness snapshot. The snapshot is a bundle
of **slots** — each slot is a trait object with a Rust default. The turn loop
never reads global mutable state: it asks the snapshot. That is what makes
replacement safe — a turn can never see a half-applied composition.

| Slot | Decides | Default |
|---|---|---|
| `catalog` | Which tool schemas the model sees (native + MCP + extensions) | composed catalog |
| `trigger` | Whether a turn needs compaction | config budget (`tokens > contextWindow - reserveTokens`) |
| `summarizer` | The compaction summary | deterministic checkpoint |
| `overflow` | Context-overflow detection | threshold detector |
| `conflict` | Whether a tool batch has conflicting calls | conflict detector |
| `scorer` | Which stale tool outputs to prune/drop | heuristic scorer |
| `approval` | Tool approval policy | built-in permission policy |
| `executor` | How tool calls execute | workspace-confined dispatcher |
| `transcript` | Where transcript lines go | session store |
| `usage` | Token/cost accounting | token ledger |
| `events` | Event sink | transcript events |
| `recovery` | Error recovery policy | built-in recovery |

Plus one Lua-only slot, `agent_loop`, which owns the whole turn loop (below).

## Selecting implementations: `harness.slots:`

Named implementations register under a slot and are selected by id in config:

```yaml
harness:
  slots:
    catalog: native-only   # native tools only — no MCP/extension tail
    trigger: never         # never auto-compact
```

Two alternatives ship with dex (`catalog: native-only`, `trigger: never`);
everything else has a single built-in default until you register more. The id
`default` is an explicit no-op (keeps the environment-derived default,
including `harness:`-table numerics). Unknown slots or ids `warn_once` and
keep the default — a bad selection never fails a turn.

Rust embedders add implementations with
`crate::agent::registry::register(slot, id, impl_)` (re-registering the same
slot+id replaces — the Rust form of `dex.replace`).

## Harness profiles

A profile is a named slot map applied on top of `harness.slots:`:

```yaml
harness_profile: lean        # optional: pre-select a profile
harness_profiles:
  lean:
    trigger: never
    catalog: native-only
```

A Lua extension can switch profiles at runtime with
`dex.activate_profile(name)`; the Lua activation wins over the config
selection. Activation is **transactional**: every slot and id in the profile
is validated at the next snapshot resolution — an unknown name or id applies
nothing and warns. A profile never half-applies.

## Inspection: `dex runtime graph`

```sh
dex runtime graph
```

Prints every slot's resolved id and origin (built-in default, config
`harness.slots`, or a named profile) — the same resolver the turn loop calls,
so the graph cannot drift from what a turn runs. `dex doctor` shows the same
rows plus every discovered extension with its consent state.

## Lua components

A `harness`-capability extension can implement slots in Lua. Declare them in
the manifest and they load on the same worker as `extension.lua`:

```yaml
# manifest.yaml
dex: ">=0.15"            # harness floor: older runtimes skip the extension loudly
capabilities: [harness]
components:
  model_selector: router.lua
```

```lua
-- router.lua
return function(dex)
  dex.use("model_selector", {
    id = "cost.router",
    interface = "model_selector.v1",
    run = function(input) return { model = "opencode/grok-code" } end,
  })
end
```

- `dex.use(slot, impl)` selects an implementation for a slot (`impl` is the
  handler function or `{ id, interface?, run }`).
- `dex.replace(slot, impl)` re-registers a slot+id; it also owns the
  `agent_loop` slot.
- `dex.wrap(slot, middleware)` adds middleware around an existing
  implementation.
- `dex.fallback(primary, backup)` tries the backup when the primary fails,
  then the Rust default.

A missing or failing component file fails the whole extension.

### Interface versioning

Slot implementations negotiate the payload envelope by version, checked at
registration — a component written against a different envelope fails at load
instead of misreading payloads:

| Slot | Interface |
|---|---|
| `model_selector` | `model_selector.v1` |
| `harness.summarize` | `summarizer.v1` |
| `harness.compact` | `compactor.v1` |
| `harness.overflow` | `overflow.v1` |
| `harness.conflict` | `conflict.v1` |
| `tool_catalog` | `catalog.v1` |
| `agent_loop` | `agent_loop.v1` (negotiated by `dex.replace`) |

A breaking envelope change ships as `.v2` with the old one still accepted
during migration. The manifest `dex: ">=X.Y"` pin is the harness-version
floor for the extension as a whole.

## The agent loop slot

The `agent_loop` capability unlocks the deepest slot:

```lua
dex.replace("agent_loop", { id = "my.loop", interface = "agent_loop.v1", run = function(ctx)
  while true do
    local r = ctx.model.call()
    if r.tool_calls and #r.tool_calls > 0 then
      ctx.tools.execute()
    else
      local s = ctx.finish(r.content or "")
      if not s.steered then return r.content or "" end
    end
  end
end })
```

`run(ctx)` owns iteration; Rust keeps every invariant (persistence,
streaming, cancellation, budgets, the history ledger). The step surface:
`model.call()`, `tools.execute()`, `finish(response)` (`{steered}` says
whether steering wants another round), `cancelled()`, and `state()` First
registered loop wins (load order); `dex runtime graph` shows
`agent_loop = <id>` when one is active. A loop error fails the turn — there
is no default left to fail open to — while cancellation always aborts
between Lua instructions.

## Decision hooks

Alongside slots, a `harness`-capability extension can override individual
turn decisions with hooks (first non-nil opinion wins, otherwise the Rust
default): `harness.overflow`, `harness.compact`, `harness.summarize`,
`harness.conflict`, `permission.request`, `model_selector`, and
`supervisor.route`. Observe-only hooks (`llm.before`/`llm.after`,
`tool.error`, `message.received`/`message.sent`,
`session.created`/`session.loaded`) never decide anything. Full payload
shapes: [README](README.md#lua-extensions).

## Security model

All host operations pass through Rust primitives; Lua cannot bypass them:

```text
Lua policy → Runtime policy → Kernel enforcement (authoritative)
```

- Extension manifests declare `capabilities:`; a plain `tools` extension can
  never see or decide an approval — `harness`/`agent_loop` capabilities are
  enforced at registration.
- Hooks may **deny or escalate, never clear** a host denial.
- Observation hooks fail open; policy hooks fail to the Rust default; result
  hooks may escalate a result to error, never clear one. A broken component
  never crashes the process.
- Filesystem confinement and shell gating live in the kernel, below any
  policy slot or hook.

## Embedding (Rust)

The same composition is available without Lua:

```rust
let harness = DexHarness::default()
    .with_catalog_fn(|| my_schemas())
    .with_trigger(NeverCompact)
    .with_summarizer_fn(|old, _, _, _| summarize(old));
```

`DexHarness` is one owned bundle threaded through `DexTurnHost`; the turn
loop takes it as `Option<Arc<DexHarness>>` and falls back to the registry
resolver (`from_env` + config selections) when unset.
