# mu

Fast terminal agent harness — a Rust filter binary plus shell plugins for agent mode.

## Build

```bash
cargo build --release
```

Install the binary to your `PATH` (e.g. `target/release/mu`).

## Setup

```bash
mu init                    # writes ~/.config/mu/config.jsonc
export OPENAI_API_KEY=...  # or your provider key
```

Add to `~/.zshrc` (after syntax-highlighting / autosuggestions):

```zsh
eval "$(mu init zsh)"
```

Press **Alt-M** to enter agent mode. Type a prompt and press Enter.

The zsh integration source lives at `shell-plugins/mu.zsh`; additional shell
integrations can live next to it with shell-specific suffixes.

## CLI

| Command | Description |
|---------|-------------|
| `mu` | Run one turn (prompt on stdin) |
| `mu --session <id>` | Continue an existing session |
| `mu --model <id>` | Override model for this turn |
| `mu init` | Write starter config |
| `mu init zsh` | Print zsh plugin |
| `mu session new` | Create session, print id |
| `mu session list` | List recent sessions |
| `mu compact --session <id>` | Force compaction |

## Shell functions

- `mu-new` — start fresh session
- `mu-attach <id>` — attach to session
- `mu-sessions` — list sessions
- `mu-compact` — compact current session

## Config

`~/.config/mu/config.jsonc` — see `mu init` for starter template.

Optional: `AGENTS.md` (global and project-local), `skills/*/SKILL.md`.

## Architecture

See [SPEC.md](SPEC.md) for the full design.
