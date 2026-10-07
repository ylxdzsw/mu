# Mu CLI

Mu runs one agent turn per invocation. It reads a prompt, streams or prints the
selected output, persists completed state, and exits. Run `mu --help` or
`mu <subcommand> --help` for generated option help.

## Run a turn

```text
mu [-s <session-id> | -c] [-m <model>] [-a <file> ...]
   [-o final|concise|detail|full] [--output-schema <JSON>] [--no-context]
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
- `-m|--model '(provider)/model[:effort]'`: use fallback starting at that
  configured provider. Quote the parentheses in shell commands.
- `--trap off|destructive|reversible|all`: set the turn's Bash trap level.
- `--output-schema <JSON>`: request a provider-native constrained JSON
  response format for this ordinary turn. The argument must be valid JSON.

`--no-context` skips skills (including built-ins and their loading guidance)
and global/project `AGENTS.md` when assembling a system prompt. The system
preamble and `<runtime>` remain. It applies only at session creation or a
successful compaction during this invocation; it does not force compaction or
change an existing epoch's prompt. The resulting prompt persists until the
next compaction, where omitting the flag restores normal injection.
Compaction itself uses the old prompt, and existing conversation/checkpoint
content is not scrubbed. Configuration, `.env`, tools, location reminders, and
explicit prompt/custom-command loading are unchanged.

An explicit `-o|--output` overrides `config.jsonc`:

- `final`: print only the final assistant message after the turn completes.
- `concise`: assistant text plus compact tool activity.
- `detail`: the normal human transcript.
- `full`: complete reasoning and tool details.

`final` is intended for supervisors and scripts. On success, stdout contains
only the final assistant message. Fatal diagnostics, including unrecovered
errors and session-busy errors, go to stderr in every output mode. A trapped
Bash call is the exception: Mu prints its complete command and stdin to stdout
and exits with status 3. Non-final assistant text already streamed before a
failure remains visible.

`--output-schema` accepts JSON directly; there is no file option. For example,
pass a file's contents with `--output-schema "$(cat schema.json)"`. Mu parses
the argument as JSON, then passes the schema value unchanged: it performs no
schema compatibility checks or rewrites and does not parse or validate the
final answer against the schema. An invalid JSON argument fails before the
prompt is queued. This option works with every `-o` presentation mode and does
not alter presentation.

The schema contract persists with the ordinary turn and is reused by `mu
retry` without the flag. It is not session-global; submitting a new prompt
without `--output-schema` resets it. Compaction summaries omit the schema, but
an in-turn continuation retains the ordinary turn's contract. Provider
mapping: Chat Completions uses `response_format.json_schema` with
`name: "mu_output"`, `strict: true`, and the schema; Responses uses
`text.format` with the same name, strictness, and schema; Anthropic uses
`output_config.format` with `type: "json_schema"` and the schema, without a
strict field. If a constrained response ends in an incomplete terminal output
state, the invocation fails nonzero. Mu does not otherwise validate answer
format or schema adherence.

An explicit refusal reported by a provider is a dedicated failure, detected
from provider refusal signals rather than textual phrase matching. It is not
automatically retried or sent to a fallback provider. Mu records available
native refusal details and usage without accepting assistant content or
executing tools, exits 1 with a stderr diagnostic, and leaves the turn
recoverable by a manual `mu retry`.
Refusal-like prose without a provider refusal signal remains ordinary output;
Mu does not guess from phrases such as "I can't".

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
overrides the attached session or configured default. It does not change
configuration, but becomes the session's latest recorded model once a provider
request is persisted.

Mu strips the shebang and optional skill frontmatter from discovered commands.
Explicit prompt files strip only the leading shebang, retaining frontmatter.
File-backed turns do not read terminal stdin. Non-terminal stdin, when
non-empty, is appended verbatim after `\n---\n\n` as a custom instruction.

```sh
mu review
printf 'Focus on authentication.' | mu review
```

Executable permission does not affect discovery. Suffixless executable command
files are recommended so they can also be invoked directly through their
shebang, such as `./.mu/review`. In zsh and Fish prompt mode, a discovered
command is invoked by its exact relative path, such as `/review` (or
`/review.md` when that is the filename).

## Management commands

### `mu init [--path <dir>] [--force]`

Create minimal project metadata. It defaults to the current directory and
refuses a nested Mu project unless `--force` is explicit.

### `mu new [--no-context]`

Create a model-free session and print its id. It does not select that session
as `current-session`.

### `mu sessions [--limit <count>]`

List recent sessions in the active scope. The default limit is 20. Sessions
written in an unsupported journal version are skipped with a warning.

### `mu transcript [-s <id>] [-o <format>] [--epoch <n>]`

Replay a persisted session without contacting a provider, defaulting to the
last selected session and configured output density. Configuration is loaded
read-only and permissively: replay does not create a missing global config,
load environment files, or require providers to pass runtime validation. With
no config files, the bundled `concise` default applies.
The default replay includes synthetic compaction turns and their derived
trigger/result lines; `final` omits those internals. `--epoch` limits replay to
activity sent under one context-epoch cache key. A compaction request and its
result remain in the old epoch; its checkpoint and continuation use the new
epoch.

### `mu status [selection options] [--json] [--include-*]`

Inspect resolved session, model, context, scope, and output state.
`--include-git`, `--include-session-details`, `--include-models`,
`--include-commands`, and `--include-skills` add their corresponding data.

Every Bash tool call receives `MU_SESSION_ID`, identifying the agent session
executing the tool, not the scope's `current-session`. Mu overrides inherited
and `.env` values for the Bash child; nested agents supply their own ID to
their tools. The variable does not implicitly select a session or model.

From the calling agent's scope, inspect its running session:

```bash
mu status -s "$MU_SESSION_ID" --json
```

### `mu context [--export | --no-context]`

Without `--export`, print the assembled system prompt Mu would use.
`--export` emits user `AGENTS.md`, non-built-in skills, environment-file
guidance, and a pointer to `mu-doc` for a foreign agent. It never contacts a
provider.

`--no-context` previews only the system preamble and runtime. It cannot be
combined with `--export`.

### `mu cat [<prompt-file-or-command>]`

Preview the exact resolved user prompt without contacting a provider or
creating session state. With no target, stdin is the prompt. Interactive output
includes provenance and rendered Markdown; redirected output is the exact
composed prompt.

### `mu retry [selection options] [-o <format>] [--trap <level>] [--no-context]`

Resume an interrupted turn, defaulting to `current-session`. It normalizes the
interrupted provider tail, restores the submitted working directory, resumes
never-started Bash calls, and may retry unresolved readonly attempts before
continuing without a new user prompt. Started higher-risk calls are not
repeated. A clean session is a no-op.
Without `--trap`, retry reuses the persisted turn policy. An explicit value
overrides it only for this invocation.
`--no-context` affects only a new system prompt assembled when this invocation
successfully applies compaction, including recovery of an already saved summary.

### `mu compact [-s <id>] [-o <format>] [--trap <level>] [--no-context]`

Force compaction for a session, defaulting to `current-session` and the
configured output density. `--output` overrides configuration. Non-terminal
stdin is an optional custom focus instruction.
`--no-context` omits skills and `AGENTS.md` from the new epoch's system prompt,
not from the compaction request itself.

Compaction itself is a persisted synthetic agent turn. If it is interrupted,
that session rejects new prompts and another `mu compact`; use `mu retry` to
finish the pending epoch transition.

Mu does not migrate session journals. An explicitly selected session, or a
management command that requires `current-session`, fails when that journal
uses an unsupported version. A normal unselected turn can ignore an
incompatible `current-session`, warn, and create a new session; `--continue`
uses the same fallback because it does not name a specific journal.

Turn options must not precede a management subcommand.
