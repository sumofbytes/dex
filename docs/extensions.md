# Lua extensions

The agent harness is runtime-composable: harness slots, profiles, and the
agent loop itself can be replaced from config or Lua without recompiling —
see [runtime](runtime.md) for the full guide.

Part of the [dex README](../README.md). Extensions register tools that appear
alongside the [built-in tools](tools.md).

Harness extensions in sandboxed Lua: drop a directory with `manifest.yaml` +
`extension.lua` into a discovery dir and it can register tools, shadow
built-ins, and subscribe to lifecycle hooks (`tool.before`/`tool.after`,
`turn.start`/`turn.end`, `before_agent_start`, `model_select`,
`session.before_compact`, plus `harness.overflow`/`harness.compact`/
`harness.conflict`/`harness.summarize`/`model_selector` when
the `harness` capability is declared; observe-only `llm.before`/`llm.after`,
`tool.error`, `permission.request` and `supervisor.route` (both require the
`harness` capability — enforced at registration, so a plain `tools` extension
can never see or decide an approval), and the read-only
`message.received`/`message.sent`/
`session.created`/`session.loaded` lifecycle events). Extension code runs in a
stripped VM — no io/os/require — and every effect flows through the same
permission gates as a model-issued call.

## Capabilities

Model-aware extensions declare the `model` capability
(`dex.model.current()` for the served `{provider, model, id, api, base_url}`,
`dex.model.auth()` for its key + endpoint + merged extra headers — or
`dex.model.auth("<provider>")` for another configured provider's deposits
(needs `net.providers`: cross-provider keys are gated like the fetches
they enable),
and `dex.model.providers()` for the configured list with resolvable keys)
and the `net` capability (`dex.net.fetch({url, method, headers, body,
timeout_ms})`, HTTP confined to that model's own endpoint —
scheme+host+port must match; `net` requires `model`). Declaring
`net.providers` additionally allows calls to any *configured provider
endpoint* — every allowed origin comes from the user's own config, never an
arbitrary host — which is how a model-independent extension (search, …)
falls back to another provider. `dex.json` encodes/decodes request
bodies. Non-2xx
is a `{status, headers, body}` value, not an error; redirects are never
followed (a 3xx surfaces as a value instead of escaping the endpoint
check); per-call `timeout_ms`
defaults to 30 s and caps at 120 s, and must sit under the tool's manifest
`timeout:` (default 30 s, cap 120 s) — provider-side search rounds run ~40 s
non-streamed, so the reference uses `timeout: 120` with `timeout_ms = 100000`.
The host re-attaches the turn's routing-affinity headers
(`x-opencode-session`/`x-opencode-client`) under Lua-explicit ones, so calls
to a gateway endpoint route like dex's own. The host fires `model_select`
(first turn always, then on `provider/model` change; fail-open) so extensions
can hide tools the model cannot serve (`dex.tools.set_active` accepts short
own-tool names). The served model is scoped to the turn — concurrent sessions
and nested subagent turns each see their own. See `examples/extensions/web` —
provider-native web search (gemini / openai-responses / anthropic) + URL fetch
(Gemini only) that reuses the current model's credentials, hides tools no
target can serve, and falls back to a `/search-model` override provider (per
extension `dex.state`) when the served model has no search API — never
switching the model silently. Copy it to
`$XDG_CONFIG_HOME/dex/extensions/web` (or `dex extensions install <dir>`) to
use it.

## Configuration

```yaml
extensions:
  paths: # extra extension dirs (user scope)
    - ~/work/dex-extensions

harness: # runtime harness numerics (env DEX_MAX_TOOL_ITERATIONS wins for iterations)
  max_tool_iterations: 200
  max_compaction_attempts: 3
  batch_max_concurrent: 10
  keep_recent_messages: 12
  min_to_summarize: 8
  recent_window: 6
  repeat_limit: 3
  keep_below_chars: 500
  truncate_above_chars: 2000
  drop_above_chars: 10000
  slots: # named registry impls; `dex runtime graph` prints the resolved graph
    # catalog: native-only # default (native + MCP + extensions) | native-only
    # trigger: never       # default (config budget) | never
```

## Harness slots

`harness.slots:` selects named registry implementations per harness slot
(the slot map, profiles, and the rest of the composability surface are
documented in [runtime](runtime.md); `dex runtime graph` prints every
slot's resolved id and origin — the same
resolver the turn loop calls, so the graph cannot drift from what a turn
runs). Unknown slots or ids `warn_once` and keep the default; Rust code can
add implementations with `crate::agent::registry::register`.

A `harness`-capability extension can also override turn decisions at
runtime (first non-nil opinion wins, otherwise the Rust default):
`harness.overflow` (`{message} -> {overflow = bool}`),
`harness.compact` (`{stored_tokens, ephemeral_overhead, message_count} ->
{compact = bool}` — the compaction gate),
`harness.summarize`
(`{conversation, previous_summary} -> {summary = "..."}` — the first
non-empty summary replaces the LLM/deterministic checkpoint; errors and
empty replies fall back to the Rust summarizer),
`harness.conflict` (`{calls} -> {conflicts = bool}`, one call per batch),
and `permission.request` (`{tool, args, requirement, mode} ->
{decision = "allow"|"deny"}`, consulted only when approval would otherwise
be required — reads stay free; `read-only` mode is never overridable).
`model_selector` (`{current, previous} -> {model = "provider/model"}` or a
bare string) picks the model one turn serves: the first non-empty opinion is
resolved like `/model` (provider switch, endpoint routing, wire protocol,
context window — but nothing persisted), and errors, empty replies, and
unresolvable selections fail open to the configured model.
`supervisor.route` (`{agent, task} -> {redirect = "name"}` and/or
`{deny = true, reason = "..."}`) gates every `delegate` spawn — first
redirect wins, any deny fails attributed to the denying extension, and a
redirect to an unknown definition falls back to the requested agent with a
loud log (fail-open).
`llm.before` may return `{append}` text persisted as a user-role note before
the compaction gate; `llm.after` (`{stop_reason, usage, elapsed_ms}`) and
`tool.error` (`{tool, args, error}`) are observe-only.
`message.received`/`message.sent` (`{session, preview, truncated, chars}` /
`{ok, preview, truncated, chars}`) and `session.created`/`session.loaded`
(`{session}`) are observe-only audit hooks — payloads carry truncated
previews, never whole prompts. Unsubscribed turns
pay no Lua cost.

## Replacing the agent loop

The `agent_loop` capability unlocks the deepest slot: `dex.replace(
"agent_loop", { id, interface? = "agent_loop.v1", run })` replaces the whole
turn loop — `run(ctx)` owns iteration while Rust keeps every invariant. The
step surface on `ctx` blocks until the host finishes each step: `model.call()`
(one engine round: persistence + streaming + cancellation + normalized
history apply; returns `{content, tool_calls = [{id, name, args}]}`),
`tools.execute()` (runs the last response's tool calls through the same
hooks/gates/dispatch a model-issued batch gets; returns `{completed, limit}`
or `{exhausted, note}`), `finish(response)` (steering injection;
`{steered = bool}` says whether to run another round), `cancelled()`, and
`state()` (`{cancelled, rounds, round_limit, messages}`). The default loop in
this surface is `while true` + `model.call` → (`tools.execute` | `finish` →
`return content`). `dex.tools.call`/`dex.net.fetch` and friends work inside
the loop under the turn's policy; a loop error fails the turn with the Lua
error (there is no default left to fail open to), and cancellation always
aborts between Lua instructions. First registered loop wins (load order);
`dex runtime graph` shows `agent_loop = <id>` when one is active.

## Discovery and management

A manifest may pin the harness it needs with `dex: ">=0.15"` — a running
harness older than the floor skips the whole extension loudly at load
instead of running it against events it never saw (only `>=` pins exist).
Components can be declared in the manifest instead of (or alongside)
`extension.lua`:

```yaml
capabilities: [harness]
components:
  model_selector: router.lua   # loaded after extension.lua, same setup contract
```

Each component file is `return function(dex) … end` and registers itself:
`dex.use(slot, impl)` selects an implementation for a harness slot —
`impl` is the handler function or `{ id, interface = "<slot>.v1", run }`;
an `interface` that doesn't match the slot's current version fails the
extension at load (`dex.replace("agent_loop", …)` owns the agent loop;
`dex.wrap` adds middleware, `dex.fallback` a backup). Components require
the `harness` capability, and a missing or failing component file fails
the whole extension.

Discovery: cwd `.dex/extensions` + `.agents/extensions` (project scope — loads
only after `dex extensions enable <id>`, the trust consent), then
`$XDG_CONFIG_HOME/dex/extensions`, config `extensions.paths:`, and
`--extensions-dir` flags (user scope — loads unless `dex extensions disable
<id>`). `dex extensions list|install|remove` manages them — `install` takes
a local directory or a git URL (`https://…` or `git@…`; plaintext `http://`
is refused, the clone is shallow, and the manifest is validated at the repo
root before anything lands). A remote install lands **disabled** — remote
code is code you have not audited — until `dex extensions enable <id>`;
`/extensions` shows
what is loaded and `/extensions reload` rescans. `dex doctor` lists every
discovered extension with its consent state.
