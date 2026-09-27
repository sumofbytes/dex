# Sessions and skills

How conversations persist as JSONL sessions, and how lightweight agent skills
are discovered and loaded. Part of the [dex README](../README.md).

## Sessions

Sessions are stored as JSONL files under:

- `$XDG_DATA_HOME/dex/sessions` (or `~/.local/share/dex/sessions`),
- organized in subdirectories by a slug of the current working directory.
- new sessions are named `<workspace>-<7 chars>` (workspace directory plus a
  k8s-style suffix, e.g. `dex-k3m9x2q`); override with `--name` or `/name`.

Each file starts with a `session` header line followed by `message` entries and
optional `session_info` (rename) entries. Entries are appended after every turn,
so a crash or Ctrl+C loses at most the in-progress turn. Starting `dex` in a
directory creates a fresh session; use `--session <path>` to continue a saved
one.

## Skills

Skills are discovered from these directories (first match wins per directory):

- `<cwd>/.dex/skills`
- `<cwd>/.agents/skills`
- `$XDG_CONFIG_HOME/dex/skills` (or `~/.config/dex/skills`)

A skill is a directory containing a `SKILL.md` file with YAML frontmatter:

```markdown
---
name: my-skill
description: Short description surfaced to the model.
---

Detailed instructions / reference content...
```

Only the `name` and `description` are included in the system prompt; the full
body is loaded on demand via `/skill:<name>` or when the conversation references
it.
