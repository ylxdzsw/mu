---
name: mu-doc
description: Use when answering questions about Mu, configuring it, or working with its CLI, skills, commands, sessions, and built-in resources.
---

# Mu

Mu is a small, composable terminal agent: one prompt in, one completed agent
turn out. It works as a Unix command and through zsh and Fish integrations.

Project: https://github.com/ylxdzsw/mu

## Extending Mu

Skills are Markdown workflows loaded on demand. Custom commands are Markdown
prompt files with a `mu` shebang and can be invoked from the CLI or as slash
commands in shell prompt mode.

For full details, read:

- [Configuration, instructions, skills, and commands](config.md)
- [CLI reference](cli.md)

## Important paths

Mu uses project scope when the current directory belongs to a project;
otherwise it uses global scope.

- Global scope: `~/.mu`, or `$MU_CONFIG_DIR` when set.
- Project scope: `<project>/.mu`.
- Configuration: `<scope>/config.jsonc`.
- Environment values: `<scope>/.env`.
- Durable instructions: `<scope>/AGENTS.md`.
- Skills and custom commands: files under `<scope>/`.
- Session journals: `<scope>/sessions/<session-id>.jsonl`.
- Immutable attachments and provider objects: `<scope>/objects/<sha256>`.
- Last selected session: `<scope>/current-session`.
- Shipped documentation and skills: Mu's built-in directory, normally
  `/usr/share/mu`.

Session journals are append-only records, not disposable log files.
