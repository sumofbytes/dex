# Configuration

How `dex` is configured: one config file, one precedence order, one selection
knob. `dex doctor` prints every resolved value with where it came from.

Part of the [dex README](../README.md) — see also
[Environment variables](environment.md) and [CLI flags](../README.md#command-line-flags).

| Layer (wins first)    | Example                                               |
| --------------------- | ----------------------------------------------------- |
| CLI flags             | `--model`, `--base-url`                               |
| Environment variables | `DEX_MODEL`, provider key vars, `DEX_HEADERS`          |
| Config file           | `$XDG_CONFIG_HOME/dex/config.yaml` (or `$DEX_CONFIG`) |

Nothing hardcodes a model *or* a provider: if no layer selects one, `dex`
refuses to start and prints a setup guide instead of guessing. (`dex doctor`
shows the same.) Built-in providers are `anthropic` (native Messages wire) and
`openai-codex` (ChatGPT OAuth); every other provider — including the opencode
gateway — is an ordinary `providers:` entry resolved through the models.dev
catalog.

`model:` is the only selection knob and names provider _and_ model:
`<provider>/<model>` — the stored key always carries a model id (an
in-session `/model anthropic` may switch just the provider; the key stays
qualified). Write-back keeps that form: a `/model` or `/provider` pick updates
`model:` in the file, so the switch becomes the default for later runs. Session
state still re-applies the exact provider/model on `/resume`.

A minimal `~/.config/dex/config.yaml` (the endpoint comes from the
models.dev catalog, so no `base_url:` is needed here):

```yaml
providers:
  opencode:
    api_key: sk-... # the deposit place for this provider's key
model: opencode/gpt-5-nano # provider / model
```

## Provider examples

Copy-paste samples for popular providers live in `examples/config.yaml`
(copy ONE block into your config file — the checked-in file keeps exactly
one active block, so uncommenting in place would leave duplicate keys).
The short version:

| Provider | `model:` | Key | `base_url:` needed? |
|---|---|---|---|
| opencode (Zen gateway) | `opencode/gpt-5-nano` | `providers.opencode.api_key` or `OPENCODE_API_KEY` | no (catalog) |
| anthropic (built in) | `anthropic/claude-sonnet-4-5` | `providers.anthropic.api_key` or `ANTHROPIC_API_KEY` | no (built in) |
| openai-codex (built in) | `openai-codex/gpt-5.6-luna` | none — run `codex --login` first | no (built in) |
| gemini | `google/gemini-3.1-pro-preview` | `providers.google.api_key` or `GEMINI_API_KEY` | yes — `https://generativelanguage.googleapis.com/v1beta/openai/` |
| openai | `openai/gpt-5-nano` | `providers.openai.api_key` or `OPENAI_API_KEY` | yes — `https://api.openai.com/v1` |
| deepseek | `deepseek/deepseek-v4-flash` | `providers.deepseek.api_key` or `DEEPSEEK_API_KEY` | no (catalog) |
| moonshot / Kimi | `moonshotai/kimi-k2.6` | `providers.moonshotai.api_key` or `MOONSHOT_API_KEY` | no (catalog) |
| openrouter | `openrouter/qwen/qwen3-coder-flash` | `providers.openrouter.api_key` or `OPENROUTER_API_KEY` | no (catalog) |
| xai (Grok) | `xai/grok-4.6` | `providers.xai.api_key` or `XAI_API_KEY` | yes — `https://api.x.ai/v1` |
| groq | `groq/openai/gpt-oss-120b` | `providers.groq.api_key` or `GROQ_API_KEY` | yes — `https://api.groq.com/openai/v1` |
| mistral | `mistral/devstral-2512` | `providers.mistral.api_key` or `MISTRAL_API_KEY` | yes — `https://api.mistral.ai/v1` |
| cerebras | `cerebras/gpt-oss-120b` | `providers.cerebras.api_key` or `CEREBRAS_API_KEY` | yes — `https://api.cerebras.ai/v1` |
| zai (Zhipu / GLM) | `zai/glm-5.3-flash` | `providers.zai.api_key` or `ZHIPU_API_KEY` | no (catalog) |
| commandcode / custom gateway | `commandcode/<model-id>` | `providers.commandcode.api_key` | yes — the gateway URL (no catalog entry) |

Notes: the provider name must match the catalog key (`moonshotai`, not
`moonshot`; gemini lives under `google`). Model ids rotate — run
`dex update --models`, then `/model` lists current ids. A gateway outside
the catalog additionally needs `context_window:` (or `DEX_CONTEXT_WINDOW`)
since nothing sizes its models.

## Model selection

The stored selection always carries a model id: a provider-only selection
(`model: anthropic`) fails with a "names a provider but no model" error —
export the provider's key and pass an id instead:
`DEX_MODEL=anthropic/<model-id> dex`. The daemon bootstraps the models.dev
catalog in the background, so a fresh install needs no manual
`dex update --models`.

Run `dex update --models` once to cache the models.dev catalog. After that a
bare `/model <id>` stays on the current provider (`provider/<id>` switches to
another), and the wire protocol follows the same way: a first `/responses`
failure falls back to chat-completions once and is remembered, so per-model
knowledge never needs
configuring. An explicit `--base-url` pins the endpoint — prefixes become naming
only and are stripped. Manual overrides are escape hatches only:
`DEX_MODEL_APIS="id=openai-completions,..."` seeds a model's protocol (full
`endpoint/id` key beats bare id). Do NOT set a global `api:` to fix one model —
it pins every model and disables the automatic fallback.

## Legacy keys

Legacy spellings map to their canonical replacement and log a one-time warning
with a pointer at it. The provider id always lives in `model:` as
`provider/model` — never in a separate key.

| Legacy key                                        | Canonical replacement                                                             |
| ------------------------------------------------- | --------------------------------------------------------------------------------- |
| top-level `base_url:`                             | `base_url:` under the provider's entry in `providers:`                            |
| top-level `api:`                                  | `api:` under the provider's entry in `providers:`                                 |
| top-level `headers:` / `http_headers:`            | `headers:` under the provider's entry (provider-scoped) or `DEX_HEADERS` (global) |
| `OPENAI_HEADERS` / `ANTHROPIC_CUSTOM_HEADERS` env | `DEX_HEADERS` (same syntax)                                                       |
| `active_provider:` / `provider:`                  | provider id in `model:` (`provider/model`)                                        |

Unknown keys are called out by name (`dex: unknown config key(s) ...`) and a
parse error lists the valid keys: `model`, `providers`, `context_window`,
`thinking_effort`, `system_prompt`, `system_prompt_file`, `mcp_servers`,
`agent_wake`, `cache_warming`, `extensions` (+ the legacy
ones above). Other keys are preserved untouched.

## System prompt

`system_prompt:` (inline text) or `system_prompt_file:` (path to a file)
replaces the built-in base prompt (identity + working rules). Project
instructions (`AGENTS.md`/`CLAUDE.md`), extension appendix, and skills are
appended in addition; subagent children keep their own persona and rules.

`dex serve` ignores CLI flags — the daemon falls back to its own env/file
layers unless the client forwards per-request text. Empty or whitespace-only
values count as unset at every layer and fall through.

Precedence: `--system-prompt` > `--system-prompt-file` > `DEX_SYSTEM_PROMPT` >
`DEX_SYSTEM_PROMPT_FILE` > file `system_prompt:` > file `system_prompt_file:` >
built-in default. `dex doctor` shows the resolved source as `system prompt`.

## Other OpenAI-compatible providers

Any models.dev provider with an OpenAI-style endpoint works without dedicated
integration. Deposit its key under `providers:` and pick it by name:

```yaml
model: zai/glm-5.3-flash
providers:
  zai:
    api_key: zsk-... # the deposit place; or export ZHIPU_API_KEY
    # base_url: ...        # optional; defaults to the catalog endpoint
    # api: openai-completions  # optional protocol pin; learned otherwise
    # headers: {X-Custom: ...} # optional; sent only to this provider
```

Anthropic is built in as its own provider — `model: anthropic/claude-sonnet-4-5`
(or `--model anthropic`) with `ANTHROPIC_API_KEY`. It speaks the native Messages
wire (`anthropic-messages`: `x-api-key` auth, `anthropic-version` header,
block-shaped tool calls and thinking blocks replayed with their signatures). Any
other provider entry can also pin that wire with `api: anthropic-messages` when
its endpoint speaks the Messages API:

```yaml
providers:
  gateway:
    api_key: ...
    base_url: https://gateway.example/v1
    api: anthropic-messages
```

`/provider zai` and `/model zai/<id>` switch to it (the completion list shows
`zai/<id>` once configured). The endpoint, model list, pricing, context windows
and reasoning options come from the cached models.dev catalog — run
`dex update --models` once. The key resolves per provider: config
`providers.<name>.api_key` > the provider's own documented env var (from the
catalog, e.g. `ZHIPU_API_KEY`, `OPENROUTER_API_KEY`, `OPENCODE_API_KEY`). There
is no per-provider default key var outside the catalog
— one provider's key never leaks into another. A model's advertised thinking
options (e.g. `low/high/max`) are shown in the `/model` confirmation;
`/thinking <level>` pins one per model (remembered per endpoint+model and
validated against the advertised list — unknown models accept anything, a stale
catalog never blocks). `DEX_THINKING_EFFORT` is the fallback when nothing is
pinned, and an effort no model advertises warns once instead of failing opaquely
at the API. A file `thinking_effort:` default sits under both (stored choice >
env > file). Wire protocol resolves like any OpenAI-compatible provider:
responses first, one fallback
to completions, remembered per endpoint+model. Native-protocol-only providers
(no OpenAI-compatible endpoint in the catalog, e.g. anthropic) are not
selectable this way.

For ChatGPT-backed Codex, first run `codex --login`, then:

```sh
DEX_MODEL=openai-codex/gpt-5.6-luna dex
```

Replace `gpt-5.6-luna` with the model id you want to use. A bare provider
name such as `DEX_MODEL=openai-codex` does not select a model and will fail
with a “provider but no model” error.

`dex` reads the current access token and account ID from
`CODEX_ACCESS_TOKEN`/`CODEX_ACCOUNT_ID` or `$CODEX_HOME/auth.json` (default
`~/.codex/auth.json`). Run `codex --login` again when the local token expires.
