# mu

Small composable agent runtime: one prompt on stdin, one completed agent turn out.

## Build

```bash
cargo build --release
```

Install the binary to your `PATH` (e.g. `target/release/mu`).

`mu` targets Unix-like systems and expects `bash` to be available on `PATH`.

## Setup

```bash
export OPENAI_API_KEY=...  # or your provider key
```

`mu` creates `~/.mu/config.jsonc` automatically with an OpenAI-compatible
starter provider if the file does not exist. Edit that file to use another
OpenAI-compatible endpoint, API-key env var, or default model.

Run a single turn by piping a prompt to `mu`, or source the zsh plugin for an
integrated shell prompt mode that keeps using the same session across turns.
Arch Linux packaging for the current checkout lives in [`PKGBUILD`](PKGBUILD)
at the repo root.

## zsh plugin

```zsh
source /path/to/mu/mu.zsh
# Arch package install path:
# source /usr/share/mu/mu.zsh
```

Press Tab with the cursor at the beginning of the line to toggle into or out of
`mu>` mode without losing the current buffer. Press Enter to submit the current
buffer as one `mu` turn when it contains non-whitespace text; empty or
whitespace-only Enter just draws a fresh `mu>` prompt. Ctrl-C cancels the
current `mu>` draft, leaves the cancelled line in scrollback, and draws a fresh
prompt. Backspace always deletes. Ctrl-D keeps normal shell EOF behavior even in
`mu>` mode, so an empty `mu>` prompt exits the shell. Press Up in `mu>` mode to
detour through normal shell history recall; when you return to the saved draft,
Down re-enters `mu>` mode with that draft restored.

The plugin owns only zsh line editing and prompt mode. Each submission still
spawns the `mu` binary for one foreground turn, so streaming output, Ctrl-C, and
session persistence follow the same command-line path as scripted use.
Ctrl-D is handled as the normal terminal EOT key (`^D`); browser terminals such
as xterm.js/WebTerm forward it as input when the browser has not intercepted the
key first.

To keep using an existing session in zsh mode, set `MU_ZSH_SESSION_ID=<id>`
before entering `mu>` mode.

While `mu>` mode is active, the plugin automatically suspends common editor
helpers such as `zsh-syntax-highlighting` and `zsh-autosuggestions`, then
restores their prior state when you return to the shell prompt. For any other
conflicting plugin, register zsh functions in `MU_ZSH_ENTER_HOOKS` and
`MU_ZSH_EXIT_HOOKS`; the enter hooks run after `mu>` mode is activated, and the
exit hooks run after the shell prompt has been restored.

```zsh
mu_disable_conflicts() {
  my-plugin-disable
}

mu_restore_conflicts() {
  my-plugin-enable
}

MU_ZSH_ENTER_HOOKS+=(mu_disable_conflicts)
MU_ZSH_EXIT_HOOKS+=(mu_restore_conflicts)
```

## CLI

| Command | Description |
|---------|-------------|
| `mu` | Run one turn (prompt on stdin) |
| `mu -s <id>` | Attach to an existing session in the active scope |
| `mu -c` | Continue the latest session in the active scope, or create one |
| `mu --model <id>` | Override model for this turn |
| `mu --effort <low|medium|high|xhigh|max>` | Override reasoning effort for this turn |
| `mu -i image.png` | Attach an image to the turn |
| `mu --output plain` | Render plain assistant/tool text |
| `mu --output terminal` | Render interactive terminal output |
| `mu --output json` | Render newline-delimited JSON events |
| `mu status --json` | Report the resolved model, effort, session, and context state |
| `mu models refresh` | Refresh `~/.mu/models.json` from the active provider |
| `mu models list [--json]` | Inspect the cached provider model catalog |
| `mu session new` | Create session, print id |
| `mu session list` | List recent sessions |
| `mu compact --session <id>` | Force compaction |

## Config

Global config and state live in `~/.mu`. Project config and state live in
`.mu` beside the nearest `.git` or existing `.mu` project marker. Global config
is loaded first; project `config.jsonc` overrides it when a project is active.
Optional `.env` files in those same directories are also loaded with project
values overriding global values; the resulting environment is used for provider
API key lookup and `bash` tool processes.

User intent stays in `config.jsonc`. Generated model discovery is cached in
`~/.mu/models.json` and can be refreshed with `mu models refresh`; it never
rewrites the hand-authored config file.

Optional: `.env`, `AGENTS.md` (global and project-local), `skills/*/SKILL.md`.
Provider API key values and names listed in `redaction.env` are exact-value
redacted from `bash` tool output before it is stored or shown to the model.

Sessions are selected from exactly one scope: project sessions when inside a
project, global sessions otherwise. Project session history is stored in
`.mu/sessions.db` and ignored by the generated `.mu/.gitignore`.

## Architecture

See [SPEC.md](SPEC.md) for the current design.
