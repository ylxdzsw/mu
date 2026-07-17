# mu — Product Specification

`mu` is a small, composable agent runtime for the terminal: one prompt in, one
completed agent turn out. The core `mu` binary reads a prompt on stdin, accepts
attached image and audio inputs, runs an agent loop, streams turn events in the
selected output format, persists completed messages, and exits. Interactive
shell use builds around that simple turn unit instead of changing it.

This document defines the product behavior and implementation architecture.
Where a sequence is load-bearing (the per-turn lifecycle, streaming protocol,
or config schema), it is spelled out concretely.

---

## 1. Goals and non-goals

### Goals

- **Fast.** Per-invocation cold start in the single-digit-millisecond range.
  Every agent turn spawns a fresh `mu` process, so startup cost is paid every
  turn and must be negligible next to model latency.
- **Responsive.** Output streams as it is produced. Control returns to the shell
  immediately when a turn completes.
- **Composable.** The main abstraction is a turn, not a chat app, daemon,
  terminal UI, or project manager. The zsh plugin and shell scripts coordinate
  turns; they do not host a separate agent loop.
- **Non-magical.** No TUI. The shell owns the terminal and line editing; `mu`
  just reads a prompt and appends output. Output streams as it is produced (a
  tool line may appear before its output), but once a line is printed it is never
  rewritten or erased.
- **Minimal.** One model-visible function tool: `bash`, with a small Mu-owned
  command suite available inside it. A flat config directory. A
  SQLite file for state in the active scope. The core binary itself has no
  interactive input handling.
- **Unix-like terminal native.** `mu` runs as an ordinary foreground process in
  a Unix-like shell environment. Completion, history, job control, aliases, and
  interactive programs remain owned by the user's shell because `mu` never
  replaces it.
- **Day-to-day general purpose.** Coding is supported but not the focus. The
  agent is a general terminal assistant.

### Non-goals

- **No TUI, no REPL inside core `mu`.** No alternate screen, no full-screen layout,
  no widgets, no in-place history editing, no mouse, no line editor in the
  turn binary. `mu` never puts the terminal into raw mode. Interactive
  convenience layers live outside the core binary, and each submitted line is
  still a separate `mu` turn.
- **No re-rendering.** Lines are written once and never rewritten. Native
  terminal scrollback is the history mechanism.
- **No daemon in the core turn path.** Each turn is a fresh, stateless-on-exit
  process that loads/saves session state from SQLite.
- **No plugin SDK, MCP, or in-process subagent orchestrator.** Extensibility is
  via skills (markdown) and `bash` (call any CLI tool, including another `mu`
  process when independent delegation is useful).
- **No core shell emulation.** The core `mu` binary does not ship shell behavior,
  raw terminal editing, completion, or prompt rendering. The zsh plugin is a
  thin shell surface that owns zsh line editing and calls `mu` for each turn.
- **No Windows support.** `mu` is Unix-ish-only. It expects Unix process
  semantics, `bash -lc`, signals, process groups, and advisory file locks.

---

## 2. Key decisions

### 2.1 Language and runtime: Rust, single native binary

The defining requirement is startup speed for a process spawned on every turn.
Interpreted/JIT runtimes (Node, bun, Python) carry a 50–300 ms+ startup tax that
is unacceptable here.

**Decision: implement `mu` in Rust as a single native binary.**

Rationale:

- Cold start in single-digit milliseconds. No runtime bootstrap, no JIT warmup.
- One physical binary to install and update. Private `apply_patch` and
  `view_image` symlinks dispatch back into it by `argv[0]`.
- Mature ecosystem for everything needed: async runtime (`tokio`), HTTP/SSE
  (`reqwest`), SQLite (`rusqlite`), JSONC/serde.
- Because the shell owns line editing, `mu` needs **no** terminal/line-editor
  library at all — a further simplification over a REPL-owning design.

Tradeoff accepted: slower iteration than TypeScript, and no off-the-shelf
"AI SDK". Provider integration is hand-written against HTTP APIs (see §7); the
surface is small (chat completions + streaming + tool calls).

### 2.2 Single binary + shell surface

`mu` is one executable with a default **turn runner** mode: prompt and attached
inputs in, streamed turn events out, completed state persisted, exit. It also
owns management subcommands for core state inspection and mutation. The turn
path itself has no concept of prompts, key bindings, or long-lived UI state.

Interactive use is a thin shell layer around that unit:

- The zsh plugin is the preferred interactive surface. It owns zsh line editing,
  prompt mode, and keybindings, then submits each entered prompt by spawning
  `mu` for one foreground turn.

This single-binary shape is the central decision (see §3 for the full rationale
recap). It keeps the agent semantics small and scriptable while leaving the
shell responsible for interaction.

### 2.3 Interactive mode lives in the shell surface

The zsh plugin is the built-in interactive direction. It owns only line
collection, prompt mode, keybindings, and session continuity; every non-empty
submitted line still runs as a fresh foreground `mu` process.

Consequences:

- `mu` remains scriptable and stateless on exit.
- The zsh plugin never duplicates provider, tool, store, or agent-loop
  semantics.
- Ctrl-C and terminal behavior remain ordinary Unix process behavior.

### 2.4 Minimal fixed toolset

Exactly one model-visible tool, with no dynamic core registration: `bash`. See
§4. All local search, file reads, writes, edits, web fetches, tests, and other
CLI work are done through that shell tool. The `risk` field on a bash call is
advisory UI/audit metadata only; it is not a sandbox or approval proof.

### 2.5 Skills via progressive disclosure, no skill tool

Skill metadata (name + description + path) is injected into the system prompt.
The agent loads a skill file on demand using `bash` (`sed`, `cat`, `rg`, etc.).
No dedicated "skill" tool — this keeps the model-visible surface at one tool
and makes skills "just files". Built-in skills live in `/usr/share/mu` at the
lowest precedence; shipped built-ins may include
self-customization guidance such as `customize-mu` or delegation guidance such
as `subagent`, but user and project instructions can shadow them by name.
Skills may declare optional `requires_env` and `requires_commands` frontmatter
keys. Each key is a comma-separated list, and every listed requirement must be
met before the skill is injected.

### 2.6 Flat config, single SQLite state file

All user-facing configuration and instruction files live under a flat `.mu`
directory (`config.jsonc`, `AGENTS.md`, prompt/skill/command files). Runtime state is one SQLite database
in the active global/project scope. See §9, §10, and §11.

---

## 3. Architecture overview

`mu` has one executable and a small zsh integration around it. The CLI turn
runner remains the core unit.

```
   ┌──────────────────────────── shell surfaces ────────────────────────────────┐
   │  shell scripts: `mu [opts] <<< PROMPT`                                    │
   │  zsh plugin: prompt mode; every entered line spawns one `mu` turn         │
   └───────────────────────────────────┬───────────────────────────────────────┘
                                        │ invokes the same executable / command path
                                        ▼
   ┌────────────────────────────── mu (single binary) ─────────────────────────┐
   │  default turn mode: one prompt in, one completed turn out                 │
   │  management subcommands: project / session / status / compact / retry     │
   │  turn/management command modules: project/config/session resolution       │
   │                           provider client + agent loop                    │
   │                           tool registry: bash                             │
   │                           renderer / event stream                         │
   │                           store (SQLite in active global/project scope)   │
   └───────────────────────────────────────────────────────────────────────────┘
```

### Why this split (recap)

The hard part of "replace bash" is shell fidelity. Having the real shell own the
terminal gives that for free and forever; a core binary that owns a long-lived
REPL would have to reimplement completion, job control, and PTY handling
behavior indefinitely. The cost is (a) session state must persist across process
invocations — handled by SQLite in the active session scope, §11 — and (b)
interactive shell commands are not automatically visible to the agent (§6.3).
Shell integration is preferred because it is only a line-editing surface around
repeated turn invocations, not a replacement runtime.

### Binary module responsibilities

- **Entry.** Resolve project/config/session scope, parse args (`--session`,
  `--continue-latest`, `--attach`, `--output`, subcommands), read the prompt from
  stdin, run one turn, persisting each completed message as it lands (§11), exit.
- **Agent loop.** Send context to the provider, stream the response, execute
  tool calls, loop until the model stops requesting tools, yield final text.
  A configurable max-iterations guard bounds runaway loops (§11).
- **Tool registry.** The built-in `bash` tool with a JSON-schema parameter
  definition and an execute function.
- **Provider client.** Streaming HTTP to the model API behind one internal
  interface.
- **Renderer.** Sole writer to output; render the same turn as plain text or
  terminal UI (§5).
- **Store.** SQLite load/append in either project-local or global scope (§11).

The binary runs on a single `tokio` runtime. There is no input thread or line
editor. Bare `mu` reads stdin once as the prompt; file-backed turns read
non-terminal stdin once as an optional custom instruction.

### Binary CLI surface

The core binary is invoked one of two ways: as a **turn** (default, reads a
prompt on stdin) or as a **subcommand** (management; manual compaction alone
accepts optional non-terminal stdin as a custom focus). The surface is small:

- `mu [-s <id>] [-c] [--model <id>] [-a <file>] [--output final|plain|terminal]`
  — run one turn; prompt read from stdin. `-a/--attach` is repeatable and accepts
  supported image or audio files.
- `mu [-s <id>] [-c] [--model <id>] [-a <file>] [--output final|plain|terminal] <prompt-file>`
  — run one turn from a prompt file; if the first line starts with `#!`, drop
  it before sending the prompt. A `mu` shebang may contain exactly
  `--model <id>` as a turn-local default. Non-terminal stdin is appended as a
  custom instruction. `-a/--attach` is repeatable.
- `mu [-s <id>] [-c] [--model <id>] [--output final|plain|terminal] <custom-command>`
  — run a discovered shebang command from the active project/global `.mu`
  instruction index. Command names are relative `.mu` paths including
  extensions; built-in subcommands and explicit prompt paths win.
- `mu.zsh` — zsh prompt mode; each accepted prompt runs one foreground `mu`
  turn and keeps using the same session. `MU_ZSH_SESSION_ID=<id>` seeds
  attachment to an existing session.
- `mu project inspect --path <dir>` — report whether a directory resolves to a
  project scope, and which marker (`.mu` or `.git`) was found.
- `mu project init [--path <dir>] [--force]` — create minimal `.mu/` project
  metadata in the current directory by default, or in an explicitly chosen
  directory.
- `mu status --json [--include-models] [--include-commands] [--include-skills]`
  — machine-readable shell state for prompt rendering and completion.
- `mu session new` — create a session and print its id.
- `mu session list` — list recent sessions.
- `mu session transcript --session <id>` — print a persisted session
  transcript.
- `mu compact --session <id>` — force compaction. Terminal stdin is not read;
  non-terminal stdin is an optional verbatim custom focus instruction.
- `mu retry [-s <id>] [-c] [--model <id>]` — resume an interrupted (unclean)
  turn: normalize the tail and continue the agent loop with no new prompt.
  `--model` overrides the session model for the retry. No-op on a clean session.

The turn runner remains one completed turn per invocation. Bare `mu` reads the
prompt from stdin; a positional name first resolves to a discovered custom
command unless it is an explicit path such as `./prompt.md`, then falls back to
prompt-file mode. Prompt-file mode trims a leading shebang line when present.
For any file-backed turn, terminal stdin is left alone so the command does not
block; non-terminal stdin is read through EOF and, when non-empty, appended to
the loaded file body with `\n---\n\n`. Bare `mu` continues to use stdin as its
complete prompt.
Exact subcommand names win at the top level, so a prompt file that collides with
a subcommand name must be passed with a disambiguating path such as `./status`.
`mu session list`, `mu session transcript`, and project inspection/init do
**not** require a configured provider. `mu session new` can reuse the latest
session model, but in a fresh scope it needs a resolvable configured model.
Turn invocation and `mu compact` require a configured provider because they can
contact the provider (§7).

### Turn lifecycle (authoritative end-to-end flow)

This is the exact sequence the binary follows for one turn invocation:

1. **Parse args**, resolve the active scope from the invoking `pwd` (§9), read
   the resolved prompt source (stdin, prompt file, or custom command), and load
   any repeatable `-a/--attach` image or audio files. Each attachment must be at
   most 20 MiB and must be PNG, JPEG, WebP, GIF, WAV, or MP3 content matching
   its filename extension.
2. **Load config** (§9): global first, then project config over it when a
   project is active. If the provider's required fields are missing, print an
   error to stderr and exit non-zero (§7). Resolve the effective model:
   `--model` if given, else the session's stored `model`, else the merged
   config default.
3. **Open the active-scope SQLite DB** (create the current schema if absent; reject non-current schemas with instructions to remove `sessions.db`):
   project-local when inside a project, global otherwise.
4. **Resolve the session:**
   - If `--session <id>` is given and the row exists in the active scope → use
     it.
   - If `--session <id>` is given and the row does **not** exist in the active
     scope → print an error to stderr, exit non-zero (do *not* silently create
     it or fall back to a global session).
   - If `-c/--continue-latest` is given → use the latest session in the active
     scope, or create one if no session exists.
   - If neither `--session` nor `--continue-latest` is given → create a new
     session row, persist the assembled system prompt as the first message, and
     write its id to the runtime file named by `$MU_SESSION_FILE` when that env var
     is set (§11).
5. **Acquire the session `flock`** (§11). If held, print "session busy", exit
   non-zero.
6. **Normalize any interrupted tail, then build the context list.** If the
   previous turn was interrupted it may have left a dangling assistant
   tool-call message (some `tool_calls` without a result). Synthesize an
   interrupted result for each result-less call so the history is API-valid
   (§11; idempotent — a no-op on a clean session). Then build the context
   message list from persisted history: the leading system message, the latest
   compaction summary message as user context (if any), then all non-system
   messages after that summary, in order.
7. **Pre-turn compaction check** (§11, Tier 1): if the session's stored
   `last_total_tokens` (or bytes÷4 on the first turn) exceeds the configured
   fraction of the model context window, run compaction now, then rebuild the
   context list.
8. **Append the new user message** to the DB and to the context list. If files
   were attached, persist attachment metadata with the multi-part user content,
   store deduplicated bytes in SQLite, and reload the hydrated text + attachment
   parts rather than only the textual projection. (`mu retry` skips this
   step: it resumes the existing, now-normalized history with no new prompt.)
9. **Agent loop** — repeat until the model returns `finish_reason: "stop"` or the
   max-iterations cap is hit:
   a. Send the context list + tool definitions to the provider (streaming).
   b. Accumulate the streamed assistant message (text deltas and tool-call
      deltas; see §7 for the delta-accumulation rules).
   c. On stream end, record this response's `usage` (accumulate into the turn's
      running totals; see "usage accounting" below).
   d. **Persist the completed assistant message** (including any `tool_calls`)
      to the DB and append it to the context list.
   e. If `finish_reason` is `tool_calls`: split the calls into maximal
      contiguous batches of eligible readonly work. `plain` and `terminal` may
      execute contiguous `risk:"readonly"` `bash` calls concurrently, but
      **persist tool result
      messages** (`role: "tool"`, with their `tool_call_id`) in the model's
      original call order before looping back to (a). Any non-readonly call,
      unknown tool, or call that requires guardrail review is a sequential
      barrier.
   f. If `finish_reason` is `stop`: the loop ends.
10. **Update the session row:** `last_total_tokens` = the `total_tokens` from the
    *final* provider response of the turn (this already reflects the full context
    incl. prior turns); bump `updated_at`.
11. **Print the turn summary line** to stderr (§5), release the `flock`, exit 0.

**Usage accounting.** Each provider response in the loop carries its own `usage`.
For the **context fullness** figure (`last_total_tokens`, the summary's
`context: N%`) use only the **final** response's `total_tokens` — because
`prompt_tokens` already includes the entire prior context, the last response's
`total_tokens` is the true current context size (summing across iterations would
double-count). For the `in`/`out` token display, sum `prompt_tokens` and
`completion_tokens` across all iterations of the turn. Subtract provider-reported
cache reads and writes from `in`, and display those cache figures separately.

**Interruption.** Steps 9d/9e persist only *after* a message is fully formed. If
SIGINT / a dropped connection / a provider error occurs mid-stream, the partial
assistant message is never written; the DB holds only completed messages (§11).
No tool call begins without first persisting its parent assistant message, and a
result is persisted for every call that begins execution. Once tool execution
has started, interrupts fan out to every active tool process group, stop
launching new tools, drain partial output, and still persist tool results in
request order, so Ctrl-C stops work without making already-produced tool output
disappear. Nothing else is written on interruption: the process just exits. Any
dangling tool-call left by the interruption is repaired by step 6 of the *next*
invocation (see §11, "Interrupted turns and retry").

---

## 4. Tools

The model-visible tool surface is exactly:

```ts
bash({
  title: string,
  risk: "readonly" | "reversible" | "destructive",
  command: string,
  cwd?: string,
  timeout?: number, // seconds, default 120
  stdin?: string
})
```

`bash` prepends `/usr/libexec/mu` to its post-login `PATH`. That directory owns
two private symlinks to the physical `/usr/bin/mu` binary. Before normal CLI
parsing or async-runtime startup, `mu` checks the basename of `argv[0]` and
dispatches these applets:

- **`apply_patch`** accepts one patch argument or reads it from stdin. Its
  `*** Begin Patch` / `*** End Patch` format supports add, update, move, and
  delete operations with context hunks. Relative paths resolve from the shell
  call's working directory; absolute paths are used as written. It preflights
  the whole patch, rejects conflicting operations and existing add/move
  destinations, then applies validated file changes. Updating through a
  symlink edits its regular-file target while preserving the link; deleting a
  symlink removes only the link; moving a symlink renames the link. Dangling
  links can therefore be deleted or moved but cannot be updated.
- **`view_image [--detail auto|low|high|original] PATH`** loads a validated PNG,
  JPEG, WebP, or GIF through the same attachment loader and 20 MiB limit used by
  `mu -a`. `--detail` is optional and defaults to `auto`. The command writes a
  text summary normally and sends image bytes over a dedicated inherited
  artifact descriptor; it fails when invoked without Mu's artifact channel.

These are ordinary commands called through `bash`, not additional model-visible
function tools. The artifact channel is versioned and length-framed, is drained
concurrently with stdout, and is scoped to one bash call. A call may emit at
most eight images. Tool-image metadata and SHA-256-deduplicated bytes persist
with the tool message. Responses adapters serialize images in the native
`function_call_output`; Chat Completions adapters retain the tool text and add a
labeled multimodal user-message projection on the wire only.

`title` is the short human-readable action shown in the terminal. `risk` is
advisory metadata for UI/audit and drives optional guardrail review for
`destructive` calls; `mu` does not sandbox a call based on it. `command` is
executed as `bash -lc <command>`. `cwd`, when
present, applies only to that invocation; `cd`, shell variables, and exported
environment do not persist to later bash calls. `stdin`, when present, is piped
literally to the child process so the agent can pass bytes containing `$`, backticks,
quotes, or heredoc delimiters without shell expansion.

**Execution ordering.** Human-facing output may execute maximal contiguous
batches of `risk:"readonly"` `bash` calls concurrently because each
call runs in its own process group with isolated `cwd`, environment, timeout,
and stdin. This is an execution optimization only: stored tool-call records,
stored tool messages, and the next model request still see the original
assistant tool-call order.

**Terminal visibility.** While a tool call has started but its title has not
begun streaming, styled terminal output shows a mutable
`[preparing toolcall]` indicator. The indicator is cleared when `bash` begins
committing its `# <title>` line, followed by a `$ <command>` line. In styled
terminal output, `#` shares the title styling and `$` shares the command's risk
color; plain output instead includes an explicit `[risk]` label. If the call
includes a `cwd` field whose
resolved path differs from `mu`'s process working directory, it then prints an
`@ <raw cwd>` line using the exact `cwd` string supplied by the agent. If the
call includes a `stdin` field, it then prints a `< [stdin N bytes]` summary line
before command output. It streams combined output and finishes with an exit
status/duration line. Every tool error is visible.

**Output truncation policy.** Every bash output is
capped before it enters the context window so a single large result cannot blow
the budget:

- Default caps: **2000 lines** or **50 KB**, whichever is hit first
  (`limits.max_lines` / `limits.max_bytes`). A per-line byte cap
  (`limits.max_line_bytes`, default 10 KB) also applies, so a single pathological
  line cannot dominate.
- When output exceeds a cap, the model receives a **tail preview** plus a marker
  stating how much was elided, and the
  **full output is spilled to a temp file** under a truncation directory in the
  state dir. The marker points the model at that file so it can inspect the
  result with another `bash` call; nothing is lost, it just is not forced into
  context.
- Spilled temp files are garbage-collected after a retention window (default 7
  days), pruned opportunistically on startup.

All local search, file reads, writes, edits, tests, and web fetches go through
`bash`. The model should choose ordinary structured CLI patterns (`rg`, `find`,
`sed`, `python - <<'PY'` only when appropriate, `curl`, `git diff`, etc.) and use
literal `stdin` for content that should not be interpreted by the shell.

**Process lifecycle.** Each call spawns one child process. On Unix it is placed
in its own process group before `exec`, and on Linux `PR_SET_PDEATHSIG` asks the
kernel to send SIGTERM if `mu` dies. On timeout or interrupt, `mu` sends SIGTERM
to the process group, waits a short grace period, then sends SIGKILL; if group
signaling fails it falls back to killing the direct child. Ordinary commands are
expected not to outlive the tool call.

For recursive `mu` delegation, the bash tool sets `MU_SUBAGENT_DEPTH` to one
more than the current process depth. Normal management commands still work at
any depth, but recursive agent turns are rejected once the process environment
reports depth greater than `1`.

`timeout` defaults to 120 seconds and must be greater than zero. `mu` does not
pre-check command argv size; if `bash -lc <command>` fails with OS
argument-list-too-long (`E2BIG`), the tool returns a clear error. `mu` does not
fall back to temp scripts.

---

## 5. Output and rendering

`mu` supports three output formats: `final`, `plain`, and `terminal`. They are
different renderings of the same agent turn and must not imply different agent
behavior.

- **Final output** is for supervisor agents invoking `mu` as a subagent. It
  does not stream. On success, stdout is exactly the final raw assistant message
  content from the completed turn, written once after the turn finishes and
  without an added newline. Tool output, intermediate assistant tool-call
  messages, reasoning/progress, automatic retry notices, summaries, and bells
  are suppressed. Automatic retries and per-completed-message persistence still
  behave the same as in human-facing modes. On fatal failure after retry
  exhaustion or any other unrecovered error, stdout is `error: <message>`
  followed by one newline and the process exits non-zero.
- **Plain text** is for simple scripting and low-friction terminal use. It
  prioritizes assistant/tool text and avoids terminal-specific control.
- **Terminal output** is for humans in an interactive terminal. It may use color
  and may update recent status lines for in-progress activity, but it must keep
  normal scrollback. It must not use an alternate screen, clear the screen, or
  require mouse interaction.
**Concurrency contract.** All output modes may run contiguous readonly
`bash` calls concurrently. `terminal` keeps append-only scrollback and the
one-live-line rule: at most one bash call owns live terminal streaming at a
time, even while later readonly calls are already running in the background.
`plain` follows the same ordered human-facing display without live-line redraws.
`final` suppresses the live transcript display while preserving the same
execution, ordering, and persistence semantics.

The renderer is the sole writer to stdout/stderr and enforces the selected
format. It may style output only when stdout is a TTY and `--output terminal` is
selected. It never clears the screen, uses an alternate screen, or requires
mouse interaction. Plain output is always ANSI-free.

Assistant Markdown is parsed on TTYs. The renderer commits only output whose
terminal representation is stable: ordinary prose streams as soon as it is not
being held for an inline span, while headings, quotes, and list items stream once
their line prefix is unambiguous. A heading prefix waits for the space after the
full opening `#` run, so `##` is not rendered as h2 until it cannot still become
h3. Closing heading hashes are not special-cased and are rendered literally.
Inline links, inline code, emphasis, strong text, and double-tilde
strikethrough wait for the current span to complete; fenced code starts terminal
code styling at the opening fence, streams code lines without printing fence
markers, and resets styling at the closing fence or response boundary. Markdown
tables are buffered until the table is complete enough to align and commit once,
so columns never require rewriting prior output. Each column is at most 80
visible terminal cells wide; longer header or body cells wrap into aligned
continuation rows without truncating their content. While a confirmed table is
buffered, TTY terminal output shows a mutable `[table ~N tokens]` live indicator;
the completed table clears and overwrites that indicator instead of committing a
final table-status line. Markdown features outside this supported terminal
subset are emitted as raw Markdown rather than partially rendered. When stdout
is piped or redirected, assistant deltas pass through byte-for-byte as the model
produced them, preserving raw Markdown for downstream consumers.

### 5.1 TTY block-spacing contract

Terminal output is structured as a sequence of top-level transcript blocks:
the zsh `mu>` prompt, assistant text, committed thought lines, bash tool blocks,
notices, and similar human-facing sections. Spacing has exactly one owner at
each boundary: zsh owns the transition from a submitted `mu>` prompt to the
child process's first visible block, and the renderer owns subsequent
renderer-to-renderer block transitions.

- Top-level transcript blocks are separated by exactly one empty line.
- After submission, the canonical normalized prefix is
  `mu> prompt\n\n[first visible block]`. Neither a missing empty line
  (`mu> prompt\n[first visible block]`) nor two empty lines
  (`mu> prompt\n\n\n[first visible block]`) are valid.
- The renderer never adds leading spacing before its first visible block. That
  block may be a live thought indicator, assistant text, a tool call, or a
  notice. In styled TTY output, provider-emitted whitespace before it is
  boundary noise: it does not render and does not mark a block as committed.
  Blank lines inside visible assistant content remain intact, and plain output
  continues to preserve raw assistant deltas.
- The *next* top-level block owns that separator. Committed block formatters
  should end with exactly one newline; they must not rely on trailing blank
  lines baked into their own text.
- Live status lines such as the updating `[thought ...]` line or the
  `[preparing toolcall]` indicator may reserve the top separator on first
  render, but subsequent ticks only redraw that one mutable trailing line. A
  first live status line does not add spacing on behalf of a preceding shell
  prompt.
- A bash tool block includes its header, streamed preview/output, omission
  marker, and final exit line; those pieces are not separated from each other by
  extra blank lines.
- The turn summary is its own final transcript block. When a turn produced
  transcript output, it has exactly one empty line before the summary and one
  empty line between the summary and the next shell prompt.

This contract applies to human-facing `terminal` and `plain` output.

**Stream routing (explicit).** The conversation transcript goes to **stdout**:
tool presentation, tool failures, Bash output, and assistant text. Fatal process
errors and the turn summary go to **stderr**. Thus `mu <<< prompt > out.txt`
captures the complete portable transcript while fatal diagnostics/summary
remain visible. Stdout TTY detection selects rich versus portable rendering;
stderr TTY detection suppresses the summary when redirected.

- **Tool presentation.** Styled terminal output shows `[preparing toolcall]` as
  its one mutable live line after a tool call starts and before title bytes are
  available; plain output omits this transient status. Bash then streams the
  active command header as the model composes the tool-call arguments:
  `# <title>` first, then `$ <command>` once the `risk` value is available so
  terminal output can color the whole command line consistently. The title and
  command are append-only and capped in place; the command display is the first
  decoded line with a byte cap. If fields arrive out of order, display buffers
  until the ordered header can be committed.
  If a `stdin` field is provided, the optional `< [stdin N bytes]` summary starts
  only after the command line has committed; styled terminal output may update
  that summary as the only live line, while plain output commits it once the
  final byte count is known.
  Later tool-call headers are buffered until their original-order display slot
  becomes active. Once execution begins, a header that was already streamed is
  not printed a second time. `plain` shows explicit risk labels such as
  `[readonly]` where `terminal` uses color. Both human-facing outputs stream the
  same output head preview, then print the omission marker only once at tool
  completion if a middle section was actually omitted, followed by any reserved
  tail and a matching exit line. Full tool results still follow the shared
  model-context truncation policy (§4). Multiple tool calls in one assistant
  message are displayed in provider order. In `plain` and `terminal`,
  concurrent readonly batches still present exactly one active bash stream at a
  time in original tool order; later calls may already be running, but their
  headers and execution output are buffered until they become the active slot.
- **Assistant text.** `plain` and redirected output stream raw Markdown deltas
  unchanged. TTY `terminal` display commits parsed Markdown as soon as the
  relevant unit is stable: prose streams token-by-token unless an inline span is
  open, list/heading/quote content streams after the prefix is stable, tables
  wait for the complete table, and unsupported Markdown stays raw.
- **Reasoning progress.** A definite reasoning-block start immediately creates
  the terminal's mutable thought line. Chat Completions reasoning keeps the
  existing `[thought <duration>, <tokens> tokens]` form, using its streamed
  reasoning text for the live estimate. Responses reasoning is opaque and uses
  `[thought <duration>]` without a per-thought token count. When an opted-in
  Responses summary begins with a complete bold-only line or ATX heading, Mu
  conservatively extracts that first line and updates the live display to
  `[thought <duration>] <title>`; prose, missing summaries, and unrecognized
  formats remain timer-only. Titles are normalized to one line and capped at 80
  visible terminal cells. Reasoning completion commits the current thought line
  in both human-facing modes even when no reasoning or summary text was exposed.
- **Errors.** Always printed and clearly prefixed, with TTY styling when
  available. Fatal turn failure produces a non-zero process exit code so the
  shell's `$?` is meaningful.

**Turn summary line.** When `mu` exits normally (turn complete), it prints a
single structured summary line to stderr:

```
[mu] tokens: 1234 in (567 cache read, 89 cache write) / 456 out  context: 12%
```

All figures come from the provider's reported `usage` for the turn: `in` is
`prompt_tokens` excluding cache reads and writes, `out` is `completion_tokens`,
and cache usage is shown parenthetically when reported. Cache write is omitted
when the provider does not report it; `context` is the new
`total_tokens` ÷ model context window. This is the *only* stderr output in the normal case. It appears after all
stdout, and goes to stderr so it stays out of a captured stdout transcript. It
is suppressed if stderr is not a TTY (piped/redirected), since it would pollute
log files. In both human-facing modes it is followed by one blank line so the
next shell prompt is visually separated from the completed turn.

Plain mode avoids terminal-only control sequences so it remains suitable for
scripts. Human terminal mode may show progress for in-flight work, but committed
transcript content is never erased from scrollback. When
`terminal_bell.enabled` is true, terminal mode also emits a BEL (`\a`) after a
successful turn's summary once total turn duration meets
`terminal_bell.min_duration_ms` (default 10s).

---

## 6. zsh shell surface

The zsh plugin is the preferred interactive surface. It behaves like a shell
editing mode: Tab with the cursor at the beginning of the line toggles the
current prompt into or out of `mu>` mode while preserving the current buffer;
Enter submits the current buffer as one `mu` turn when it contains non-whitespace
text and otherwise just draws a fresh `mu>` prompt; Ctrl-C cancels the current
`mu>` draft but leaves the cancelled line in scrollback; Backspace remains an
ordinary delete key; and Ctrl-D keeps normal shell EOF behavior even while
`mu>` mode is active. Up and Down stay within the current `mu>` buffer and do
not browse shell history; the user leaves `mu>` mode first if they want normal
shell history navigation. The plugin must not duplicate agent-loop, provider,
store, or tool semantics.

The plugin requires zsh, `jq`, and the `mu` binary on `PATH`. Setting
`MU_ZSH_BIN` to a specific executable overrides the binary name/path used by
the plugin.

### 6.1 Invocation pattern

Submitting a non-empty prompt runs `mu` as an ordinary foreground child process.
The plugin passes `--session` after the first turn, forwards configured output
mode, writes the prompt to the child process's stdin, waits for the turn to
finish, and then redraws `mu>` with the same session id.
After ZLE commits the submitted prompt line to scrollback, the plugin prints one
empty line before child-process output starts, independent of whether the child
uses `terminal` or `plain` human-facing output.

Consequences:

- `mu` owns the terminal while each turn is running; streaming output works
  directly.
- Ctrl-C while editing in `mu>` mode cancels the current draft, leaves that
  prompt line visible in scrollback, and redraws `mu>` like a shell prompt
  interrupt. Ctrl-C while a foreground `mu` turn is running uses ordinary Unix
  signal behavior for the foreground process.
- After each turn exits, zsh returns to `mu>` mode with the same session id.

### 6.2 Entry and exit

- Source `mu.zsh` from `.zshrc`.
- Press Tab with the cursor at the beginning of the line to enter `mu>` mode;
  press Tab at the beginning of a `mu>` line to leave it again. In both
  directions, keep the current buffer and cursor position intact.
- Enter a non-whitespace line to run one turn. Empty or whitespace-only Enter
  should draw a fresh `mu>` prompt without submitting anything.
- Press Ctrl-C while editing to cancel the current draft, keep the cancelled
  line in scrollback, clear the live buffer, and draw a fresh `mu>` prompt.
- Backspace should always delete backward; it is not a mode-exit key.
- Ctrl-D should keep normal shell EOF semantics even inside `mu>` mode, so an
  empty `mu>` prompt exits the shell rather than merely leaving prompt mode.
- Press Up or Down while editing in `mu>` mode to move within the current
  buffer only. They must not recall shell history; leave `mu>` mode first if
  shell history navigation is desired.
- Shift+Enter inserts a newline without submitting when the terminal sends the
  CSI-u sequence `Esc [ 13 ; 2 u`. Terminals that send ordinary Enter for this
  key combination cannot be distinguished by zsh and require a matching key
  configuration.
- Typing `/` at the start of a `mu>` line proactively lists slash commands.
  After that, Tab delegates matching, candidate lists, and menu selection to
  the user's normal zsh completion settings.
- A buffer beginning with `/` is a slash command. Known custom commands take
  everything after their name as a custom instruction, including inserted
  newlines; `/compact` accepts the same instruction syntax as a custom focus.
  Unknown names report a slash-command error. Other built-in slash commands keep
  their own argument rules.
- `/attach <file>` resolves and stages one readable regular file in shell
  memory for the next user message and may be repeated. It creates no session
  message itself. `/attach` lists pending files and `/attach --clear` discards
  them. The prompt shows the pending count. Empty Enter, draft cancellation,
  mode changes, `/model`, `/new`, `/retry`, and `/compact` do not consume the
  queue; the next ordinary prompt or custom command passes every staged file as
  a repeatable `-a` argument and clears the queue before launching `mu`.
- While `mu>` mode is active, conflicting line-editor plugins should be
  suspended. Common ZLE helpers such as syntax highlighting and autosuggestions
  may be disabled automatically; additional plugin toggles may be attached with
  mode enter/exit hooks. The arrays `MU_ZSH_ENTER_HOOKS` and
  `MU_ZSH_EXIT_HOOKS` contain zsh function names; enter hooks run after prompt
  mode is active, and exit hooks run after the normal shell prompt is restored.
- Ctrl-D is the normal terminal EOT key (`^D`). xterm-style and browser-terminal
  input paths forward it as input when the browser or OS has
  not intercepted the key before the terminal receives it.

### 6.3 Context boundaries

- **Full structured history:** `mu` records prompts, assistant responses, and
  tool calls in SQLite (§11). Tool output is stored with the shared
  truncation/spill policy, so the DB keeps the structured transcript and spill
  files hold oversized raw command output.
- **No shell-command sharing:** commands run outside `mu` or the shell
  plugin are
  not automatically fed to the agent. `mu` keeps the boundary explicit and
  private.

### 6.4 Session management

Session lifecycle is exposed through CLI commands:

- The zsh plugin without a session lazily creates one on its first submitted
  prompt and reuses that session for later prompts in the same shell.
- Exporting `MU_ZSH_SESSION_ID=<id>` before entering `mu>` attaches the zsh
  plugin to an existing session.
- `mu -c` continues the latest session in the active scope for a one-shot turn.
- `mu session new` creates a session and prints its id.
- `mu session list` lists recent sessions.
- `mu compact --session <id>` compacts a session on demand.

---

## 7. Provider / model integration

Mu supports exactly two hand-written HTTP/SSE protocols: OpenAI-compatible
Chat Completions and OpenAI Responses. Each configured provider has a required
complete `endpoint`. After URL parsing and optional trailing-slash
normalization, a case-sensitive path ending in `/chat/completions` selects Chat
Completions and a path ending in `/responses` selects Responses. Query parameters
are preserved but do not affect classification. Every other path fails during
configuration loading; Mu never infers a protocol from a hostname, provider id,
or model name. A gateway exposing both protocols is represented by two provider
entries.

Both adapters accept the semantic transcript and Mu's `bash` function schema,
stream protocol-neutral text/reasoning/tool-call events, and return a semantic
assistant result plus usage. The renderer, tool executor, guardrail, retries,
and compaction remain protocol-neutral.

**Chat Completions.** Mu posts directly to the configured endpoint with
`messages`, the Chat function wrapper, `stream:true`, and
`stream_options:{include_usage:true}`. It accumulates indexed
`delta.tool_calls`, assistant text, and optional `reasoning_content`. A resolved
effort is sent as top-level `reasoning_effort`. Complete reasoning attached to
an assistant tool-call response is persisted and replayed verbatim only when
the current endpoint and wire model id match its origin. This supports
DeepSeek thinking tool loops without model-name heuristics.

**Responses.** Mu posts directly to the configured endpoint with `stream:true`,
`store:false`, `include:["reasoning.encrypted_content"]`, locally reconstructed
`input`, and a flat Responses function-tool definition. It never sends
`previous_response_id` or a conversation identifier. Every request opts into
reasoning summaries with `reasoning:{summary:"auto"}` and adds `effort` to that
object when one is resolved. Providers that reject the summary option fail the
request normally; Mu does not retry without it. Typed SSE events provide
reasoning-item boundaries, optional reasoning-summary text, output text, and
function-call argument deltas. The complete successful `response.output` item
array is stored unchanged, including encrypted reasoning and any summary, and
replayed as input only for the same endpoint and wire model. Semantic tool
results become `function_call_output` items connected by `call_id`.

The semantic transcript remains authoritative for display, compaction, and
cross-model continuation. Switching model or protocol inside a session keeps
semantic messages and reconstructs function calls/results, but omits native
state whose endpoint or model origin does not match. Changing only effort does
not invalidate native replay. Compaction excludes native state before the
active summary boundary and retains it with the recent semantic suffix.

Text and images are supported by both adapters. Images serialize as Chat
`image_url` or Responses `input_image`. Existing audio inputs serialize as Chat
`input_audio`; a Responses endpoint rejects audio locally with a clear error.
Only successfully completed streams produce replay state, so retries never
depend on a partial or remote response chain.

**Model context window.** The 75% threshold needs the model's max context size.
Source it from `config.jsonc`: each configured model entry carries a
`context_window` integer. mu does not fetch model cards. If a model has no
configured `context_window`, the threshold-based tiers (Tier 1 pre-turn and Tier
2 in-loop) are skipped for it and the Tier 3 API-error fallback is the only
guard.

Model and provider selection come from `config.jsonc`: a complete `endpoint`, optional
env var holding the API key, and ordered provider/model definitions. If the
global config file is missing, `mu` creates a starter `~/.mu/config.jsonc`
automatically before loading configuration. In a scope with no sessions, the
first configured model is used. API keys are read from environment variables;
`mu` does not store secrets in its database.

**No provider, hard fail.** If no provider is configured, a provider has no
valid supported endpoint, or a non-empty configured key env var is unset, a *turn* invocation
exits immediately with a non-zero status and a clear message pointing at
`config.jsonc`. `mu compact` follows the same rule because it calls the
provider. There is no silent fallback once configuration has been loaded.

Because the semantic message history is stored separately from origin-bound
native replay (§11), swapping endpoint/model across turns is supported.

---

## 8. Skills

Skills are reusable, on-demand instruction files discovered inside the active
global and project `.mu` directories.

- A skill is a regular file with YAML front-matter defining `name` and
  `description`. The `name` must match the filename stem. For external
  compatibility with the open skill spec, `folder/SKILL.md` also qualifies when
  `name` matches `folder`.
- Optional `requires_env` and `requires_commands` keys contain comma-separated
  environment-variable and executable names. A skill is active and listed only
  when every declared variable is present and every declared command resolves
  on `PATH`.
- On startup `mu` scans `.mu` with bounded depth/file limits, parses only
  qualifying front-matter, and injects a compact `<available_skills>` block —
  name, description, absolute file path — into the system prompt.
- When a task matches a skill, the model reads the full file via `bash`, using
  the **absolute path** from the injected block. Relative paths written inside a
  skill file resolve against that file's containing directory.

The same file may also be a custom command when its first line is a permissive
`mu` shebang. The shebang may contain no arguments or exactly
`--model <provider/model[:effort]>`; all other arguments are rejected when the
file is invoked. An explicit invocation `--model` overrides the shebang model,
which otherwise overrides the attached session or configured default for that
turn without rewriting stored session state. Progressive disclosure remains:
only short metadata is always in context; full instructions are pulled in on
demand.

---

## 9. Project discovery

On startup, `mu` treats the invoking current working directory as authoritative
for the turn, then searches upward from that `pwd` to resolve the active scope.

A directory is a project when it contains `.mu` or `.git`.

If a directory contains only a `.git` marker, `.mu` is created there only when
`mu` needs to write project state. Merely discovering or reading project
information must not mutate the filesystem.

If the search reaches the user's home directory or the filesystem root without
finding a project, `mu` uses the global scope rooted at `~/.mu`.

Nested project merging is not supported. The first project found while walking
upward is the active project.

Git worktrees are treated as their own projects. If the discovered `.git`
marker is a worktree pointer file and there is no closer `.mu`, the directory
containing that `.git` file is the project root. The agent should be told both
the project root and the current working directory. It should also be told
relevant worktree information when available.

The shell tool's working directory defaults to the process working directory,
not the project root.

The project-local directory is `.mu`. It may contain:

- `config.jsonc`, optional project configuration.
- `.env`, optional local environment values.
- `AGENTS.md`, the project-local agent instructions.
- optional instruction files that may be plain references, custom commands,
  skills, or both.
- `sessions.db`, the project-local session history and state database.
- `.gitignore`, which ignores session database and related SQLite files.

Project state is private to the project. A project should be movable and
understandable by inspecting its `.mu` directory, while still avoiding committing
volatile session state by default.

Automatic project state creation writes only runtime state and `.gitignore`;
it does not create project configuration. Explicit `mu project init` creates a
minimal config overlay and `.gitignore`, but no empty skills directory. It
refuses to create a nested mu project inside another discovered project unless
`--force` is supplied. Global configuration creation writes the full starter
`config.jsonc` and no `.gitignore`.

---

## 10. Configuration

`mu` has global configuration and optional project configuration. The global
configuration directory is `~/.mu` by default (or `$MU_CONFIG_DIR` when set).
Project configuration lives in the active project's `.mu` directory.

The global and project directories have the same conceptual shape:

```
~/.mu/ or <project>/.mu/
  config.jsonc      # provider endpoint + key env var + model; optional tuning
  .env              # optional environment values for provider lookup + bash
  AGENTS.md         # agent instructions, appended to system prompt
  review.md         # optional command/skill/reference instruction file
```

When a project is active, global configuration is loaded first and project
configuration is merged over it. Project values take precedence. Parent project
configuration is not merged because nested projects are not supported. When the
upwalk reaches home or root without finding a project, only global
configuration is used.

Optional `.env` files are loaded with the same scope precedence:
process environment first, then global `.env`, then active-project `.env`.
The resulting effective environment is used for provider API-key lookup and is
passed to every `bash` tool process. `.env` files are parsed as dotenv data, not
sourced as shell scripts.

Configuration and session storage are related but separate concepts. Config is
merged across scopes; sessions live in exactly one scope: the discovered
project's `.mu/sessions.db` or the global `~/.mu/sessions.db`. Sessions from
one scope are not visible in another.

- **config.jsonc** — JSON with comments and trailing commas. Concrete shape
  (field names are normative):

  ```jsonc
  {
    "providers": {
      "openai": {
        "endpoint": "https://api.openai.com/v1/responses", // required complete POST URL
        "api_key_env": "OPENAI_API_KEY",         // optional: env var NAME, not the key
        "models": {
          "gpt-5.6-terra": {
            "context_window": 1050000,           // needed for Tier-1 compaction & context%
            // Optional ordered suggestions for status output and shell completion.
            "supported_efforts": ["none", "low", "medium", "high", "xhigh", "max"]
          }
        }
      }
    },
    "terminal_bell": {                           // optional terminal notification policy
      "enabled": true,
      "min_duration_ms": 10000
    },
    "compaction": { "fraction": 0.75, "keep_recent_turns": 2 },  // optional
    "limits": { "max_iterations": 50, "max_lines": 2000, "max_bytes": 51200, "max_line_bytes": 10240 },
    "redaction": {
      "env": ["GITHUB_TOKEN"]                    // optional; provider api_key_env is implicit
    }
  }
  ```

  At least one provider and one model are required; everything else has the
  defaults shown. Provider and model order is meaningful: project config entries
  are listed before inherited global entries, and model suggestions follow that
  order. `supported_efforts` contains arbitrary provider-defined strings and is
  advisory: it drives status output and shell completion but does not restrict
  manually entered effort suffixes. If global `config.jsonc` is missing, `mu`
  creates a starter file automatically. `mu` hard-fails on a turn if the required
  fields are missing or the API-key env var is unset (§7).
- **.env** — optional dotenv data. Values are visible to `bash`; this is
  convenience, not sandboxing. Values from provider `api_key_env` and
  `redaction.env` are exact-value redacted from bash output before the output is
  stored or shown to the model. Empty redaction values are ignored with a
  warning. Short redaction values are still redacted with a warning.
- **AGENTS.md** — system-prompt addendum. Global instructions are loaded first;
  active-project instructions are appended after them when a project is active.
  Both are included; "project overrides global" means later text wins by
  convention, not that global instructions are dropped.

The system prompt is intentionally minimal. It is assembled once when a session
is created, persisted as the first message, and then loaded from session history
for later turns. Existing sessions do not rebuild it when files or config change.
The assembled prompt has this fixed order:

1. A short role/behavior preamble (a few sentences). Illustrative:
   > You are mu, a terminal agent. Exactly one function tool is available:
   > `bash`; do not call any other function tool. Inside `bash`, Mu provides
   > `apply_patch` for structured file edits and `view_image` for loading an
   > image into the tool result. These are shell commands, not function tools.
   > Each bash call is isolated; pass `cwd` explicitly when needed. Keep
   > responses concise.
   The exact wording lives in `src/system_preamble.md`; keep it short.
2. A `<runtime>` block of host-stable facts only, as plain `key: value` lines:
   ```
   <runtime>
   os: linux
   date: 2026-06-18
   user: alice (uid 1000)
   </runtime>
   ```
   Per-session environment — current working directory, project root, session
   id, and git worktree details — is **not** part of the system prompt. It is
   introduced once as the first user message when the session is created, and a
   later working-directory change is announced with a `<system-reminder>` on the
   affected turn (§11, "Agent environment context"). Keeping this out of the
   persisted system prompt keeps the system prefix stable for every later turn in
   the session.
3. The `<available_skills>` block (§8), or omitted if there are no skills. Skill
   metadata is merged from built-in, global, and active-project instruction
   indexes. Priority is project > global/user > built-in for same-name skills
   and commands.
4. The global `AGENTS.md` contents, if the file exists.
5. The project-local `AGENTS.md` contents, if a project is active and the file
   exists.

Tool definitions are **not** part of this prompt; they go in the API `tools`
parameter (§7). Frontier models need little scaffolding, so the fixed parts (1–2)
stay terse and `AGENTS.md` carries user customization.

---

## 11. State and persistence

State is stored in a SQLite database in exactly one active scope: the project
database (`<project>/.mu/sessions.db`) when a project is active, or the global
database (`~/.mu/sessions.db`) otherwise. SQLite is chosen for zero-setup
embedded storage, transactional durability, fast open, and easy querying. **WAL
mode** is enabled so per-turn load/append is fast and concurrent shells do not
block each other unnecessarily.

Conceptual schema (flat and small):

- **session** — `id`, `created_at`, `updated_at`, `cwd`, `model`,
  `title`,
  `last_total_tokens` (the most recent `usage.total_tokens` reported by the
  provider; used for the pre-turn overflow check, §"Context window and
  compaction"). `model` is set at session creation from the
  effective model (lifecycle step 2). After a successful turn or retry, `model`
  is updated to that invocation's effective model; failed invocations do not
  update it. `cwd` records the last working directory used for that session.
  `title` is set lazily from the first user prompt (first ~60 chars) and is
  display-only for `mu session list`.
- **message** — `id`, `session_id`, `role`
  (`system`/`user`/`assistant`/`tool`/`summary`),
  `content`, optional full user content JSON for multi-part inputs, `created_at`,
  ordering index, and for tool results `tool_call_id`. Assistant rows may also
  contain exact origin-bound native replay: Chat reasoning for a tool-call
  message or a complete Responses output-item array. This native payload
  augments rather than replaces the semantic fields and is never rendered. The
  first message in a new session is the persisted system
  prompt. For user messages with image or audio attachments, `content` remains
  a textual projection for listing/token estimates, while attachment metadata
  references content-addressed bytes stored once in SQLite. The full hydrated
  parts are reloaded for model context. A `summary` row is a compaction
  summary (§"Context window and compaction"); the context builder keeps the
  leading system message, then starts from the latest `summary` row and includes
  everything after it.
- **attachment_blob** — content-addressed attachment bytes keyed by SHA-256,
  with byte size and creation time. Message content stores filename, MIME type,
  and blob reference metadata; identical bytes are stored once per scope DB.
- **tool_call** — `id`, `message_id`, `tool`, `args` (JSON), `risk`, `output`,
  `status` (`ok` / `error` / `interrupted`), timings. Records the agent's tool
  invocations for inspection and the renderer's truncation pointers. (Tool
  *results* fed back to the model are stored as `tool` messages; this table is
  the structured audit copy.) There is intentionally **no** turn-status or
  checkpoint column on `session`: whether the last turn finished is derived from
  the message tail (§"Interrupted turns and retry").

### Session mapping

`mu` maps each interactive shell instance to at most one active session:

- **Lazy creation.** On the first submitted prompt, the zsh plugin invokes `mu`
  without `--session`, exporting `MU_SESSION_FILE` to a temporary path. `mu`
  creates the session row and writes the new id to that file. After `mu` exits,
  the plugin reads the id and passes `--session <id>` on later prompts. The id is
  never printed to stdout by the turn, so the transcript stays clean.
- **Attach / continue.** `MU_ZSH_SESSION_ID=<id>` seeds the zsh plugin with an
  existing session, while `mu -s <id>` and `mu -c` handle one-shot re-entry from
  the command line. `mu session list` lists recent candidates.
- **Per-turn lifecycle.** Each turn: open DB → acquire session lock → normalize
  any interrupted tail → load session messages → run turn (persisting each
  completed message as it lands) → release lock → exit. The connection opens
  lazily so a turn that errors early stays cheap.

Sessions are append-only logs; resuming replays messages into the context
window. Multiple shells holding *different* sessions run concurrently (safe under
WAL).

### Agent environment context

For each turn, the agent should know:

- The current working directory.
- The project root, if the session is project-scoped.
- The active session id.
- Relevant git worktree information, if the project is a worktree.

The current working directory remains important even inside a project because it
reflects the user's immediate intent. Project root provides broader context, but
it should not replace `pwd`.

Stable environment information is introduced once, when the session is created.
This includes project path when present, session id, and git worktree
information. Later turns in the same session should not repeat that full
environment block.

If the working directory changes after session creation, the next turn should
append a short XML reminder and update the session's stored `cwd`:

```
<system-reminder>
current working directory changed to: /new/pwd
</system-reminder>
```

This reminder is emitted only on a submitted turn whose `pwd` differs from the
stored session `cwd`. It does not restate project information.

### Message-level persistence and interruption

Persistence is at **message granularity**, and only **completed** messages are
written:

- The user prompt is written when the turn starts. As the turn proceeds, each
  fully-formed assistant message (including its `tool_calls`) is committed as its
  stream completes, and each tool result is committed when that tool finishes.
  A result is persisted for **every tool call that begins execution** — even one
  killed mid-run gets a result recorded — so a side-effecting command is never
  lost from history.
- A partial/in-flight assistant message (streamed text, reasoning) is **never**
  persisted. On interruption — Ctrl-C, a dropped connection, a provider error —
  the process simply exits and the partial stream dies with it. Nothing is
  written on the interruption path.

### Interrupted turns and retry

There is **no** stored turn-status flag or checkpoint. Whether the last turn
finished is *derived* from the message log:

- A session is **clean** when its last message is a completed assistant reply
  (an `assistant` message with no `tool_calls`), a compaction `summary`, or when
  its only message is the synthetic environment seed (no real turn yet).
- Otherwise it is **unclean**: the tail is a user prompt with no reply, a tool
  result with no following assistant turn, or an assistant message still
  carrying `tool_calls`.

**Rationale — derive, don't store.** A separate boolean can drift out of sync
with the messages (precisely in the crash cases that matter) and would risk
"retrying" a turn that actually completed. The log is the single source of
truth, so cleanliness is read from it and cannot desync.

**Normalizing an interrupted tail.** Before any turn or retry runs, mu makes the
tail API-valid: for the most recent assistant tool-call message, every
`tool_call` that has no result gets a synthesized interrupted result
(`INTERRUPTED_TOOL_RESULT`: "may have started and not completed; verify state").
Calls that finished keep their real result untouched. This is idempotent (a
no-op on a clean session).

**Rationale — treat result-less calls uniformly.** We do **not** try to tell a
call that "never started" from one "started but killed": the window between
persisting a tool-call request and spawning the process is sub-millisecond, and
a write may have realized side effects. Assuming "maybe executed" and asking the
agent to verify is the safe, simple choice — it removes the need for any
per-call "running" marker in the database.

**Recovery is not a special mode.** On the next invocation:

- A **new prompt** normalizes the tail, then appends on top and runs. This makes
  the common "Ctrl-C to redirect" flow work: after interrupting, the user can
  just type the next instruction; the agent sees the interrupted results and the
  new prompt and continues or redirects. No forced retry, no stuck session.
- **`mu retry`** normalizes the tail and re-runs the loop with *no* new prompt,
  so the model continues the interrupted turn. `--model` overrides the stored
  session model for that retry; the zsh `/retry` command forwards an active
  `/model` selection. It refuses on a clean session ("nothing to retry").

### Session concurrency lock

Two processes can target the same session, so concurrent turns against one
session are possible and must be serialized. Each turn acquires a per-session
lock for its duration; a second `mu` targeting the same session finds it held
and **fails fast** with a "session busy" message rather than interleaving
writes.

The lock is an advisory `flock` on a per-session lock file under the active
state directory's `locks/` folder (for example `<project>/.mu/locks/` or
`~/.mu/locks/`), acquired in lifecycle step 5 before any DB writes. This is
deliberately *not* a SQLite-level lock:
`BEGIN IMMEDIATE` takes a reserved lock on the **whole database file**, which
would serialize unrelated sessions against each other. A per-session `flock`
lets different sessions proceed independently.

WAL caveat: WAL lets readers and writers not block each other, but **two writers
still serialize at the SQLite level** (only one write transaction at a time).
Different sessions writing concurrently is rare and brief (per-turn appends), so
mu sets a `busy_timeout` (e.g. 5s) on the connection to ride out the momentary
contention rather than erroring. The `flock` handles same-session serialization;
`busy_timeout` handles the rare cross-session write overlap.

### Context window and compaction

**Token counting (source of truth).** mu does not run a tokenizer. It uses the
**provider's reported usage** — both adapters map native Chat or Responses usage
into input, output, total, cache-read, optional cache-write, and reasoning-output
token fields. mu stores
the latest `total_tokens` on the session after each turn; that figure is the
authoritative measure of how full the context is.

A `bytes ÷ 4` approximation (`approx_tokens(s) = ceil(len_bytes(s) / 4)`) is
used only where no API figure exists yet:
- the **first turn** of a session (no prior `usage` reported), to size the very
  first request;
- estimating the size of **not-yet-sent** content (e.g. which messages to keep
  when building a compaction), where the provider has not yet returned a count.

Context management then uses a **three-tier strategy**, from most to least
graceful:

**Tier 1 — graceful pre-turn compaction (75% threshold).** At the start of each
turn, mu compares the stored `total_tokens` from the previous turn (or the
bytes÷4 estimate on the first turn) against the model's context window. If it
exceeds a configurable fraction (default 75%), mu compacts *before* sending the
new turn. Because this runs between turns, it is fully graceful — no turn is
wasted, no replay.

**Tier 2 — proactive in-loop compaction.** A single turn can add many large tool
results, so the pre-turn figure goes stale *within* a turn. Before each model
call after the first in the agent loop, mu re-estimates the working context
(bytes÷4 over the in-memory message list) and compacts against the same fraction
threshold if it has grown too large. This catches runaway tool output before it
becomes a hard API error. If a
single compaction cannot bring the context back under the threshold (e.g. the
retained recent turns are themselves oversized), mu stops re-compacting for the
rest of that turn and lets Tier 3 handle the true overflow, so it never loops on
summarize calls.

**Tier 3 — hard-stop on API overflow error.** If the provider still returns a
context-length error during a turn, mu catches it, compacts immediately, and
retries (up to 3 times). If it still overflows, the turn is aborted with a clear
message. Overflow is recognized from an HTTP `413`, a structured error
`code`/`type` of `context_length_exceeded`, or a known overflow phrase in a 4xx
body (e.g. "prompt is too long", "maximum context length", "context window") —
message matching is gated to client errors so an unrelated 5xx body is not
misclassified.

**Compaction algorithm** (same in all tiers): summarize everything up to a
cut point into a single **`summary` message row** (replacing any prior summary
row), keeping the most recent N turns (default 2, configurable) as verbatim rows
after it. The cut is always at a user-message boundary, so a retained assistant
`tool_calls` message is never separated from its `tool` results. The
summarization *input* clamps each entry (tool results hardest) so a huge history
cannot make the summarize request itself overflow; the stored transcript is
untouched. The next turn's context builder (lifecycle step 6) loads the latest
`summary` row plus all rows after it — so compacted history is naturally
excluded without deleting anything. The original rows remain in SQLite (the
on-disk transcript is lossless); only the in-context working set shrinks. When a
prior summary row exists, the new one is generated by updating it ("update the
anchored summary, preserve still-true details, remove stale facts").

**Manual compaction.** `mu compact --session <id>` forces compaction on demand.
Like a prompt file or custom command, it leaves terminal stdin alone and reads
non-terminal stdin through EOF as an optional verbatim custom instruction. The
instruction gives relevant material more of the available detail and summary
budget, while the summarizer must still preserve every important fact needed to
continue correctly. In zsh prompt mode, `/compact <instruction>` pipes the text
after the command through this same stdin path. Automatic compaction never
supplies a custom focus.

### Agent-loop bounds

The agent loop runs until the model stops requesting tools. A configurable
**max-iterations** cap (default **50** tool round-trips, `limits.max_iterations`)
bounds a runaway loop: on reaching it, `mu` stops, emits a clear notice, and
exits non-zero, leaving all completed messages persisted so the user can inspect
and re-prompt.

**Exit codes.** `0` success; `1` general/config/provider error; `2` session busy
(lock held) or `--session` not found; `128 + signal` when a forwarded
terminating signal ends the turn — most commonly `130` for SIGINT (the shell's
default for Ctrl-C), and `143` for SIGTERM. A signalled exit takes precedence
over the generic error code even when the interruption first surfaces as a turn
error. The summary line is printed only on exit `0`.

### Abort, pause, and resume

Abort means the current language-model request or tool execution is cancelled
when possible, the turn stops, and `mu` exits. Abort is an explicit interruption
of work in progress; completed messages remain persisted and partial messages
are discarded as described above. The interrupted turn leaves the session
"unclean"; it is resumed by `mu retry` (continue with no new prompt) or
superseded by simply sending the next prompt (§11, "Interrupted turns and
retry").

Pausing at arbitrary points and resuming a partially completed model stream are
not supported: resume always restarts from the last completed message, with any
in-flight tool call recorded as interrupted.

---

## 12. Safety posture

`mu` is deliberately **unsandboxed**. Commands execute directly through `bash`,
and files can be read or modified with the user's permissions. There are no
interactive per-action confirmation prompts. The `risk` field drives the
destructive-action guardrail described below, but it is not a sandbox boundary.

The protections that remain are cheap and non-intrusive:

- **Visibility is the safeguard.** Output is non-magical and append-only. The
  transcript records what ran and its captured result; terminal scrollback and
  SQLite history provide the audit trail.
- **Interruptibility.** Because `mu` runs as a foreground job, Ctrl-C is the
  practical "stop" button: it stops launching new work, interrupts every active
  tool process group, drains visible output where possible, persists completed
  messages/tool results, and exits non-zero.
- **Secrets** are never persisted by `mu`; provider keys come from the
  environment or `config.jsonc`, never the database.
- **External content** (file contents, command output, fetched pages, web search
  results from CLIs, etc.) is treated as untrusted data, not as instructions to
  follow.

Sandboxing and interactive approvals are not part of the product. Guardrail
review can prevent a declared destructive action from executing, but it does
not constrain commands declared at other risk levels and is not a sandbox.

### 12.1 Guardrail

An opt-out review gate for destructive commands. Unless disabled, a separate
model call assesses each `bash` call whose declared `risk` is `"destructive"`
before execution. The reviewer returns `risk_level`, `user_auth_level`, and
`reason`; the action executes only if `user_auth_level >= risk_level` on a fixed
ordinal scale. There is no interactive y/n prompt — denied actions return as
tool errors so the agent can adapt or ask the user.

**Ordinal scale.** Risk ranks are `low`(0), `medium`(1), `high`(2), and
`critical`(4). Authorization ranks are `unknown`(0), `low`(1), `medium`(2),
`high`(3), and `explicit`(4). The gap before `critical` ensures only explicit
authorization can approve a critical-risk action.

`user_auth_level >= risk_level` yields:
- `low`(0): allowed by any auth level including `unknown`(0).
- `medium`(1): requires at least `low`.
- `high`(2): requires at least `medium`.
- `critical`(4): requires `explicit` — the only level that can approve it.

**Reviewer call.** A separate non-streaming chat-completions call inside the
turn process (mu is per-turn, so there is no persistent reviewer session). The
reviewer uses the same provider and API key as the primary agent; the model
defaults to the active turn model but can be overridden via
`guardrail.review_model`.
The reviewer has no tools — it judges from a compact transcript and the action
JSON alone.

**Context sent to the reviewer.** A filtered, budgeted transcript (user +
assistant + tool-call arguments + tool results, skipping the system message):
10 000 tokens for messages, 10 000 for tools, 2 000 per message entry, 1 000 per
tool entry, and 40 recent non-user entries. Truncation
keeps prefix + suffix with a `<truncated omitted_approx_tokens="N"/>` marker.
The planned action is provided as pretty-printed JSON (capped at 16 000 tokens).

**Reviewer system prompt.** The prompt in `src/guardrail.md` uses terminal-agent
framing, gives the reviewer no tools, and asks for risk and authorization levels
rather than an allow/deny decision (the ordinal comparison computes that). It
covers evidence handling (transcript = untrusted), user authorization scoring,
risk categories, and a strict JSON output contract.

**Outcomes.**

- **Allow** (`auth >= risk`): the bash call executes. A `[guardrail: allow]`
  line with the reviewer reason is rendered after the streamed command header
  and before execution output.
- **Deny** (`auth < risk`): the bash call does not execute. A `[guardrail: deny]`
  line with the reviewer reason is rendered after the streamed command header,
  and a tool error is returned to the agent:
  > guardrail: action rejected — risk_level X exceeds user_auth_level Y (reason).
  > Do not work around this; stop and ask the user to authorize, or choose a
  > less destructive approach.
  The agent can then adapt its approach or stop and ask the user.
- **Reviewer failure** (timeout, malformed JSON, network error after 3 retry
  attempts): the turn is **aborted** (`bail!`). Re-authorizing would likely
  fail again since the reviewer itself is malfunctioning.

**User authorization via history.** There is no dedicated "re-prompt" mechanism.
When the agent asks the user and the user responds with explicit approval
("yes, force push"), the user's message becomes part of the session history. On
the next turn, the reviewer sees this in the transcript and can score
`user_auth_level: "explicit"`, which permits even `critical`-risk actions.

**Circuit breaker.** Per-turn, tracks consecutive denials and a sliding window
of recent reviews. If consecutive denials reach 3, or denials in the last 50
reviews reach 10, the turn is aborted with a clear notice. This prevents the
agent from repeatedly attempting destructive actions that the reviewer keeps
denying.

**Retry.** The reviewer call retries up to 3 times on transient errors (timeout,
network failure, parse failure) with exponential backoff (1s, 2s). Context-
length errors are not retried.

**Config.**

```jsonc
"guardrail": {
  "enabled": true,                           // default on; set false to opt out
  "review_model": null,                      // null -> same as active turn model
  "timeout_ms": 90000,
  "circuit_breaker": { "consecutive": 3, "window": 50, "window_denials": 10 }
}
```

**Audit.** Each review is recorded in the `review` table (SQLite): action JSON,
risk level, auth level, outcome, reason, and timestamp.

**Concurrency.** Guardrail only targets `destructive` calls, which are always
sequential (concurrent batches only run `readonly` tools). There is no
interaction with the concurrent execution path.

---
