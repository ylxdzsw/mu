---
name: customize-mu
description: Use ONLY when editing mu setup files: ~/.mu/*, custom commands, skills, or built-in/global/project instructions. Do not use for application code.
---

# Customizing mu

Use this when the task is to change how `mu` itself behaves for the user:
configuration, environment overlays, durable instructions, custom commands, or
skills. Do not use it for ordinary application-code changes unless the user is
also changing `mu` setup files.

## First steps

Before editing, inspect the active setup instead of guessing:

```bash
mu status --json --include-models --include-commands --include-skills
```

Then read the relevant files from the active scopes:

- Global config directory: `~/.mu`, or `$MU_CONFIG_DIR` when set.
- Project config directory: `<project>/.mu` when the current directory resolves
  to a project.
- Built-ins: `/usr/share/mu`.

`mu` discovers a project by walking upward from the invoking `pwd` until it
finds `.mu` or `.git`. The walk stops before the user's home directory and
before filesystem root. If no project is found, `mu` uses global scope.

## Precedence

- Config: global `config.jsonc`, then project `.mu/config.jsonc` as a deep
  overlay.
- Environment: process environment, then global `.env`, then project `.mu/.env`.
- Instruction index: built-ins, then global `.mu`, then project `.mu`; later
  scopes shadow earlier skills or commands with the same name.
- Prompt guidance: role preamble and runtime context, then available skill
  metadata, then global `AGENTS.md`, then project `AGENTS.md`.

Use project files for repository-specific behavior. Use global files for the
user's personal defaults. Avoid editing built-ins unless the user is changing
the installed `mu` package or this repository's shipped defaults.

## Config JSONC

`config.jsonc` accepts comments and trailing commas. Global config is created
automatically with a starter provider if it does not exist. Project config from
`mu project init` is only an overlay stub. Omitted non-provider fields inherit
from bundled defaults; bundled providers are used only to create a missing
global config. Objects merge recursively across scopes; scalars and arrays
replace inherited values.

Complete shape, with default values where applicable:

```jsonc
{
  "output": "concise",            // final | concise | detail | full
  "line_wrapping": true,          // interactive output; no CLI override
  "providers": {
    "openai": {
      "endpoint": "https://api.openai.com/v1/responses", // complete POST URL
      "api_key_env": "OPENAI_API_KEY",                   // empty means no key
      "models": {
        "gpt-5.6-terra": {
          "context_window": 1050000, // compaction and status denominator
          "supported_efforts": ["none", "low", "medium", "high", "xhigh", "max"],
          "replay_key": "openai-gpt-5.6" // optional native-replay group
        }
      }
    }
  },
  "terminal_bell": { "enabled": true, "min_duration_ms": 10000 },
  "compaction": {
    "enabled": true,
    "soft_fraction": 0.70,
    "hard_fraction": 0.85,
    "hard_headroom_tokens": 48000
  },
  "limits": {
    "max_iterations": 50,
    "max_lines": 2000,
    "max_bytes": 51200,
    "max_line_bytes": 10240
  },
  "redaction": { "env": ["*_API_KEY", "*_API_TOKEN", "*_AUTH_TOKEN"] },
  "guardrail": {
    "enabled": true,
    "review_model": "openai/gpt-5.6-terra:low", // optional; active model if omitted
    "timeout_seconds": 120,
    "max_denials_per_turn": 3
  }
}
```

At least one provider and model are required. Endpoint paths must end in
`/chat/completions`, `/responses`, or `/messages`; the suffix selects the API.
`endpoint` is required; `api_key_env` and all model metadata are optional.
Without `context_window`, percentage reporting and proactive compaction are
unavailable. `output` is overridden by CLI `--output`.
Set `compaction.enabled` to `false` to disable automatic soft-threshold,
hard-threshold, and context-overflow compaction. Context-length failures then
abort the turn, while explicit `mu compact` remains available.
Compaction requires a model `context_window` and runs only when context tokens
are strictly greater than the applicable threshold. Before a new turn's first
provider request, `compaction.soft_fraction` sets the graceful threshold to
`floor(context_window * soft_fraction)`. Before each semantic request within a
turn, the hard threshold is the lower of
`floor(context_window * hard_fraction)` and
`context_window.saturating_sub(hard_headroom_tokens)`. Compaction keeps the
smallest suffix of whole turns containing at least five requests, counting each
submitted prompt and each Bash tool call as one request. This retains five
tool-free turns, while a current turn with five tool calls satisfies the budget
by itself.
`limits.max_iterations` caps one agent turn; the other limits bound the
model-visible preview of bash output by lines, total bytes, and bytes per line.
The bell sounds only for turns lasting at least `min_duration_ms`. The guardrail
reviews destructive bash calls; `timeout_seconds` and `max_denials_per_turn`
must be positive.

Model references use `provider/model[:effort]` for a fixed provider or
`model[:effort]` for ordered provider fallback. A bare model includes every
provider defining that model id; a session remembers its position per model.
Provider and model object order controls the default model, fallback, status,
and completion order, with project entries before inherited global entries.
Effort strings are provider-defined and unrestricted; `supported_efforts` is
only an ordered status/completion hint. An exact model id containing `:` wins
before effort-suffix parsing.

`replay_key` is a non-empty, non-secret compatibility label, defaulting to
`provider/model`. Responses and Anthropic native state is shared only within
the same API and effective key; Chat Completions replay is API-wide.

## Environment

Use `.env` for secrets and host-specific values. Mu parses it as data, never as
shell code:

```text
("export" whitespace)? NAME "=" VALUE
NAME  = [A-Za-z_][A-Za-z0-9_]*
VALUE = bare [A-Za-z0-9_./:@%+,=-]*, single-quoted, or double-quoted
```

Blank lines and full-line comments are accepted; assignments cannot be
indented or contain spacing around `=`, inline comments, or trailing syntax.
Single quotes are literal. Double quotes allow only `\"`, `\\`, `\$`, and
escaped backticks; expansion, concatenated quoting, multiline values, and other
shell syntax are rejected. LF/CRLF and a final line without newline are valid.
Invalid UTF-8, NUL, and lone carriage returns are rejected. Each file is applied
atomically and its last duplicate wins.

The effective environment is passed to bash and used for API keys and skill
requirements. Values named by `api_key_env` or `redaction.env` are redacted
before bash output is stored or shown to the model. Redaction selectors are
case-sensitive exact names or one leading `*` plus a non-empty suffix; `[]`
disables the defaults, and empty selected values are ignored.

## AGENTS.md

`AGENTS.md` is durable guidance appended to the system prompt. Global
`~/.mu/AGENTS.md` loads first. Project `.mu/AGENTS.md` loads after it and should
hold repository-specific conventions, verification commands, review rules, and
other guidance that should apply on every turn in that project.

Keep `AGENTS.md` short. Put reusable task workflows in skills instead.

## Commands

A custom command is a regular instruction file whose first line contains a
common `mu` shebang:

```markdown
#!/usr/bin/env -S mu --model openai/gpt-5:high
Summarize the current checkout and suggest the next release note.
```

The shebang accepts no arguments or exactly `-m|--model <model-ref>` as separate
tokens; other arguments and `--model=value` are rejected on invocation. Use
`env -S` with a model. An invocation `--model` overrides the shebang, which
otherwise overrides the attached session or configured default for that turn
without rewriting session model state.

Commands are invoked by their relative `.mu` path, including extension, for
example `mu review.md` or `/review.md` in zsh or Fish prompt mode. They accept
the normal turn options for session, model, attachments, and output. Built-in
subcommands win exact name collisions. Absolute, `./`, and `../` targets are
explicit prompt files and bypass command lookup.

Use `mu cat review.md` to inspect the resolved command or prompt without a
provider or session mutation. Interactive output includes provenance; redirected
output is the exact composed prompt.

Every prompt file can take an optional custom instruction from non-terminal
stdin. When calling it through the Bash tool, prefer the tool's `stdin` argument
over shell redirection:

```ts
bash({
  title: "Run custom review command",
  risk: "readonly",
  command: "mu review.md",
  stdin: "Focus on authentication and authorization."
})
```

For a human invoking `mu` directly from a terminal, a quoted heredoc remains
appropriate for multiline input.

For file-backed turns, terminal stdin is not read, and an empty pipe leaves the
file prompt unchanged. Non-empty stdin is appended verbatim after
`\n---\n\n`. In shell prompt mode, `/review.md Focus on authentication` passes
the trailing text as that instruction; Shift+Enter may add lines. Pending shell
attachments are forwarded and consumed by a custom command.

Prompt-file mode strips the shebang before sending the prompt. A `mu` shebang's
model default applies equally to an explicit prompt path and a discovered
command. A file may be both command and skill; command execution strips both
headers. Command files need not have executable permission.

## Skills

A skill is a regular instruction file with optional `mu` shebang followed by
YAML-style frontmatter. The parser supports single-line `name`, `description`,
`requires_env`, and `requires_commands` scalars; it is not general YAML.

Prefer a flat file when the skill consists only of instructions:

```text
.mu/my-skill.md
```

```markdown
---
name: my-skill
description: Use when the user asks for a focused workflow.
requires_env: API_TOKEN, ORG_ID
requires_commands: gh, jq
---

Workflow instructions.
```

Use `requires_env` when a skill only works with specific environment variables,
and `requires_commands` when it needs CLIs on `PATH`. Each key is optional and
comma-separated; every listed env var must be non-empty and every listed command
must be an executable regular file on the effective `PATH`. Environment names
use shell identifier syntax; command names allow ASCII letters, digits, `_`,
`-`, and `.`, but no `/`. Requirements are AND-only and gate only the skill
role: a command in the same file remains callable. An inactive higher-scope
skill does not shadow an active lower-scope skill.

Use the folder form when the skill bundles supporting scripts, references,
examples, or assets, or when external Open Skills compatibility is an explicit
goal. Do not create a directory merely to hold one `SKILL.md`:

```text
.mu/my-skill/SKILL.md
```

The skill name must match the flat file stem or the parent directory of
`SKILL.md`. It is 1-64 bytes, starts with a lowercase ASCII letter or digit, and
otherwise allows those characters plus `_` and `-`. Description is 1-256 bytes
and should say what the skill does and when it triggers.

Mu injects only active skill name, description, and absolute path. Before
responding, the agent scans that list and must read a named or even partially
relevant skill in full. Loading is context acquisition, not mandatory
obedience; the agent decides whether its instructions apply. Relative paths in
a skill resolve from the skill file's directory.

Discovery scans built-in, global, then project roots; later active entries
shadow the same skill name or command path. Paths are ASCII alphanumeric plus
`_`, `-`, and `.`, with no component starting `.` or `-`; symlinks are not
followed. Reserved root entries are `cache`, `locks`, `sessions`, `objects`,
`current-session`, `sessions.db*`, `config.jsonc`, `.env`, `.gitignore`, and
`AGENTS.md`. Each root scans at depth 4 and at most 512 files; the merged
alphabetical index exposes at most 64 skills and 256 commands.

In skill examples, pass multiline or escaping-sensitive command input through
the Bash tool's `stdin` argument. Keep it out of the command string and omit
`stdin` when the command needs no input.

## Project initialization

`mu project init` creates a minimal project scope:

- `.mu/`
- `.mu/config.jsonc`
- `.mu/.gitignore`

It intentionally does not create `.env`, `AGENTS.md`, `sessions/`, or
`objects/`; runtime state appears only when Mu needs it. By default it refuses
to create a nested `mu` project inside another discovered project; use
`--force` only when the user explicitly wants a nested project.

## Verification

After editing `mu` setup, prefer cheap structured checks:

```bash
mu status --json --include-models --include-commands --include-skills
mu context
mu cat <prompt-file-or-command>
```
