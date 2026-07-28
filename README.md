# mu

`mu` is a small, composable agent for the terminal: one prompt in, one
completed agent turn out. It works equally well as a Unix command in scripts or
as an interactive assistant inside zsh or Fish.

## Quick start

Build the binary and put it on `PATH`:

```sh
cargo build --release --features portable
export PATH="$PWD/target/release:$PATH"
```

The `portable` feature bundles SQLite and embeds Mu's built-in skills, so this
source-tree binary can run without being installed. On its first normal
invocation it writes the built-ins and three applet symlinks into your user
cache.

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

## Interactive shell usage

The most comfortable way to use `mu` is right inside your shell. For zsh,
source the included plugin from `.zshrc`:

```zsh
source /path/to/mu/mu.zsh
# Arch package: source /usr/share/zsh/plugins/mu/mu.zsh
```

For Fish 4 or newer, source the Fish plugin near the end of `config.fish`, after
your prompt and key bindings:

```fish
source /path/to/mu/mu.fish
# The Arch package also loads /usr/share/fish/vendor_conf.d/mu.fish.
# Source that file again at the end of config.fish if later configuration
# replaces its prompt wrappers or Tab bindings.
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

- `/new` starts a new session while keeping the current model and attachments.
- `/model` selects a configured model for later turns in this shell scope.
- `/attach <file>` adds an image or audio file to the next turn.
- `/retry` resumes a turn interrupted by Ctrl-C, a crash, or a lost connection.
- `/compact` compacts older turns in a long session, optionally with a focus
  instruction. It reports when all history is already inside the configured
  recent-turn retention window.

Both plugins require `jq` and `mu` on `PATH`, plus their respective shell. The
Fish integration requires Fish 4 because it records replayable turns with
`history append`.

The plugin keeps its session, model choice, and pending attachments together in
one project scope. Changing directories only hides that state, so returning is
non-destructive. Running a Mu prompt or slash action in another scope discards
the old bundle; invalid model and attachment input leaves it untouched.

## More ways to run a turn

Use a specific model or attach files to a one-shot turn:

```sh
mu -m openai/gpt-5:high -a screenshot.png -a recording.wav <<"EOF"
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

Preview the exact user-prompt text without starting a turn:

```sh
mu cat review.md
printf 'Focus on authentication.' | mu cat review.md
printf '# Standalone prompt\n' | mu cat
```

`mu cat` resolves a target exactly like a turn: explicit paths select prompt
files, while other names select the active project, global, or built-in command
before falling back to a file in the current directory. For file-backed prompts,
non-terminal stdin is appended after the same `---` separator used during
execution. A terminal shows the resolved source and rendered Markdown;
redirected output is the exact composed prompt text. It does not contact a
provider or create a session.

Choose how much the caller sees:

```sh
mu -o final prompt.md       # final assistant message only
mu -o concise prompt.md     # assistant text plus one-line tool calls
mu -o detail prompt.md      # normal human transcript (built-in default)
mu -o full prompt.md        # complete reasoning and tool details
```

Inspect sessions and resolved state with `mu session list`, `mu session
transcript --session <id>`, and `mu status --json`. Add `--include-git` or
`--include-session-details` when those heavier status sections are needed. Run
`mu --help` for the full CLI surface.

## How it works

The core stays deliberately small. Each turn starts a fresh native process,
loads its session, streams the agent and its tool activity, saves completed
messages, and exits. A few ideas follow from that:

- **A turn is the primitive.** `mu` is a fast native binary, not a daemon, TUI,
  or in-process REPL. Shell pipelines and prompt files compose it naturally.
- **Shell-native interaction.** The zsh and Fish integrations add a persistent
  prompt mode without replacing the shell or duplicating the agent runtime.
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

- OpenAI-compatible Chat Completions, OpenAI Responses, and Anthropic Messages
  providers, with strict full-endpoint selection, ordered model selection, and
  per-turn model and reasoning-effort overrides.
- Persistent global or project-scoped sessions, continuation, transcripts,
  automatic context compaction, and interrupted-turn recovery.
- Four output densities with automatic interactive-terminal rendering.
- Image and audio attachments from the CLI and both shell prompt modes.
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

For Codex, add a personal `SessionStart` hook at `~/.codex/hooks.json`. Plain
text printed by the hook is added as developer context, and including `compact`
reloads the projection after context compaction:

```json
{
  "description": "Load mu instructions and skills.",
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup|compact",
        "hooks": [
          {
            "type": "command",
            "command": "mu context --export",
            "timeout": 10,
            "statusMessage": "Loading mu context"
          }
        ]
      }
    ]
  }
}
```

On the next Codex session, use `/hooks` to review and trust the new command hook.
Codex records trust against the hook definition and asks again after it changes.

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

Then select it per turn with `mu -m openai/gpt-4o` (`--model` also works), or reorder the
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
submitted image and audio attachments are stored directly in SQLite.

Setting `"output": "concise"` in global or project `config.jsonc` changes the
default output density; an explicit `-o`/`--output` always wins. Output density
controls brevity, not terminal behavior: `mu` automatically enables live lines,
color, and rich Markdown when stdout is a terminal, and redirected output is
sequential and ANSI-free. Interactive output also defaults to
`"line_wrapping": true`: prose and tables fit the detected terminal width, while
compact tool and status rows are ellipsized to it. Setting it to `false` leaves
prose and table cells for the terminal to wrap, but still caps compact rows at
80 visible cells. It never changes redirected, final, persisted, or
model-visible text.

During compaction, interactive output uses one mutable
`[compacting <duration>]` line. The completed result marks its rebuilt context
percentage with `~` because it is estimated until the next provider response;
the shell prompt uses the same marker and returns to an unprefixed percentage
after that response supplies exact usage.

## Native installation and portable builds

The default Cargo build is for a native installation and uses system SQLite:

```sh
cargo build --release
```

For a binary installed as `<prefix>/bin/mu`, Mu always uses
`<prefix>/share/mu/` for package-owned built-ins and
`<prefix>/libexec/mu/` for package-owned applets. It assumes the installation
is correct: it neither checks nor creates these directories at startup. Arch
Linux packaging for this checkout is in [PKGBUILD](PKGBUILD) and uses this
native default.

Add `portable` for a standalone Unix binary:

```sh
cargo build --release --features portable
```

Portable builds bundle SQLite and embed every shipped built-in. When the binary
is under a `bin/` directory, each resource is resolved independently: an
existing `<prefix>/share/mu/` wins for built-ins and an existing
`<prefix>/libexec/mu/` wins for applets. Any resource without that installed
directory falls back to the cache:

- absolute `$XDG_CACHE_HOME/mu` when `XDG_CACHE_HOME` is set;
- `$HOME/Library/Caches/mu` on macOS;
- `$HOME/.cache/mu` on other Unix systems.

Mu aborts rather than using `/tmp` if no cache root can be determined or if a
cache path cannot be created. Cached resources live in fixed `builtins/` and
`applets/` subdirectories. A missing subdirectory is created and populated in
place; cached applets are absolute symlinks to the current executable. An
existing directory is authoritative and is never inspected, refreshed, or
repaired. A conflicting non-directory is an error, and a failed first
population may leave a partial directory that later runs deliberately trust.
Moving or upgrading the binary does not update either cache: remove the
applicable cache subdirectory manually to regenerate it.

Version tags publish portable Linux x86-64 musl and macOS ARM64/Intel archives
with SHA-256 checksum files. Built-ins are embedded, so those archives do not
contain a separate `builtins/` directory. Releases continue to include the
unchanged Windows MSYS2 UCRT64 package and archive.

## Reference

See [SPEC.md](SPEC.md) for the complete product contract, including exact CLI,
configuration, discovery, rendering, persistence, provider, and shell behavior.

## License

`mu` is available under the [MIT License](LICENSE).
