# mu

`mu` is a small, composable agent for the terminal: one prompt in, one
completed agent turn out. It works equally well as a Unix command in scripts or
as an interactive assistant inside zsh.

## Quick start

Build the binary and put it on `PATH`:

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"
```

Now ask it something:

```sh
mu <<< 'Summarize the changes in this repository.'
```

That works with no setup and no API key. Out of the box `mu` uses a free model
from [OpenCode Zen](https://opencode.ai/zen/), so you can try it immediately
after building. Bring your own provider whenever you want (see
[Using your own provider](#using-your-own-provider)).

Continue the latest session for another turn:

```sh
mu -c <<< 'Now identify the riskiest change.'
```

`mu` targets Unix-like systems and expects `bash` on `PATH`.

## Interactive zsh usage

The most comfortable way to use `mu` is right inside your shell. Source the
included plugin from `.zshrc`:

```zsh
source /path/to/mu/mu.zsh
# Arch package: source /usr/share/zsh/plugins/mu/mu.zsh
```

At an empty shell prompt, press **Tab** to enter `mu>` mode, type a request, and
press **Enter**:

```
mu> what changed in the last three commits?
```

Each submission runs one foreground `mu` turn while the plugin keeps the session
connected. Press Tab again to return to the normal shell without losing your
input, so `mu` and your usual commands share one prompt. The shell keeps owning
line editing, history, and job control.

Type `/` to list prompt-mode commands. The common ones:

- `/new` starts a new session.
- `/model` selects a configured model.
- `/attach <file>` adds an image or audio file to the next turn.
- `/retry` resumes a turn interrupted by Ctrl-C, a crash, or a lost connection.
- `/compact` compacts a long session, optionally with a focus instruction.

The plugin requires `zsh`, `jq`, and `mu` on `PATH`.

## More ways to run a turn

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

`mu` is compatible with shebang lines, so an executable prompt can select its own
model:

```sh
#!/usr/bin/env -S mu --model openai/gpt-5:high
```

Choose how much the caller sees:

```sh
mu --output final prompt.md       # final assistant message only
mu --output concise prompt.md     # assistant text plus one-line tool calls
mu --output detail prompt.md      # normal human transcript (built-in default)
mu --output full prompt.md        # complete reasoning and tool details
```

Inspect sessions and resolved state with `mu session list`, `mu session
transcript --session <id>`, and `mu status --json`. Run `mu --help` for the full
CLI surface.

## How it works

The core stays deliberately small. Each turn starts a fresh native process,
loads its session, streams the agent and its tool activity, saves completed
messages, and exits. A few ideas follow from that:

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
  be global or project-local, while commands run from the directory where `mu`
  was invoked.

## Key features

- OpenAI-compatible Chat Completions and OpenAI Responses providers, with
  strict full-endpoint selection, ordered model selection, and per-turn model
  and reasoning-effort overrides.
- Persistent global or project-scoped sessions, continuation, transcripts,
  automatic context compaction, and interrupted-turn recovery.
- Four output densities with automatic interactive-terminal rendering.
- Image and audio attachments from both the CLI and zsh prompt mode.
- Reusable prompt files, executable prompts, slash commands, project/user
  instructions, and conditionally available skills.
- A built-in safety guardrail and exact-value redaction for configured secrets,
  with exact or suffix-based environment-variable selectors, in `bash` output.

## Sharing mu's context with other agents

`mu context` introspects the agent context and has two modes. On its own it
prints the assembled system prompt mu itself would use — role preamble, runtime
block, the full skills index, and your merged `AGENTS.md` — so you can see
exactly what a new session receives:

```sh
mu context             # the system prompt mu itself would use (inspection)
mu context --export    # a portable projection for another agent to ingest
```

`--export` instead emits a projection tailored for a *foreign* agent: a short
preamble explaining the content was authored for mu, followed by your own merged
`AGENTS.md` and your non-built-in skills. The role preamble, runtime block, and
built-in skills are left out. Neither mode contacts a provider, and scope
resolves from the working directory like `mu status`.

Because `--export` re-reads your instructions and skills on every call, it stays
current with no separate sync step. In a project with no user `AGENTS.md` and no
user skills it prints nothing, so it is safe to wire up unconditionally.

For Claude Code, run it from a `SessionStart` hook so each new session ingests
your mu context. Add to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "mu context --export" }] }
    ]
  }
}
```

The export preamble tells the agent the guidance was written for mu (whose only
tool is `bash`) so it adapts the intent to its own richer toolset — for example,
reading a skill file with its file tools rather than a shell — and points it at
mu's `customize-mu` reference if it wants the full configuration contract.

## Using your own provider

On first use, `mu` creates `~/.mu/config.jsonc`. It ships with two providers:
the keyless OpenCode Zen free model used by default, and a commented OpenAI
example. To use a keyed provider, add its API key to `~/.mu/.env` (create the
file if needed):

```dotenv
OPENAI_API_KEY=...
```

Then select it per turn with `mu --model openai/gpt-4o`, or reorder the
providers in `config.jsonc` so yours comes first and becomes the default. Any
OpenAI-compatible endpoint works; edit the endpoint, API-key environment
variable, and model list to match your provider.

## Configuration and project scope

Global configuration and state live in `~/.mu`. Inside a project—the nearest
ancestor with `.git` or `.mu`—project state lives in `<project>/.mu` and project
configuration can override global defaults. The invoking working directory is
preserved for the agent and its `bash` tool.

Most repositories need no setup: `mu` discovers the project and creates only the
runtime state it needs. Use `mu project init` when you explicitly want a local
configuration scaffold, and keep project-specific guidance in `AGENTS.md` or
`.mu` instruction files.

Mu keeps one SQLite database per active scope: `<project>/.mu/sessions.db` in a
project, or `~/.mu/sessions.db` globally (with the usual SQLite WAL/SHM
sidecars). `.mu` contains that database family and authored files only. Session
ownership lives in the database; ephemeral oversized-output spills use
exclusive random files in the private OS temporary directory `$TMPDIR/mu`, and
image attachments are stored directly in SQLite.

Setting `"output": "concise"` in global or project `config.jsonc` changes the
default output density; an explicit `--output` always wins. Output density
controls brevity, not terminal behavior: `mu` automatically enables live lines,
color, and rich Markdown when stdout is a terminal, and redirected output is
sequential and ANSI-free.

## Packaged installations

Packaged installations also expose three Mu-owned commands inside agent `bash`
calls: `apply_patch` for structured text edits, `edit` for exact text
replacement, and `view_image` for loading a local image into the model's tool
result. They are private symlinks under `/usr/libexec/mu`, all backed by the
same `mu` executable. For a source-tree build, create equivalent sibling
symlinks if you want to exercise the applets directly:

```sh
ln -sf mu target/release/apply_patch
ln -sf mu target/release/edit
ln -sf mu target/release/view_image
```

## Reference

See [SPEC.md](SPEC.md) for the complete product contract, including exact CLI,
configuration, discovery, rendering, persistence, provider, and zsh behavior.
Arch Linux packaging for this checkout is in [PKGBUILD](PKGBUILD).

## License

`mu` is available under the [MIT License](LICENSE).
