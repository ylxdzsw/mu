# mu

Small composable agent runtime: one prompt on stdin, one completed agent turn out.

## Build

```bash
cargo build --release
```

Install the binary to your `PATH` (e.g. `target/release/mu`).

## Setup

```bash
mu init                    # writes ~/.mu/config.jsonc
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
| `mu -s <id>` | Attach to an existing session in the active scope |
| `mu -c` | Continue the latest session in the active scope, or create one |
| `mu --model <id>` | Override model for this turn |
| `mu -i image.png` | Attach an image to the turn |
| `mu --output plain` | Render plain assistant/tool text |
| `mu --output terminal` | Render interactive terminal output |
| `mu --output json` | Render newline-delimited JSON events |
| `mu-cli` | Run the thin interactive REPL wrapper |
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

Global config and state live in `~/.mu`. Project config and state live in
`.mu` beside the nearest `.git` or existing `.mu` project marker. Global config
is loaded first; project `config.jsonc` overrides it when a project is active.

Optional: `AGENTS.md` (global and project-local), `skills/*/SKILL.md`.

Sessions are selected from exactly one scope: project sessions when inside a
project, global sessions otherwise. Project session history is stored in
`.mu/sessions.db` and ignored by the generated `.mu/.gitignore`.

## Architecture

See [SPEC.md](SPEC.md) for the current design.
