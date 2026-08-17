# Mu CLI

Mu runs one agent turn per invocation. It reads a prompt, streams or prints the
selected output, persists completed state, and exits. Run `mu --help` or
`mu <subcommand> --help` for generated option help.

## Run a turn

```text
mu [-s <session-id> | -c] [-m <model>] [-a <file> ...]
   [-o final|concise|detail|full]
mu [turn options] <prompt-file-or-command>
```

With no positional file or command, Mu reads the complete prompt from stdin.
`-a|--attach` is repeatable and accepts supported image and audio files.

Session and model selection:

- No `-s` or `-c`: create a fresh session.
- `-s|--session <id>`: run in that session in the active scope.
- `-c|--continue`: continue the active scope's last selected session.
- `-m|--model provider/model[:effort]`: use a fixed provider.
- `-m|--model model[:effort]`: use ordered provider fallback.

An explicit `-o|--output` overrides `config.jsonc`:

- `final`: print only the final assistant message after the turn completes.
- `concise`: assistant text plus compact tool activity.
- `detail`: the normal human transcript.
- `full`: complete reasoning and tool details.

`final` is intended for supervisors and scripts. On success, stdout contains
only the final assistant message. An unrecovered fatal error writes
`error: ...` to stdout and exits nonzero.

When invoking Mu through an agent's Bash tool, pass multiline or
escaping-sensitive prompt text through the tool's `stdin` field:

```ts
bash({
  title: "Run a focused Mu turn",
  risk: "readonly",
  command: "mu --output final",
  cwd: "/work/project",
  timeout: 600,
  stdin: "Review the current changes and report correctness issues."
})
```

## Prompt files and custom commands

A positional name first resolves to a discovered custom command in the active
project, global, or built-in instruction index. If no command matches, it is a
prompt file relative to the invoking directory. Absolute paths and paths
starting with `./` or `../` always select an explicit prompt file. Built-in
subcommand names win exact collisions.

A prompt file may start with a Mu shebang:

```markdown
#!/usr/bin/env -S mu --model openai/gpt-5:high
Summarize the current checkout.
```

The shebang accepts no arguments or exactly `-m|--model <model-ref>` as separate
tokens. An invocation model overrides the shebang; the shebang otherwise
overrides the attached session or configured default for that turn without
rewriting session model state.

Mu strips the shebang and optional skill frontmatter before submitting the
prompt. File-backed turns do not read terminal stdin. Non-terminal stdin, when
non-empty, is appended verbatim after `\n---\n\n` as a custom instruction.

```sh
mu review.md
printf 'Focus on authentication.' | mu review.md
```

In zsh and Fish prompt mode, a discovered command is invoked by its relative
path, including extension, such as `/review.md`.

### Built-in `/grill`

`/grill <topic>` invokes the extensionless built-in `grill` custom command. The
topic argument is required. Its agent conducts a structured design interview:
it maps the topic's decisions as a tree, works the tree in frontier rounds
(asking every unblocked question at once with a recommended answer), looks up
facts from the environment via Bash rather than asking the user, and waits for
answers before advancing to the next round. When the frontier is empty it
writes a concise spec of all settled decisions to `grill-spec.md` in the
current directory.

### Built-in `/goal`

`/goal <goal>` invokes the extensionless built-in `goal` custom command. The
goal argument is required. Its agent acts only as a supervisor: it creates one
fresh worker session, continues that same session until completion, and judges
the result without planning or performing the work. The worker owns state
inspection, planning, execution, and verification. Each continuation repeats
the original goal verbatim to the worker to prevent drift.

## Management commands

### `mu init [--path <dir>] [--force]`

Create minimal project metadata. It defaults to the current directory and
refuses a nested Mu project unless `--force` is explicit.

### `mu new`

Create a model-free session and print its id. It does not select that session
as `current-session`.

### `mu sessions [--limit <count>]`

List recent sessions in the active scope. The default limit is 20.

### `mu transcript [-s <id>] [-o <format>] [--epoch <n>] [--html]`

Replay a persisted session without contacting a provider, defaulting to the
last selected session and `detail` output. `--html` emits a browser-viewable
xterm.js document whose pinned assets are loaded from jsDelivr when opened.
The default replay includes synthetic compaction turns and their derived
trigger/result lines; `final` omits those internals. `--epoch` limits replay to
activity sent under one context-epoch cache key. A compaction request and its
result remain in the old epoch; its checkpoint and continuation use the new
epoch.

### `mu status [selection options] [--json] [--include-*]`

Inspect resolved session, model, context, scope, and output state.
`--include-git`, `--include-session-details`, `--include-models`,
`--include-commands`, and `--include-skills` add their corresponding data.

### `mu context [--export]`

Without `--export`, print the assembled system prompt Mu would use.
`--export` emits user `AGENTS.md`, non-built-in skills, environment-file
guidance, and a pointer to `mu-doc.md` for a foreign agent. It never contacts a
provider.

### `mu cat [<prompt-file-or-command>]`

Preview the exact resolved user prompt without contacting a provider or
creating session state. With no target, stdin is the prompt. Interactive output
includes provenance and rendered Markdown; redirected output is the exact
composed prompt.

### `mu retry [selection options] [-o <format>]`

Resume an interrupted turn, defaulting to `current-session`. It normalizes the
interrupted tail, restores the submitted working directory, and continues
without a new user prompt. A clean session is a no-op.

### `mu compact [-s <id>]`

Force compaction for a session, defaulting to `current-session`. Non-terminal
stdin is an optional custom focus instruction.

Compaction itself is a persisted synthetic agent turn. If it is interrupted,
that session rejects new prompts and another `mu compact`; use `mu retry` to
finish the pending epoch transition.

Turn options must not precede a management subcommand.
