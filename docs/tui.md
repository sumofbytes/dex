# TUI reference

The interactive terminal UI: slash commands, the `!` shell escape, agent
modes, keyboard controls, and the steering/follow-up queue. Launch it with
`dex` (see [Usage](../README.md#usage)).

Part of the [dex README](../README.md).

## Slash commands

| Command                      | Description                                               |
| ---------------------------- | --------------------------------------------------------- |
| `/quit`                      | Exit the REPL.                                            |
| `/permissions`               | Legacy alias of `/mode`.                                  |
| `/mode [plan\|manual\|auto]` | Show or set the agent mode (also Shift+Tab).              |
| `/mcp`                       | Show MCP servers, tools, and connection errors.           |
| `/extensions [reload]`       | Show loaded Lua extensions (`reload` rescans).            |
| `/clear`                     | Clear the conversation history (keeps the system prompt). |
| `/new`                       | Start a new session and clear history.                    |
| `/session`                   | Show the current session id, path, and turn count.        |
| `/resume [index\|path]`      | List sessions, or resume one by index/path.               |
| `/name <name>`               | Rename the current session.                               |
| `/skill:<name>`              | Load a skill's full content into the conversation.        |
| `/model`                     | Show the current model and wire protocol.                 |
| `/model <name>`              | Switch the model for the rest of the session.             |
| `/thinking [<level>\|clear]` | Show or set reasoning effort.                             |
| `/waive <reason>`            | Waive verification with a reason.                         |
| `/undo`                      | Undo the last recorded file change.                       |
| `/provider`                  | Show the current and available providers.                 |
| `/provider <name>`           | Switch provider for the rest of the session.              |
| `/help` (unknown)            | Unknown commands print a hint.                            |

## Shell escape

Prefix any TUI input with `!` to run it as a shell command directly, without
involving the agent:

```text
> !ls -la
> !!cargo test -- --nocapture
```

The command runs in the daemon workspace via the `bash` tool and renders as a
tool block. It needs no approval — the `!` itself is the approval, even in
`read-only` mode (that mode constrains the model, not your own typing) — and the
run is saved to session history: `!` output feeds the model's context on the
next turn, while `!!` stays visible in the transcript but is never sent to the
model. A shell run may overlap an agent turn; only one `!` runs at a time per
session — a second is refused until the first finishes, and `Esc`/`Ctrl+C`
cancels it. `dex "!<command>"` and `dex connect <url> "!<command>"` do the same
without the TUI.

## Agent modes

The TUI has one autonomy selector, cycled with **Shift+Tab** or set with
`/mode [plan|manual|auto]`:

| Mode | Intent | Tool gate | Model directive |
| ------- | ---------------------------------------- | --------------------------------- | -------------------------------------- |
| `plan` | Research and produce a plan; make **no** changes | `read-only` — every mutation/shell call denied | explore first, then present a plan; do not edit |
| `manual` | Author with a human in the loop | `ask` — `write`/`edit`/`bash` raise the inline approval panel | none |
| `auto` (default) | Hands-off execution | `trusted` — no prompts | none |

The default is `auto`: a stock launch seeds the mode from the permission
default (`trusted` → `auto`; a stricter `--permission`/`DEX_PERMISSION` ceiling
seeds the matching mode). Prefer a human in the loop? Set
`DEX_PERMISSION=ask` or pass `--permission ask` — that seeds `manual` and caps
the Shift+Tab cycle. The mode is journaled with
each turn, so `--reattach` and `/resume` restore the last selector instead of
reseeding from the ceiling.

The mode is a client-side selector that *derives* the per-turn permission; it
is not a second wire concept. The daemon's `--permission`/`DEX_PERMISSION` is a
**ceiling**: a client may only go stricter (`plan` ≤ `manual` ≤ `auto`), so the
cycle clamps and reports `ceiling …` rather than letting an `auto` client bypass
an `ask` daemon. Switching takes effect on the next submit. A `plan` turn also
appends a plan directive to the system prompt; already-sent turns are
unaffected.

## Keyboard controls

- **Enter** — submit the current input.
- **Tab** — autocomplete the selected slash command, provider, or model; **↑/↓**
  navigate suggestions; **Esc** — discard the draft and close the popup.
- **Shift+Enter** — insert a newline (multi-line input). Needs a terminal with
  Kitty keyboard-protocol support (e.g. Ghostty, Kitty, WezTerm, foot);
  otherwise use **Ctrl+J**, which works everywhere.
- **Enter while working** — queue a steering message for the next model
  boundary.
- **Alt+Enter while working** — queue a follow-up for after the current task.
- **Alt+Up while working** — recall queued steers (newest first), then
  follow-ups, back into the composer to edit them before delivery; press again
  for the next one.
- **Esc** or **Ctrl+C** — cancel the active turn and restore queued messages (a
  third Ctrl+C force-quits a stuck turn); **Ctrl+C** with a drafted prompt
  clears it first, and **Ctrl+D** on an empty line quits.
- **Ctrl+T** — expand/collapse the full thinking block.
- **Ctrl+O** — unfold/fold the output of successful tool calls (failures and
  write/edit diffs always show theirs).
- **Ctrl+A** — open a sub-agent's transcript in place of the main one (again
  to cycle, **Esc** to close; from the task view it switches over).
- **Ctrl+B** — open a background task's output log (a running task first;
  again to cycle, **Esc** to close; from an agent transcript it switches
  over). `/tasks <id>` opens a specific one; PgUp/PgDn, the arrows and the
  mouse wheel scroll it.
- **Shift+Tab** — cycle the agent mode: `plan` → `manual` → `auto` → `plan`
  (clamped to the daemon's `--permission`/`DEX_PERMISSION` ceiling, which it
  cannot exceed). See [Agent modes](#agent-modes).
- **Alt+V** — cycle your voice color (plain by default; magenta → sky → peach →
  violet → rose → amber → coral → plain); the composer and new prompts use it,
  already-sent rows keep theirs.
- **PageUp/PageDown**, **Shift+Up/Down**, or **mouse wheel** — scroll the
  transcript.
- **Paste** — pasted text is inserted at the cursor.
- **Mouse wheel** — scrolls the transcript (or the open sub-agent transcript or
  task log). **Drag** — selects transcript text (sub-agent transcripts too; not
  task logs) with a visible highlight and copies it to the clipboard on release
  (OSC 52; a click just clears). **Shift+drag** (Option+drag in iTerm2) still bypasses
  mouse reporting for native selection; tmux users may need
  `set -g set-clipboard on`.

## Steering and follow-ups

While `dex` is working, the input remains available. Submitted steering and
follow-up messages stay visible in the queue directly above the input box until
the worker accepts them. Steering is delivered before the next model call;
follow-ups wait until the current task has finished. The queue is kept separate
from the transcript so pending messages do not scroll away. **Alt+Up** recalls
queued steers (newest first), then follow-ups, back into the composer for
editing (the daemon drops its queued copy); a message already accepted at a
model boundary has been injected and can no longer be recalled.

## Reading the transcript

The transcript has three tiers. Your prompts (`❯ …`) and the assistant's
answers are full-contrast text. Everything the agent *does* is a compact, dim
step (`✓ read src/a.rs:1-40`) with its outcome (`lines 1-40 of 200`) on the
row beneath: `◌` while running, green `✓` when done, red `✗` on failure
(failures keep their output; `Ctrl+O` shows the rest). Every block is separated
by one blank row. Durations print only for calls over a second, and thinking shorter than 2s leaves no row.

While a turn runs the last row reads `● Working 12s · bash 3s · Esc to
interrupt` (turn time, the call in flight, its own time). It settles into
`Done in 12s · 3 tools · ↓1.2k · 14:32` (`↓` = tokens generated this turn;
the last field is the local time the turn ended, so you can tell how long the
session has been idle), or `Cancelled after …` / `Failed after …`.

Tables render as aligned columns fitted to the terminal width at the time they
stream (they do not re-fit on resize); fenced code has a faint block fill.

## Footer and `/session`

The footer keeps one row: mode, `provider/model`, cwd and branch on the left;
context pressure (`ctx 12k/128k 9.3%`, yellow at 75% of the compaction trigger,
red past it), session cost and — only when remote — `remote <host>` on the
right. Narrow terminals shed cwd, then branch, then the absolute ctx numbers,
then cost; never the mode or the model. Tokens (`↑`/`↓`), cache hit rate,
output speed, and the base-context breakdown are in `/session`.

While sub-agents or background tasks are active, an activity row appears under
the footer: `⟡ explorer·grep` per live agent, `⟳ task-3 npm test` per running
task, `✓ name` for ~10s after one finishes (then folded into a `✓N done` tally
that resets when you send a prompt), and `^A agents · ^B tasks` hints on the
right. Chips that don't fit collapse into `+N`; on a short terminal the row is
dropped first and the footer shows a compact count (`⟡2 ⟳1`) instead.
