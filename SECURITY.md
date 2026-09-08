# Security Policy

## Supported versions

Security fixes land on the default branch and ship with the next release.
Please run the latest release before reporting.

## Reporting a vulnerability

**Do not open a public issue for a security problem.**

Use GitHub's private vulnerability reporting for this repository:

  https://github.com/arpitsr/dex/security/advisories/new

(Repository → Security → Report a vulnerability.) If private reporting is
unavailable for any reason, contact the maintainer directly and say the
report is a security issue.

Include: affected version (`dex --version`), a minimal reproduction, and your
assessment of impact. Expect an initial response within a week.

## Scope notes

`dex` is an agent that runs model-directed shell commands and file edits on
your machine, under the permission mode you configure (`read-only`,
`ask-writes`, `ask-shell`, `trusted`). By design, approving a `bash` command
or running `trusted` hands the model that capability — that is the product
working as intended, not a vulnerability.

In scope:

- Escaping the workspace sandbox (`resolve_workspace_path`) via symlinks or
  path tricks from any builtin tool
- Bypassing or confusing the permission/approval model (e.g. a tool or model
  response that mutates state without the configured approval). User-typed
  `!` shell runs are not a bypass: they execute only on your explicit
  per-command action (like typing into your own terminal), while the
  permission mode constrains model-directed tools.
- Session file handling: crafted JSONL session records that cause unintended
  tool execution on resume
- Skill loading: `SKILL.md` frontmatter or skill bodies that bypass tool
  confinement or inject unintended configuration
- The local daemon (`src/daemon/`): binding, authentication between the
  client and daemon, cross-origin/SSE issues from other local processes

Out of scope: the LLM providers you configure, what the model decides to do
within the permission you granted, and vulnerabilities in dependencies
(report upstream — but feel free to CC us).
