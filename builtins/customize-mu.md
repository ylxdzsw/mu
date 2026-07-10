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
mu status --json --include-models --include-commands
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

- Config: `~/.mu/config.jsonc`, then project `.mu/config.jsonc` as a deep
  overlay.
- Environment: process environment, then `~/.mu/.env`, then project `.mu/.env`.
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
`mu project init` is only an overlay stub.

Common shape:

```jsonc
{
  "providers": {
    "openai": {
      "base_url": "https://api.openai.com/v1",
      "api_key_env": "OPENAI_API_KEY",
      "models": {
        "gpt-4o": {
          "context_window": 128000,
          "supported_efforts": ["low", "medium", "high"]
        }
      }
    }
  },
  "terminal_bell": { "enabled": true, "min_duration_ms": 10000 },
  "compaction": { "fraction": 0.75, "keep_recent_turns": 2 },
  "limits": {
    "max_iterations": 50,
    "max_lines": 2000,
    "max_bytes": 51200,
    "max_line_bytes": 10240
  },
  "redaction": { "env": [] },
  "guardrail": {
    "enabled": true,
    "review_model": "openai/gpt-4o:low",
    "timeout_ms": 90000,
    "circuit_breaker": { "consecutive": 3, "window": 50, "window_denials": 10 }
  }
}
```

Model references use `provider/model[:effort]`. Bare `model[:effort]` is valid
only when exactly one configured provider has that model id. Supported effort
values are `low`, `medium`, `high`, `xhigh`, and `max`.

Provider and model object order matters. In a scope with no existing sessions,
`mu` uses the first configured model after project order is merged over global
order.

Use `.env` for secrets and environment-specific values. Provider API key values
and exact values of names listed in `redaction.env` are redacted from bash tool
output before storage and before the model sees them.

## AGENTS.md

`AGENTS.md` is durable guidance appended to the system prompt. Global
`~/.mu/AGENTS.md` loads first. Project `.mu/AGENTS.md` loads after it and should
hold repository-specific conventions, verification commands, review rules, and
other guidance that should apply on every turn in that project.

Keep `AGENTS.md` short. Put reusable task workflows in skills instead.

## Commands

A custom command is any valid instruction file whose first line is a common variant
of `mu` shebang:

```markdown
#!/usr/bin/env -S mu --output plain
Summarize the current checkout and suggest the next release note.
```

Commands are invoked by their relative `.mu` path, including extension, for
example `mu review.md` or `/review.md` in the zsh prompt mode. Built-in
subcommands and explicit prompt paths such as `./status` win over command names.

Every prompt file can take an optional custom instruction from non-terminal
stdin. Prefer a quoted heredoc for multiline text:

```sh
mu review.md <<'EOF'
Focus on authentication and authorization.
EOF
```

For file-backed turns, terminal stdin is not read, and an empty pipe leaves the
file prompt unchanged. Non-empty stdin is appended after `---`. In zsh prompt
mode, `/review.md Focus on authentication` passes the text after the command as
that custom instruction; Shift+Enter may add more lines.

Prompt-file mode strips the shebang before sending the prompt. If a command also
has skill frontmatter, `mu` strips both the shebang and frontmatter for command
execution.

## Skills

A skill is an instruction file with YAML frontmatter containing `name` and
`description`. `mu` injects only skill metadata into the system prompt. There is
no skill tool; the agent reads the skill file on demand with normal `bash`
commands such as `sed`, `cat`, or `rg`.

Flat file form:

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
must resolve before `mu` lists the skill. Do not use requirements to replace a
clear trigger description.

Folder form:

```text
.mu/my-skill/SKILL.md
```

The skill name must match the flat file stem or the parent directory of
`SKILL.md`. Names are lowercase ASCII letters or digits plus `_` and `-`.
Descriptions should say both what the skill does and when it should trigger.

## Project initialization

`mu project init` creates a minimal project scope:

- `.mu/`
- `.mu/config.jsonc`
- `.mu/.gitignore`

It intentionally does not create `.env`, `AGENTS.md`, or `sessions.db`. By default
it refuses to create a nested `mu` project inside another discovered project; use
`--force` only when the user explicitly wants a nested project.

## Verification

After editing `mu` setup, prefer cheap structured checks:

```bash
mu status --json --include-models --include-commands
mu status --json --include-skills
```
