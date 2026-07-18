# mu

`mu` is a small, composable agent runtime for the terminal: one prompt in, one
completed agent turn out. It works equally well as a Unix command in scripts or
as an interactive assistant inside zsh.

The core stays deliberately small. Each turn starts a fresh native process,
loads its session, streams the agent and its tool activity, saves completed
messages, and exits. The shell continues to own line editing, history, job
control, and the terminal.

## Quick start

Build the binary and put it on `PATH`:

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

`mu` targets Unix-like systems and expects `bash` on `PATH`.

Packaged installations also expose two Mu-owned commands inside agent `bash`
calls: `apply_patch` for structured text edits and `view_image` for loading a
local image into the model's tool result. They are private symlinks under
`/usr/libexec/mu`, both backed by the same `mu` executable. For a source-tree
build, create equivalent sibling symlinks if you want to exercise the applets
directly:

```sh
ln -sf mu target/release/apply_patch
ln -sf mu target/release/view_image
```

Add an API key to `~/.mu/.env` (create the file if needed):

```dotenv
OPENAI_API_KEY=...
```

On first use, `mu` creates `~/.mu/config.jsonc` with a starter
OpenAI-compatible provider. Edit that file to select another endpoint, API-key
environment variable, or model.

Run a turn with a prompt:

```sh
mu <<< 'Summarize the changes in this repository.'
```

Continue the latest session for another turn:

```sh
mu -c <<< 'Now identify the riskiest change.'
```

## Interactive zsh usage

Source the included plugin from `.zshrc`:

```zsh
source /path/to/mu/mu.zsh
# Arch package: source /usr/share/zsh/plugins/mu/mu.zsh
```

The plugin requires `zsh`, `jq`, and `mu` on `PATH`. At an empty shell prompt,
press Tab to enter `mu>` mode, type a request, and press Enter. Each submission
runs one foreground `mu` turn while the plugin keeps the session connected.
Press Tab again to return to the normal shell without losing the current input.

Common prompt-mode commands include:

- `/new` starts a new session.
- `/model` selects a configured model.
- `/attach <file>` adds an image or audio file to the next turn.
- `/retry` resumes a turn interrupted by Ctrl-C, a crash, or a lost connection,
  using the model selected by `/model` when one is active.
- `/compact` compacts a long session, optionally with a focus instruction.

Typing `/` lists available commands.

## Examples

Use a specific model or attach files to a one-shot turn:

```sh
mu --model openai/gpt-5:high -a screenshot.png -a recording.wav <<"EOF"
Describe these inputs.
EOF
```

Keep reusable prompts in files:

```sh
mu review.md

mu release-note.md <<'EOF'
Emphasize compatibility and migration risks.
EOF
```

`mu` is designed to be compatible with shebang; executable prompts may select
a turn-local model with `#!/usr/bin/env -S mu --model <model>`.

Choose output for the caller:

```sh
mu --output final prompt.md       # final assistant message only
mu --output concise prompt.md     # assistant text plus one-line tool calls
mu --output detail prompt.md      # normal human transcript (default)
mu --output full prompt.md        # complete reasoning and tool details
```

`--output` controls brevity, not terminal behavior. Mu automatically enables
live lines, color, and rich Markdown when stdout is a terminal; redirected
output is sequential and ANSI-free.

Inspect sessions and resolved state with `mu session list`, `mu session
transcript --session <id>`, and `mu status --json`. Run `mu --help` for the full
CLI surface.

## Design highlights

- **A turn is the primitive.** `mu` is a fast native binary, not a daemon, TUI,
  or in-process REPL. Shell pipelines and prompt files compose it naturally.
- **Shell-native interaction.** The zsh integration adds a persistent prompt
  mode without replacing zsh or duplicating the agent runtime.
- **One universal tool.** The model sees `bash`; existing command-line tools
  provide search, editing, testing, web access, and specialized workflows.
- **Streaming, durable sessions.** Output appears as it is produced, while
  completed messages are persisted in SQLite and survive separate invocations.
- **Progressive customization.** Markdown instructions, commands, and skills
  extend behavior without a plugin SDK or additional model-visible tools.
- **Project-aware, working-directory faithful.** Configuration and sessions can
  be global or project-local, while commands continue to run from the directory
  where `mu` was invoked.

## Key features

- OpenAI-compatible Chat Completions and OpenAI Responses providers, with
  strict full-endpoint selection, ordered model selection, and
  per-turn model and reasoning-effort overrides.
- Persistent global or project-scoped sessions, continuation, transcripts,
  automatic context compaction, and interrupted-turn recovery.
- Four output densities with automatic interactive-terminal rendering.
- Image and audio attachments from both the CLI and zsh prompt mode.
- Reusable prompt files, executable prompts, slash commands, project/user
  instructions, and conditionally available skills.
- A built-in safety guardrail and exact-value redaction for configured secrets
  in `bash` output.

## Configuration and project scope

Global configuration and state live in `~/.mu`. Inside a project—the nearest
ancestor with `.git` or `.mu`—project state lives in `<project>/.mu` and project
configuration can override global defaults. The invoking working directory is
preserved for the agent and its `bash` tool.

Most repositories need no setup: `mu` discovers the project and creates only
the runtime state it needs. Use `mu project init` when you explicitly want a
local configuration scaffold, and keep project-specific guidance in
`AGENTS.md` or `.mu` instruction files.

## Reference

See [SPEC.md](SPEC.md) for the complete product contract, including exact CLI,
configuration, discovery, rendering, persistence, provider, and zsh behavior.
Arch Linux packaging for this checkout is in [PKGBUILD](PKGBUILD).

## License

`mu` is available under the [MIT License](LICENSE).
