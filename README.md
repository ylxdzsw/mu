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
OpenAI-compatible endpoint, API-key env var, or configured model order.

Run a single turn by piping a prompt to `mu`, pass a prompt file directly to
`mu` for file-backed prompts and executable prompt scripts, or source the zsh
plugin for an integrated shell prompt mode that keeps using the same session
across turns. Arch Linux packaging for the current checkout lives in
[`PKGBUILD`](PKGBUILD) at the repo root.

## zsh plugin

```zsh
source /path/to/mu/mu.zsh
# Arch package install path:
# source /usr/share/zsh/plugins/mu/mu.zsh
```

The plugin requires `zsh`, `jq`, and the `mu` binary on `PATH` unless
`MU_ZSH_BIN` points at a specific executable.

Press Tab with the cursor at the beginning of the line to toggle into or out of
`mu>` mode without losing the current buffer. Press Enter to submit the current
buffer as one `mu` turn when it contains non-whitespace text; empty or
whitespace-only Enter just draws a fresh `mu>` prompt. Ctrl-C cancels the
current `mu>` draft, leaves the cancelled line in scrollback, and draws a fresh
prompt. Backspace always deletes. Ctrl-D keeps normal shell EOF behavior even in
`mu>` mode, so an empty `mu>` prompt exits the shell. Up and Down stay within
the current `mu>` buffer and never browse shell history; leave `mu>` mode first
if you want normal shell history navigation.

Shift+Enter inserts a newline without submitting when the terminal sends the
CSI-u Shift+Enter sequence (`Esc [ 13 ; 2 u`). Terminal emulators may need to
be configured to send it; if they emit ordinary Enter instead, zsh cannot
distinguish Shift+Enter from Enter.

Typing `/` at the start of a `mu>` line proactively lists slash commands.
After that, Tab uses normal zsh completion semantics, including your configured
matchers, candidate lists, and menu selection.

`/attach <file>` stages an image or audio file for the next user message and
may be repeated. It does not contact the agent or write to the session by
itself. `/attach` lists pending files, and `/attach --clear` discards them.
Pending files survive draft cancellation and leaving prompt mode, are shown as
a count in the prompt, and are consumed by the next prompt or custom command.

The plugin owns only zsh line editing and prompt mode. Each submission still
spawns the `mu` binary for one foreground turn, so streaming output, Ctrl-C, and
session persistence follow the same command-line path as scripted use.
Ctrl-D is handled as the normal terminal EOT key (`^D`); browser terminals
forward it as input when the browser has not intercepted the key first.

To keep using an existing session in zsh mode, set `MU_ZSH_SESSION_ID=<id>`
before entering `mu>` mode.

Built-in command and skill files are loaded from `/usr/share/mu` with the lowest
priority: project instructions shadow user instructions, and user instructions
shadow built-ins with the same name. This directory is instruction-only;
runtime state and configuration remain in the global or project `.mu`
directory. Built-ins with unmet command or environment requirements are not
listed to the agent.

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
| `mu prompt.md` | Run one turn from a prompt file; trims a shebang line automatically |
| `mu -s <id>` | Attach to an existing session in the active scope |
| `mu -c` | Continue the latest session in the active scope, or create one |
| `mu --model <provider>/<model>[:effort]` | Override model for this turn |
| `mu -a image.png -a audio.wav` | Attach repeatable image or audio inputs to the turn |
| `mu --output final` | Print only the final raw assistant message after the turn finishes |
| `mu --output plain` | Render sequential plain assistant/tool text |
| `mu --output terminal` | Render sequential interactive terminal output |
| `mu project inspect --path <dir>` | Report the project scope and marker resolved for a directory |
| `mu project init [--path <dir>] [--force]` | Create minimal local `.mu` metadata in the current directory or target directory |
| `mu status --json [--include-models]` | Report the resolved model, session, context state, and optional configured model list |
| `mu status --json --include-commands` | Include discovered custom command entries |
| `mu status --json --include-skills` | Include active skill entries |
| `mu session new` | Create session, print id |
| `mu session list` | List recent sessions |
| `mu session transcript --session <id>` | Print a persisted session transcript |
| `mu compact --session <id>` | Force compaction; non-terminal stdin supplies an optional focus |
| `mu retry [-s <id>] [-c]` | Resume an interrupted turn (see below) |

Prompt-file mode accepts the same turn options as stdin mode. Put the prompt
file last, for example `mu --output plain --model openai/gpt-5:high prompt.md`.
When stdin is not a terminal, its non-empty contents are appended to the prompt
file after `---`; use a quoted heredoc for custom instructions:

```sh
mu prompt.md <<'EOF'
Use a concise style and include risks.
EOF
```

Terminal stdin is not read in prompt-file mode, so `mu prompt.md` runs without
blocking. Bare `mu` still uses stdin as its complete prompt.

Manual compaction accepts the same optional custom-instruction input as a
prompt file or custom command. The instruction gives relevant material more of
the summary budget without allowing other important context to be omitted:

```sh
mu compact --session <id> <<'EOF'
Focus on the provider redesign and unresolved API decisions.
EOF
```

If a turn is interrupted (Ctrl-C, a dropped connection, a crash), only the
completed messages are kept — any tool command that started running is recorded,
including one that was killed mid-run, so already-applied changes are never
silently dropped. The session is then "unclean": you can `mu retry` to continue
that turn where it left off, or just send your next prompt to redirect (it lands
on top of the interrupted history). `mu status` reports cleanliness.

Top-level subcommands win on exact name matches. If you want to use a prompt
file named like a subcommand such as `status`, prepend `./` to disambiguate:
`mu ./status`.

Executable prompt files work directly with a shebang and `chmod +x`:

```markdown
#!/usr/bin/env -S mu --output plain
Write a concise release note for the current checkout.
```

Prompt-file mode removes the shebang line before sending the prompt to the
model. Stdin mode does not trim shebang-like input. Prompt-file mode keeps the
same working directory as the invoking shell.

The same heredoc form works for executable prompt files:

```sh
./release-note.md <<'EOF'
Mention the authentication change.
EOF
```

Files under global or project `.mu` can also act as a small instruction library.
A file whose first line is a permissive `mu` shebang is a custom command and can
be run by its relative `.mu` path, for example `mu review.md`. A file with YAML
frontmatter containing `name` and `description` is listed as a skill only when
the name matches the filename stem; `name/SKILL.md` files also qualify when the
name matches the parent directory for compatibility with the open skill spec.
Skills may also use `requires_env` and `requires_commands` frontmatter keys.
Each key accepts comma-separated entries, and all listed env vars and commands
must be available before the skill is listed.
Commands and skills are discovered with bounded depth and file-count limits.

In `mu>` mode, `/review.md Focus on authentication` runs that command with the
text after its name as the custom instruction. Shift+Enter may add more
instruction lines before submitting. A leading `/` is always slash-command
syntax; an unknown name reports an error.

## Config

Projects are discovered by upwalking from the invoking `pwd`. A project is the
nearest ancestor directory containing `.git` or `.mu`. For a git worktree with
only a `.git` pointer file, the worktree root itself is the project. If the
walk reaches your home directory or `/` without finding a project, `mu` uses
the global scope in `~/.mu`.

The invoking `pwd` stays authoritative even inside a project. Sessions record
that `pwd`, the agent sees that `pwd`, and the `bash` tool defaults to that
same `pwd`; `mu` does not silently replace it with the project root.

Global config and state live in `~/.mu`. Project config and state live in the
active project's `.mu`. Global config is loaded first; project `config.jsonc`
overrides it when a project is active. Optional `.env` files in those same
directories are also loaded with project values overriding global values; the
resulting environment is used for provider API key lookup and `bash` tool
processes.

`mu project init` creates a minimal local `.mu` scaffold in the current
directory by default, or at `--path <dir>` when provided. It writes `.mu/`,
`.mu/config.jsonc`, and `.mu/.gitignore`, but does not create an empty
`skills/` directory. By default it refuses to create a nested mu project inside
another discovered project; pass `--force` only when you explicitly want that.
Normal project use creates local runtime state only; it does not create a
project `config.jsonc`.

Providers and models are configured directly in `config.jsonc`. Model
references use the same format everywhere: `provider/model[:effort]`, where the
effort suffix is optional. A bare `model[:effort]` is also accepted when that
model id is provided by exactly one configured provider. In a scope with no
sessions yet, `mu` uses the first configured model; project config order takes
precedence over global config order.

`terminal_bell.enabled` defaults to `true`. When enabled, `mu --output terminal`
rings the terminal bell after a successful turn that ran for at least
`terminal_bell.min_duration_ms` (default `10000`).

Optional: `.env`, `AGENTS.md` (global and project-local), instruction files
under `.mu`.
Provider API key values and names listed in `redaction.env` are exact-value
redacted from `bash` tool output before it is stored or shown to the model.

Sessions live in exactly one scope: the nearest discovered project or the
global scope. Project session history is stored in `<project>/.mu/sessions.db`;
global session history is stored in `~/.mu/sessions.db`. Sessions from one
scope are not visible in another.

## Product contract

See [SPEC.md](SPEC.md) for the complete behavior and architecture.
