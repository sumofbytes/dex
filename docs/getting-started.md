# Getting started

From zero to a working agent in about five minutes. Part of the
[dex README](../README.md) — for the full config surface see
[Configuration](configuration.md).

## 1. Install

macOS / Linux:

```sh
curl -fsSL https://raw.githubusercontent.com/sumofbytes/dex/HEAD/scripts/install.sh | sh
```

Windows: download `dex-v*-*-x86_64-pc-windows-msvc.zip` from the
[releases page](https://github.com/sumofbytes/dex/releases). Or build from
source with `cargo build --release`.

## 2. Pick a provider

`dex` works with any OpenAI-compatible API plus Anthropic's native API. You
need two things: a **model selection** and an **API key**. The easiest starts:

| Provider | Key to obtain | Model example |
| --- | --- | --- |
| Anthropic | an `ANTHROPIC_API_KEY` from [console.anthropic.com](https://console.anthropic.com) | `anthropic/claude-sonnet-4-5` |
| OpenCode Zen | an `OPENCODE_API_KEY` from [opencode.ai](https://opencode.ai) | `opencode/gpt-5-nano` |
| OpenRouter | an `OPENROUTER_API_KEY` from [openrouter.ai](https://openrouter.ai) | `openrouter/qwen/qwen3-coder-flash` |
| ChatGPT (Codex) | none — run `codex --login` once | `openai-codex/gpt-5.6-luna` |

Model ids rotate; `dex update --models` refreshes the catalog and `/model`
inside the TUI lists what's currently available. Other providers (OpenAI,
DeepSeek, Moonshot/Kimi, Groq, Mistral, …) are listed in
[Configuration](configuration.md#provider-examples).

## 3. Write the config

Create `~/.config/dex/config.yaml` (respects `$XDG_CONFIG_HOME`; `$DEX_CONFIG`
overrides the path) with a provider entry and the model selection:

```yaml
# Anthropic — key via env: export ANTHROPIC_API_KEY=sk-ant-...
model: anthropic/claude-sonnet-4-5
```

```yaml
# OpenCode Zen — key in the file:
providers:
  opencode:
    api_key: sk-...
model: opencode/gpt-5-nano
```

```yaml
# Any OpenAI-compatible endpoint outside the models.dev catalog needs its URL:
providers:
  my-gateway:
    api_key: sk-...
    base_url: https://gateway.example.com/v1
    context_window: 200000
model: my-gateway/your-model
```

Keys can also come from environment variables (`ANTHROPIC_API_KEY`,
`OPENROUTER_API_KEY`, …) — see the provider table in
[Configuration](configuration.md#provider-examples).

## 4. Verify

```sh
dex doctor
```

This prints every resolved value — provider, endpoint, model, wire protocol,
key source, permission mode — with where each came from. Exit 0 means you're
ready; exit 1 means something is unresolved (the missing row tells you which).
It never touches the network, so it's safe to run anywhere.

## 5. Start

```sh
dex
```

That launches the interactive TUI. Type a request and press Enter:

```text
> find the TODOs in src/ and fix the one in parser.rs
```

The agent reads and searches files, runs shell commands, and edits code in the
**current directory** (its workspace). Tool calls stream into the transcript as
they happen. By default no approvals are requested (`trusted`); set
`--permission ask` to confirm writes and shell commands, or `--permission
read-only` to forbid them. Each run starts a new session; `Ctrl+D` on an empty
line (or `/quit`) exits, and the TUI prints the exact command to resume that
conversation.

## Everyday usage

```sh
dex "summarize the changes since last commit"   # one-shot: answer, then exit
dex --permission ask                            # TUI with approvals
dex --model anthropic/claude-sonnet-4-5         # override the model once
dex connect http://10.0.0.5:8420                # attach to a remote daemon
```

Slash commands (`/model`, `/mode`, `/resume`, …), the `!` shell escape, and the
steering queue are covered in the [TUI guide](tui.md).

## Where things live

| What | Where |
| --- | --- |
| Config file | `~/.config/dex/config.yaml` |
| Sessions | `~/.local/share/dex/sessions/*.jsonl` |
| Model catalog cache | `$XDG_CACHE_HOME/dex/` |
| Daemon token (non-loopback) | `~/.local/share/dex/daemon.token` |
| Logs (TUI) | `dex.log` in the session's directory; `DEX_LOG=debug` for more |

## Troubleshooting

- **`dex doctor` exits 1** — the row marked `resolve ERROR` names the missing
  piece (usually a model selection or key).
- **"provider but no model"** — `model:` needs both parts:
  `provider/model`, not a bare provider name.
- **Model not in the list** — run `dex update --models`, then check `/model`.
- **Restricting what the agent can touch** — tools aren't confined to the
  workspace (`bash` could reach any path anyway); run dex in a sandbox or
  container to limit it.
- **Wrong endpoint or headers** — `dex doctor` shows the origin of every
  resolved value; [Environment variables](environment.md) lists all overrides.

## Next steps

- [Configuration](configuration.md) — providers, precedence, legacy keys
- [TUI](tui.md) — slash commands, agent modes, keyboard controls
- [Tools](tools.md) — built-in tools, sub-agents, MCP servers
- [Sessions and skills](sessions-and-skills.md) — resuming work, custom skills
- [Extensions](extensions.md) — Lua-scripted tools and hooks
