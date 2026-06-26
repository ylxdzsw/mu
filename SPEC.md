# mu — Design Specification

`mu` is a small, composable agent runtime for the terminal: one prompt in, one
completed agent turn out. The core `mu` binary reads a prompt on stdin, accepts
attached inputs such as images, runs an agent loop, streams turn events in the
selected output format, persists completed messages, and exits. Interactive
shells, REPLs, web daemons, editor integrations, and other surfaces build around
that simple turn unit instead of changing it.

This document describes the design, the key decisions behind it, and the
high-level shape of the implementation. It favors prose and decisions over code,
but where a sequence is load-bearing (the per-turn lifecycle, the streaming
protocol, the config schema) it is spelled out concretely so the implementation
is unambiguous.

---

## 1. Goals and non-goals

### Goals

- **Fast.** Per-invocation cold start in the single-digit-millisecond range.
  Every agent turn spawns a fresh `mu` process, so startup cost is paid every
  turn and must be negligible next to model latency.
- **Responsive.** Output streams as it is produced. Control returns to the shell
  immediately when a turn completes.
- **Composable.** The main abstraction is a turn, not a chat app, daemon,
  terminal UI, or project manager. Thin surfaces such as the zsh plugin,
  temporary `mu-cli`, and a future `mu-web` coordinate and present turns; they
  do not host a separate agent loop.
- **Non-magical.** No TUI. The shell owns the terminal and line editing; `mu`
  just reads a prompt and appends output. Output streams as it is produced (a
  tool line may appear before its output), but once a line is printed it is never
  rewritten or erased.
- **Minimal.** One model-visible tool: `bash`. A flat config directory. A
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
  turn binary. `mu` never puts the terminal into raw mode. `mu-cli` may provide
  a convenience REPL, but each submitted line is still a separate `mu` turn.
- **No re-rendering.** Lines are written once and never rewritten. Native
  terminal scrollback is the history mechanism.
- **No daemon in the core path (V1).** Each turn is a fresh, stateless-on-exit
  process that loads/saves session state from SQLite. A future `mu-web` daemon is
  a coordinator around this turn boundary, not a separate agent runtime.
- **No plugin SDK, no MCP, no sub-agents (initially).** Extensibility is via
  skills (markdown) and `bash` (call any CLI tool).
- **No core shell emulation.** The core `mu` binary does not ship shell behavior,
  raw terminal editing, completion, or prompt rendering. The zsh plugin is a
  thin shell surface that owns zsh line editing and calls `mu` for each turn.
- **No Windows support.** `mu` is Unix-ish-only. It expects Unix process
  semantics, `bash -lc`, signals, process groups, and advisory file locks.

---

## 2. Key decisions

### 2.1 Language and runtime: Rust, single static binary

The defining requirement is startup speed for a process spawned on every turn.
Interpreted/JIT runtimes (Node, bun, Python) carry a 50–300 ms+ startup tax that
is unacceptable here.

**Decision: implement `mu` in Rust as a single statically linked binary.**

Rationale:

- Cold start in single-digit milliseconds. No runtime bootstrap, no JIT warmup.
- One binary to drop on `PATH`. Trivial to install, update, and distribute.
- Mature ecosystem for everything needed: async runtime (`tokio`), HTTP/SSE
  (`reqwest`), SQLite (`rusqlite`), JSONC/serde.
- Because the shell owns line editing, `mu` needs **no** terminal/line-editor
  library at all — a further simplification over a REPL-owning design.
- Precedent: OpenAI Codex is Rust for the same reasons.

Tradeoff accepted: slower iteration than TypeScript, and no off-the-shelf
"AI SDK". Provider integration is hand-written against HTTP APIs (see §7); the
surface is small (chat completions + streaming + tool calls).

### 2.2 Architecture split: turn core + thin surfaces

`mu`-the-binary is a **turn runner**: prompt and attached inputs in, streamed
turn events out, completed state persisted, exit. It has no concept of modes,
prompts, key bindings, web sessions, or long-lived UI state.

Interactive surfaces are thin wrappers around that unit:

- The zsh plugin is the preferred interactive surface. It owns zsh line editing,
  prompt mode, and keybindings, then submits each entered prompt by spawning
  `mu` for one foreground turn.
- `mu-cli` is a temporary standalone REPL convenience wrapper. Each line entered
  in the REPL is submitted by spawning the `mu` command-line interface for one
  turn; it is not a second in-process agent host and may be removed once the
  shell surface is mature.
- `mu-web` is a future web daemon, similar in spirit to `opencode serve`. It
  coordinates sessions and presentation, but talks to the same `mu` command-line
  interface rather than carrying a separate agent loop.

This split is the central decision (see §3 for the full rationale recap). It
keeps the agent semantics small and scriptable while letting each surface choose
the interaction style that fits it.

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
The agent loads a skill's full `SKILL.md` on demand using `bash` (`sed`, `cat`,
`rg`, etc.). No dedicated "skill" tool — this keeps the model-visible surface at
one tool and makes skills "just files".

### 2.6 Flat config, single SQLite state file

All user-facing configuration is a flat directory of plain files
(`config.jsonc`, `AGENTS.md`, `skills/`). Runtime state is one SQLite database
in the active global/project scope. See §9, §10, and §11.

---

## 3. Architecture overview

`mu` has one core runtime and multiple thin surfaces. There is no client/server
split in V1; the "glue" is the command-line contract around a completed turn.

```
   ┌──────────────────────────── thin surfaces ────────────────────────────────┐
   │  shell scripts / editor tasks: `mu [opts] <<< PROMPT`                     │
   │  zsh plugin: prompt mode; every entered line spawns one `mu` turn         │
   │  mu-cli: temporary standalone REPL wrapper around one-turn invocations    │
   │  mu-web (V2): web coordinator around the same CLI turn boundary           │
   └───────────────────────────────────┬───────────────────────────────────────┘
                                        │ spawns / coordinates foreground turns
                                        ▼
   ┌──────────────────────────── mu (binary, per turn) ────────────────────────┐
   │  project/config/session resolution                                        │
   │  stdin prompt + repeatable `-i` image attachments                         │
   │     │                                                                     │
   │     ▼                                                                     │
   │  Agent loop ──► Provider client (HTTP/SSE)                                │
   │     │                                                                     │
   │     ├──► Tool registry: bash                                              │
   │     │                                                                     │
   │     ├──► Renderer (plain / terminal / JSON event stream)                  │
   │     │                                                                     │
   │     └──► Store (SQLite in active global/project scope)                    │
   └───────────────────────────────────────────────────────────────────────────┘
```

### Why this split (recap)

The hard part of "replace bash" is shell fidelity. Having the real shell own the
terminal gives that for free and forever; a core binary that owns a long-lived
REPL would have to reimplement completion, job control, and PTY handling
behavior indefinitely. The cost is (a) session state must persist across process
invocations — handled by SQLite in the active session scope, §11 — and (b)
interactive shell commands are not automatically visible to the agent, which V1
accepts (§6.3). Shell integration is allowed because it is only a line-editing
surface around repeated turn invocations, not a replacement runtime.

### Binary module responsibilities

- **Entry.** Resolve project/config/session scope, parse args (`--session`,
  `--continue-latest`, `--image`, `--output`, subcommands), read the prompt from
  stdin, run one turn, persisting each completed message as it lands (§11), exit.
- **Agent loop.** Send context to the provider, stream the response, execute
  tool calls, loop until the model stops requesting tools, yield final text.
  A configurable max-iterations guard bounds runaway loops (§11).
- **Tool registry.** The built-in `bash` tool with a JSON-schema parameter
  definition and an execute function.
- **Provider client.** Streaming HTTP to the model API behind one internal
  interface.
- **Renderer.** Sole writer to output; render the same turn as plain text,
  terminal UI, or JSON events (§5).
- **Store.** SQLite load/append in either project-local or global scope (§11).

The binary runs on a single `tokio` runtime. There is no input thread or line
editor — stdin is read once, fully, as the prompt.

### Binary CLI surface

The core binary is invoked one of two ways: as a **turn** (default, reads a
prompt on stdin) or as a **subcommand** (management, no prompt). The surface is
small:

- `mu [-s <id>] [-c] [--model <id>] [-i <image>] [--output plain|terminal|json]`
  — run one turn; prompt read from stdin. `-i/--image` is repeatable.
- `shell-plugins/mu.zsh` — zsh prompt mode; each accepted prompt runs one
  foreground `mu` turn and keeps using the same session.
- `mu-cli [-s <id>] [--model <id>] [--output plain|terminal|json]` — temporary
  standalone REPL wrapper around repeated `mu` turn invocations.
- `mu session new` — create a session and print its id.
- `mu session list` — list recent sessions.
- `mu compact --session <id>` — force compaction.

`mu-web` is explicitly V2.

`mu session new/list` do **not** require a configured provider. Turn invocation
and `mu compact` require a configured provider because they can contact the
provider (§7).

### Turn lifecycle (authoritative end-to-end flow)

This is the exact sequence the binary follows for one turn invocation. Implement
it in this order:

1. **Parse args**, resolve the active project (§9), read the entire prompt from
   stdin into a string, and load any attached inputs such as repeatable
   `-i/--image` image files.
2. **Load config** (§9): global first, then project config over it when a
   project is active. If the provider's required fields are missing, print an
   error to stderr and exit non-zero (§7). Resolve the effective model:
   `--model` if given, else the session's stored `model`, else the merged config
   default.
3. **Open the active-scope SQLite DB** (create + run migrations if absent):
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
     session row, and write its id to the runtime file named by
     `$MU_SESSION_FILE` when that env var is set (§11).
5. **Acquire the session `flock`** (§11). If held, print "session busy", exit
   non-zero.
6. **Build the context message list** from the DB: the latest compaction summary
   message (if any) followed by all messages after it, in order.
7. **Pre-turn compaction check** (§11, Tier 1): if the session's stored
   `last_total_tokens` (or bytes÷4 on the first turn) exceeds the configured
   fraction of the model context window, run compaction now, then rebuild the
   context list.
8. **Append the new user message** to the DB and to the context list. If images
   were attached, persist and reload the full multi-part user content
   (text + image URLs), not only the textual projection.
9. **Agent loop** — repeat until the model returns `finish_reason: "stop"` or the
   max-iterations cap is hit:
   a. Send the context list + tool definitions to the provider (streaming).
   b. Accumulate the streamed assistant message (text deltas and tool-call
      deltas; see §7 for the delta-accumulation rules).
   c. On stream end, record this response's `usage` (accumulate into the turn's
      running totals; see "usage accounting" below).
   d. **Persist the completed assistant message** (including any `tool_calls`)
      to the DB and append it to the context list.
   e. If `finish_reason` is `tool_calls`: execute `bash` calls one at a time in
      request order. Render and **persist tool result messages** (`role:
      "tool"`, with their `tool_call_id`) in the model's original call order,
      then loop back to (a). A future version may run contiguous
      `risk:"readonly"` calls concurrently once renderer interleaving and
      cancellation semantics are explicitly designed.
   f. If `finish_reason` is `stop`: the loop ends.
10. **Update the session row:** `last_total_tokens` = the `total_tokens` from the
    *final* provider response of the turn (this already reflects the full context
    incl. prior turns); add this turn's cost to `cost_total`; bump `updated_at`.
11. **Print the turn summary line** to stderr (§5), release the `flock`, exit 0.

**Usage accounting.** Each provider response in the loop carries its own `usage`.
For the **context fullness** figure (`last_total_tokens`, the summary's
`context: N%`) use only the **final** response's `total_tokens` — because
`prompt_tokens` already includes the entire prior context, the last response's
`total_tokens` is the true current context size (summing across iterations would
double-count). For **cost** and the `in`/`out` token display, sum
`prompt_tokens`/`completion_tokens` across all iterations of the turn (you pay
for each round-trip).

**Interruption.** Steps 9d/9e persist only *after* a message is fully formed. If
SIGINT / a dropped connection / a provider error occurs mid-stream, the partial
message is never written; the process dies and the DB holds only completed
messages (§11). No tool call begins without first persisting its parent
assistant message. Tool results are persisted in request order immediately
after a sequential call or concurrent batch completes, so an uninterrupted
turn produces API-valid history.

---

## 4. Tools

The model-visible tool surface is exactly:

```ts
bash({
  title: string,
  risk: "readonly" | "reversible" | "destructive",
  script: string,
  cwd?: string,
  timeout?: number, // seconds, default 120
  stdin?: string
})
```

`title` is the short human-readable action shown in the terminal. `risk` is
advisory metadata for UI/audit; V1 does not sandbox, approve, or restrict a call
based on it. `script` is executed as `bash -lc <script>`. `cwd`, when present,
applies only to that invocation; `cd`, shell variables, and exported environment
do not persist to later bash calls. `stdin`, when present, is piped literally to
the child process so the agent can write bytes containing `$`, backticks,
quotes, or heredoc delimiters without shell expansion.

**Execution ordering.** V1 executes every `bash` call sequentially in request
order. This keeps filesystem effects, terminal output, and cancellation simple
while the tool is unsandboxed. A future version may run contiguous
`risk:"readonly"` calls concurrently because per-call bash processes are
isolated.

**Terminal visibility.** `bash` prints `$ [risk] <title>`, streams combined
output, and finishes with exit status/duration. Every tool error is visible.
TTY output uses OpenCode-inspired color and glyphs, including colored risk
labels; redirected output uses ANSI-free ASCII equivalents.

**Output truncation policy.** Following opencode's model, every bash output is
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
signaling fails it falls back to killing the direct child. Non-Unix platforms use
best-effort direct child termination in V1; stronger job/cgroup handling is V2.

`timeout` defaults to 120 seconds and must be greater than zero. `mu` does not
pre-check script argv size; if `bash -lc <script>` fails with OS
argument-list-too-long (`E2BIG`), the tool returns a clear error. V1 does not
fall back to temp scripts.

---

## 5. Output and rendering

`mu` supports three output formats: `plain`, `terminal`, and `json`. They are
different renderings of the same agent turn and must not imply different agent
behavior.

- **Plain text** is for simple scripting and low-friction terminal use. It
  prioritizes assistant/tool text and avoids terminal-specific control.
- **Terminal output** is for humans in an interactive terminal. It may use color
  and may update recent status lines for in-progress activity, but it must keep
  normal scrollback. It must not use an alternate screen, clear the screen, or
  require mouse interaction.
- **JSON output** is for programs, integrations, and the future web UI. It is a
  delimited serialization of the same turn events used by the other output
  formats, suitable for incremental consumers (newline-delimited JSON is the V1
  shape).

The renderer is the sole writer to stdout/stderr and enforces the selected
format. It may style output only when stdout is a TTY and `--output terminal` is
selected. It never clears the screen, uses an alternate screen, or requires
mouse interaction. Plain/JSON output is always ANSI-free.

Assistant Markdown is parsed on TTYs. The renderer buffers only the current
unstable Markdown block, commits completed blocks once, and flushes the tail at
the response boundary. Headings, emphasis, links, lists, quotes, tables, inline
code, and fenced code receive terminal styling without modifying prior output.
When stdout is piped or redirected, assistant deltas pass through byte-for-byte
as the model produced them, preserving raw Markdown for downstream consumers.

**Stream routing (explicit).** The conversation transcript goes to **stdout**:
tool presentation, tool failures, Bash output, and assistant text. Fatal process
errors and the turn summary go to **stderr**. Thus `mu <<< prompt > out.txt`
captures the complete portable transcript while fatal diagnostics/summary
remain visible. Stdout TTY detection selects rich versus portable rendering;
stderr TTY detection suppresses the summary when redirected.

- **Tool presentation.** Bash prints `$ [risk] <title>`, streams ANSI-sanitized output,
  and prints its final exit status. Full tool results still follow the shared
  model-context truncation policy (§4).
- **Assistant text.** Raw deltas stream unchanged when redirected. TTY display
  commits parsed Markdown blocks as soon as they are stable; only the current
  incomplete block is delayed.
- **Errors.** Always printed and clearly prefixed, with TTY styling and a plain
  ASCII fallback. Fatal turn failure produces a non-zero process exit code so
  the shell's `$?` is meaningful.

**Turn summary line.** When `mu` exits normally (turn complete), it prints a
single structured summary line to stderr:

```
[mu] tokens: 1234 in / 456 out  context: 12%  cost: $0.003
```

All figures come from the provider's reported `usage` for the turn: `in`/`out`
are `prompt_tokens`/`completion_tokens`; `context` is the new
`total_tokens` ÷ model context window; `cost` is `total_tokens` priced via the
optional per-model rates in `config.jsonc` (omitted if no rates configured).
This is the *only* stderr output in the normal case. It appears after all
stdout, and goes to stderr so it stays out of a captured stdout transcript. It
is suppressed if stderr is not a TTY (piped/redirected), since it would pollute
log files.

Plain and JSON modes avoid terminal-only summaries and control sequences so they
remain suitable for scripts. Human terminal mode may show progress for
in-flight work, but committed transcript content is never erased from scrollback.

---

## 6. zsh shell surface

The zsh plugin is the preferred interactive surface. It behaves like a shell
editing mode: Tab on an empty shell prompt switches the current prompt to `mu>`,
Enter submits the current buffer as one `mu` turn, Ctrl-C clears the current
`mu>` buffer and draws a fresh `mu>` prompt, and Ctrl-D or Backspace on an empty
`mu>` prompt returns to the normal shell prompt without printing a new line. The
plugin must not duplicate agent-loop, provider, store, or tool semantics.

`mu-cli` remains available for now as a temporary standalone REPL wrapper, but it
is not the architectural owner of interactive use and is expected to disappear
once the shell surface is mature.

### 6.1 Invocation pattern

Submitting a non-empty prompt runs `mu` as an ordinary foreground child process.
The plugin passes `--session` after the first turn, forwards configured output
mode, writes the prompt to the child process's stdin, waits for the turn to
finish, and then redraws `mu>` with the same session id.

Consequences:

- `mu` owns the terminal while each turn is running; streaming output works
  directly.
- Ctrl-C while editing in `mu>` mode clears the current buffer and redraws
  `mu>` like a shell prompt interrupt. Ctrl-C while a foreground `mu` turn is
  running uses ordinary Unix signal behavior for the foreground process.
- After each turn exits, zsh returns to `mu>` mode with the same session id.

### 6.2 Entry and exit

- Source `shell-plugins/mu.zsh` from `.zshrc`.
- Press Tab on an empty shell prompt to enter `mu>` mode.
- Enter a non-empty line to run one turn.
- Press Ctrl-C while editing to discard the current buffer and draw a fresh
  `mu>` prompt.
- Press Ctrl-D, or Backspace on an empty `mu>` prompt, to leave `mu>` mode and
  restore the normal shell prompt in place.
- Ctrl-D is the normal terminal EOT key (`^D`). xterm-style terminals, including
  WebTerm's xterm.js input path, forward it as input when the browser or OS has
  not intercepted the key before the terminal receives it.

### 6.3 Context boundaries

- **Full structured history:** `mu` records prompts, assistant responses, and
  tool calls in SQLite (§11). Tool output is stored with the shared
  truncation/spill policy, so the DB keeps the structured transcript and spill
  files hold oversized raw command output.
- **No shell-command sharing (V1):** commands run outside `mu` or the shell
  plugin are
  not automatically fed to the agent. Bridging interactive shell activity into a
  session is deferred; V1 keeps the boundary explicit and private.

### 6.4 Session management

Session lifecycle is exposed through CLI commands:

- The zsh plugin without a session lazily creates one on its first submitted
  prompt and reuses that session for later prompts in the same shell.
- `mu-cli -s <id>` starts the temporary standalone REPL attached to an existing
  session.
- `mu -c` continues the latest session in the active scope for a one-shot turn.
- `mu session new` creates a session and prints its id.
- `mu session list` lists recent sessions.
- `mu compact --session <id>` compacts a session on demand.

---

## 7. Provider / model integration

A single internal trait abstracts the model API. The implementation is
hand-written against the provider HTTP endpoint; the needed surface is small:

- Send messages + tool definitions, receive a streamed response.
- Stream deltas for assistant text and for tool-call arguments.
- Report token usage.

**V1 target: OpenAI-protocol chat-completions over HTTP, API key only.** This is
the single most widely implemented contract — it covers OpenAI itself, the many
compatible cloud gateways, and (importantly) local model servers
(llama.cpp/`llama-server`, vLLM, LM Studio, Ollama's OpenAI endpoint, etc.).
A configurable **base URL** plus a bearer **API key** is therefore the whole
auth/transport story for V1. Subscription/OAuth providers (Claude Pro, ChatGPT)
and the Anthropic-native protocol are out of scope for V1.

**Request shape.** `POST {base_url}/chat/completions` with
`Authorization: Bearer {key}`, `stream: true`, `model`, the `messages` array,
and a **`tools` array** carrying the `bash` tool definition
`{type:"function", function:{name, description, parameters: <JSON schema>}}`).
Tool definitions go in this dedicated `tools` parameter — **not** embedded in the
system prompt. Request `stream_options:{include_usage:true}` so the final SSE
chunk carries `usage`.

**Streaming accumulation (implement exactly).** The response is an SSE stream of
`data: {json}` lines ending with `data: [DONE]`. For each chunk, look at
`choices[0]`:
- `delta.content` (string, may be null) — append to the assistant text buffer.
- `delta.tool_calls` — an array of partial entries, each with an `index`. For a
  given `index`, the first delta carries `id`, `type`, and `function.name`;
  later deltas carry only `function.arguments` *string fragments* that must be
  **concatenated** (they are not valid JSON until joined). Maintain one
  accumulator per `index`.
- `finish_reason` — `"stop"` (assistant is done, end the loop) or `"tool_calls"`
  (execute the accumulated tool calls, then loop). Parse each tool call's joined
  `function.arguments` as JSON only after the stream ends.
- The trailing chunk with `usage` populates this response's token counts.

**Model context window.** The 75% threshold needs the model's max context size.
Source it from `config.jsonc`: each configured model entry carries a
`context_window` integer (and optional price rates). mu does not fetch model
cards. If a model has no configured `context_window`, compaction's Tier 1 is
skipped for it and Tier 2 (API-error fallback) is the only guard.

Model and provider selection come from `config.jsonc`: a `base_url`, the env var
holding the API key, and a default model id. If the global config file is
missing, `mu` creates a starter `~/.mu/config.jsonc` automatically before
loading configuration. API keys are read from environment variables; `mu` does
not store secrets in its database.

**No provider, hard fail.** If no provider is configured (no base URL, or the
key env var is unset), a *turn* invocation exits immediately with a non-zero
status and a clear message pointing at `config.jsonc` and the expected env var.
`mu compact` follows the same rule because it calls the provider. There is no
silent fallback once configuration has been loaded.

Because the canonical message history is stored in a provider-neutral form
(§11), swapping the base URL/model across turns is supported without migration;
a future second protocol (e.g. Anthropic-native) can be added behind the same
trait.

---

## 8. Skills

Skills are reusable, on-demand instruction bundles, following the opencode/pi
shape and the broader Agent Skills convention.

- A skill is a directory under the config `skills/` folder containing a
  `SKILL.md` whose YAML front-matter defines `name` and `description`. The body
  is markdown instructions; the directory may hold supporting scripts/files.
- On startup `mu` scans `skills/`, parses front-matter (bounded name/description
  lengths, graceful warnings on malformed files), and injects a compact
  `<available_skills>` block — name, description, absolute `SKILL.md` path — into
  the system prompt.
- When a task matches a skill, the model reads the full `SKILL.md` via `bash`,
  using the **absolute path** from the injected `<available_skills>` block. Any
  relative paths *written inside* a `SKILL.md` (e.g. `scripts/foo.sh`) are
  documented to resolve against the skill's own directory; the system prompt
  states this so the model expands them to absolute paths before calling tools.

Progressive disclosure: only short metadata is always in context; full
instructions are pulled in on demand. No dedicated tool, no registration — skills
are files discovered on startup (and cached, §13).

---

## 9. Project discovery

On startup, `mu` searches upward from the current working directory to find an
active project.

A directory is a project when it contains `.mu`.

A directory is also a project when it contains `.git`. In that case, `.mu` is
created adjacent to `.git` only when `mu` needs to write project state. Merely
discovering or reading project information must not mutate the filesystem.

If the search reaches the user's home directory or the filesystem root without
finding a project, no project is active for the turn and global state/config is
used.

Nested project merging is not supported. The first project found while walking
upward is the active project.

Git worktrees are treated as their own projects. When a worktree is active, the
worktree root is the project root. The agent should be told both the project
root and the current working directory. It should also be told relevant worktree
information when available.

The shell tool's working directory defaults to the process working directory,
not the project root.

The project-local directory is `.mu`. It contains:

- `config.jsonc`, the project configuration.
- `.env`, optional local environment values.
- `AGENTS.md`, the project-local agent instructions.
- `skills/`, the project-local skills directory.
- `sessions.db`, the project-local session history and state database.
- `.gitignore`, which ignores session database files and related SQLite files.

Project state is private to the project. A project should be movable and
understandable by inspecting its `.mu` directory, while still avoiding committing
volatile session state by default.

---

## 10. Configuration

`mu` has global configuration and optional project configuration. The global
configuration directory is `~/.mu` by default (or `$MU_CONFIG_DIR` when set).
Project configuration lives in the active project's `.mu` directory.

The global and project directories have the same conceptual shape:

```
~/.mu/ or <project>/.mu/
  config.jsonc      # provider base_url + key env var + model; optional tuning
  .env              # optional environment values for provider lookup + bash
  AGENTS.md         # agent instructions, appended to system prompt
  skills/
    <skill-name>/
      SKILL.md      # front-matter: name, description; body: instructions
      ...           # optional supporting files
```

When a project is active, global configuration is loaded first and project
configuration is merged over it. Project values take precedence. Parent project
configuration is not merged because nested projects are not supported. When no
project is active, only global configuration is used.

Optional `.env` files are loaded with the same scope precedence:
process environment first, then global `.env`, then active-project `.env`.
The resulting effective environment is used for provider API-key lookup and is
passed to every `bash` tool process. `.env` files are parsed as dotenv data, not
sourced as shell scripts.

Configuration and session storage are related but separate concepts. Config is
merged across scopes; sessions are selected from exactly one scope: project when
inside a project, global otherwise.

- **config.jsonc** — JSON with comments and trailing commas. Concrete shape
  (field names are normative):

  ```jsonc
  {
    "provider": {
      "base_url": "https://api.openai.com/v1",  // required
      "api_key_env": "OPENAI_API_KEY"            // required: env var NAME, not the key
    },
    "default_model": "gpt-4o",                   // required
    "models": {                                  // optional per-model tuning
      "gpt-4o": {
        "context_window": 128000,                // needed for Tier-1 compaction & context%
        "price_per_mtok": { "input": 2.5, "output": 10.0 }  // optional, for cost line
      }
    },
    "compaction": { "fraction": 0.75, "keep_recent_turns": 2 },  // optional
    "limits": { "max_iterations": 50, "max_lines": 2000, "max_bytes": 51200, "max_line_bytes": 10240 },
    "redaction": {
      "env": ["GITHUB_TOKEN"]                    // optional; provider api_key_env is implicit
    }
  }
  ```

  Only `provider.*` and `default_model` are required; everything else has the
  defaults shown. If global `config.jsonc` is missing, `mu` creates a starter
  file automatically. `mu` hard-fails on a turn if the required fields are
  missing or the API-key env var is unset (§7).
- **.env** — optional dotenv data. Values are visible to `bash`; this is
  convenience, not sandboxing. Values from provider `api_key_env` and
  `redaction.env` are exact-value redacted from bash output before the output is
  stored or shown to the model. Empty redaction values are ignored with a
  warning. Short redaction values are still redacted with a warning.
- **AGENTS.md** — system-prompt addendum. Global instructions are loaded first;
  active-project instructions are appended after them when a project is active.
  Both are included; "project overrides global" means later text wins by
  convention, not that global instructions are dropped.

The system prompt is intentionally minimal and assembled in this fixed order:

1. A short role/behavior preamble (a few sentences). Illustrative:
   > You are mu, a terminal agent. You execute the user's request using the
   > available `bash` tool, then stop. Use `bash` for local search, file reads,
   > writes, edits, web fetches, tests, and any other CLI work. Each bash call is
   > isolated; pass `cwd` explicitly when needed. Keep responses concise.
   The exact wording lives in one constant in the binary; keep it short.
2. An environment block, plain `key: value` lines:
   ```
   <env>
   cwd: /home/user/project/subdir
   project: active
   project_root: /home/user/project
   session_id: ...
   os: linux
   date: 2026-06-18
   </env>
   ```
   If no project is active, `project: none` is used and `project_root` is
   omitted. Worktree information is included when available.
3. The `<available_skills>` block (§8), or omitted if there are no skills. Skill
   metadata is merged from global and active-project skill directories.
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

- **session** — `id`, `created_at`, `updated_at`, `cwd`, `model`, `title`,
  `last_total_tokens` (the most recent `usage.total_tokens` reported by the
  provider; used for the pre-turn overflow check, §"Context window and
  compaction"), `cost_total` (accumulated USD, for the turn summary). `model` is
  set at session creation from the effective model (lifecycle step 2); a later
  `--model` overrides for that turn only and does **not** rewrite the stored
  value. `cwd` records the last working directory used for that session. `title`
  is set lazily from the first user prompt (first ~60 chars) and is display-only
  for `mu session list`.
- **message** — `id`, `session_id`, `role` (`user`/`assistant`/`tool`/`summary`),
  `content`, optional full user content JSON for multi-part inputs, `created_at`,
  ordering index, and for tool results `tool_call_id`. Provider-neutral
  representation. For user messages with image attachments, `content` remains a
  textual projection for listing/token estimates, while the full text+image
  parts are persisted and reloaded for model context. A `summary` row is a
  compaction summary (§"Context window and compaction"); the context builder
  starts from the latest `summary` row and includes everything after it.
- **tool_call** — `id`, `message_id`, `tool`, `args` (JSON), `risk`, `output`,
  `status`, timings. Records the agent's tool invocations for inspection and the
  renderer's truncation pointers. (Tool *results* fed back to the model are stored as
  `tool` messages; this table is the structured audit copy.)

### Session mapping

V1 maps each interactive shell instance to at most one active session:

- **Lazy creation.** On the first submitted prompt, the zsh plugin invokes `mu`
  without `--session`, exporting `MU_SESSION_FILE` to a temporary path. `mu`
  creates the session row and writes the new id to that file. After `mu` exits,
  the plugin reads the id and passes `--session <id>` on later prompts. The id is
  never printed to stdout by the turn, so the transcript stays clean.
- **Attach / continue.** `mu-cli -s <id>` temporarily remains available to attach
  the standalone REPL to an existing session. `mu -c` continues the latest
  session for a one-shot turn. `mu session list` lists candidates.
- **Per-turn lifecycle.** Each turn: open DB → acquire session lock → load
  session messages → run turn (persisting each completed message as it lands) →
  release lock → exit. The connection opens lazily so a turn that errors early
  stays cheap.

Sessions are append-only logs; resuming replays messages into the context
window. Multiple shells holding *different* sessions run concurrently (safe under
WAL).

### Agent environment context

For each turn, the agent should know:

- The current working directory.
- Whether a project is active.
- The project root, if one is active.
- The active session id.
- Relevant git worktree information, if the project is a worktree.

The current working directory remains important even inside a project because it
reflects the user's immediate intent. Project root provides broader context, but
it should not replace `pwd`.

Stable environment information is introduced once, when the session is created.
This includes project path, active project status, session id, and git worktree
information. Later turns in the same session should not repeat that full
environment block.

If the working directory changes after session creation, the next turn should
include the new `pwd` as additional context and update the session's stored
`cwd`. This keeps the model aware of meaningful movement without making every
turn restate the full environment.

### Message-level persistence and interruption

Persistence is at **message granularity**, and only **completed** messages are
written:

- As the turn proceeds, each fully-formed message (a complete assistant message,
  a complete tool result) is committed to SQLite as it lands.
- A partial/in-flight message is **never** persisted. If the turn is interrupted
  — Ctrl-C (SIGINT kills the process), a dropped connection, a provider error —
  the incomplete message is simply discarded. Because `mu` is a per-turn process,
  the partial state dies with the process; there is nothing to clean up.
- On the next turn, context is reconstructed purely from completed messages, so
  the history is always API-valid (no dangling tool-call without a result, the
  problem opencode and codex have to actively repair because they persist
  in-flight turns). The user simply re-prompts, or retries, from the last
  committed message.

This is simpler than opencode (which persists the partial turn and rewrites
dangling tool calls into synthetic "interrupted" errors) and codex (which reads
back a partial JSONL rollout) — both complications that only exist because those
harnesses are long-lived. mu's per-process model makes "discard the incomplete
message" correct and trivial.

### Session concurrency lock

Two processes can target the same session, so concurrent turns against one
session are possible and must be serialized. Each turn acquires a per-session
lock for its duration; a second `mu` targeting the same session finds it held
and **fails fast** with a "session busy" message rather than interleaving
writes.

The lock is an advisory `flock` on a per-session lock file under
`$XDG_RUNTIME_DIR/mu/` (one file per session id), acquired in lifecycle step 5
before any DB writes. This is deliberately *not* a SQLite-level lock:
`BEGIN IMMEDIATE` takes a reserved lock on the **whole database file**, which
would serialize unrelated sessions against each other. A per-session `flock`
lets different sessions proceed independently.

WAL caveat: WAL lets readers and writers not block each other, but **two writers
still serialize at the SQLite level** (only one write transaction at a time).
Different sessions writing concurrently is rare and brief (per-turn appends), so
mu sets a `busy_timeout` (e.g. 5s) on the connection to ride out the momentary
contention rather than erroring. The `flock` handles same-session serialization;
`busy_timeout` handles the rare cross-session write overlap. opencode's
busy-check is an in-process guard, which does not translate to mu's multi-process
model — mu needs the real cross-process `flock`.

### Context window and compaction

**Token counting (source of truth).** mu does not run a tokenizer. It uses the
**provider's reported usage** — every OpenAI-protocol response carries a `usage`
object with `prompt_tokens`, `completion_tokens`, and `total_tokens`. mu stores
the latest `total_tokens` on the session after each turn; that figure is the
authoritative measure of how full the context is. This is exactly what opencode
(`tokens.total` from the provider) and codex (`last_token_usage.total_tokens`)
do.

A `bytes ÷ 4` approximation (`approx_tokens(s) = ceil(len_bytes(s) / 4)`, the
same constant both opencode and codex use) is used only where no API figure
exists yet:
- the **first turn** of a session (no prior `usage` reported), to size the very
  first request;
- estimating the size of **not-yet-sent** content (e.g. which messages to keep
  when building a compaction), where the provider has not yet returned a count.

Context management then uses a **two-tier strategy**:

**Tier 1 — graceful pre-turn compaction (75% threshold).** At the start of each
turn, mu compares the stored `total_tokens` from the previous turn (or the
bytes÷4 estimate on the first turn) against the model's context window. If it
exceeds a configurable fraction (default 75%), mu compacts *before* sending the
new turn. Because this runs between turns, it is fully graceful — no turn is
wasted, no replay.

**Tier 2 — hard-stop on API overflow error.** The pre-turn figure can lag (a
single turn may add a very large tool result). If the provider returns a
context-length error during a turn, mu catches it, compacts immediately, and
retries the turn once. If the retry also overflows, the turn is aborted with a
clear message.

**Compaction algorithm** (same in both tiers): summarize everything up to a
cut point into a single **`summary` message row** (replacing any prior summary
row), keeping the most recent N turns (default 2, configurable) as verbatim rows
after it. The next turn's context builder (lifecycle step 6) loads the latest
`summary` row plus all rows after it — so compacted history is naturally
excluded without deleting anything. The original rows remain in SQLite (the
on-disk transcript is lossless); only the in-context working set shrinks. When a
prior summary row exists, the new one is generated by updating it ("update the
anchored summary, preserve still-true details, remove stale facts").

**Manual compaction.** `mu compact --session <id>` forces compaction on demand.

Contrast with opencode: opencode detects overflow *after* a turn completes (from
the reported token count), compacts, then replays the user's last message — so an
overflow turn costs double. mu's pre-turn check (using the same reported count,
just consulted before the next turn instead of after the last one) avoids that
waste, and Tier 2 covers the same edge case codex's mid-turn compaction does.

### Agent-loop bounds

The agent loop runs until the model stops requesting tools. A configurable
**max-iterations** cap (default **50** tool round-trips, `limits.max_iterations`)
bounds a runaway loop: on reaching it, `mu` stops, emits a clear notice, and
exits non-zero, leaving all completed messages persisted so the user can inspect
and re-prompt. In "yolo" mode with no approval gate, this cap is the main guard
against a loop silently burning tokens.

**Exit codes.** `0` success; `1` general/config/provider error; `2` session busy
(lock held) or `--session` not found; `130` interrupted by SIGINT (the shell's
default for Ctrl-C). The summary line is printed only on exit `0`.

### Abort, pause, and resume

Abort means the current language-model request or tool execution is cancelled
when possible, the turn stops, and `mu` exits. Abort is an explicit interruption
of work in progress; completed messages remain persisted and partial messages
are discarded as described above.

Pause and resume are V2 features. The initial design does not promise pausing at
tool-call boundaries or resuming a partially completed turn.

---

## 12. Safety posture

V1 is deliberately **unsandboxed and unconfirmed** — "yolo" by design. `mu` runs
whatever the agent asks through `bash`: commands execute directly, files can be
read or modified directly, and there are no allow/deny lists, per-action
confirmation prompts, or sandbox. The `risk` field is audit metadata, not an
enforcement boundary. This matches the goal of a fast, frictionless day-to-day
driver and the reality that the user is already living in a shell with the same
powers.

The protections that remain are cheap and non-intrusive:

- **Visibility is the safeguard.** Output is non-magical and append-only, so the
  user sees exactly what ran and what it produced — scrollback plus the SQLite
  transcript are the audit trail. Nothing happens off-screen.
- **Interruptibility.** Because `mu` runs as a foreground job, Ctrl-C aborts the
  turn (and the in-flight tool) immediately, which is the practical "stop"
  button.
- **Secrets** are never persisted by `mu`; provider keys come from the
  environment or `config.jsonc`, never the database.
- **External content** (file contents, command output, fetched pages, web search
  results from CLIs, etc.) is treated as untrusted data, not as instructions to
  follow.

Sandboxing and an approval/policy layer are explicitly out of scope for now and
can be layered on later without changing the architecture, should the threat
model warrant it.

### 12.1 Guardrail (optional)

An opt-in review gate for destructive commands. When enabled, a separate model
call assesses each `bash` call whose declared `risk` is `"destructive"` before
execution. The reviewer returns `risk_level`, `user_auth_level`, and `reason`;
the action executes only if `user_auth_level >= risk_level` on a fixed ordinal
scale. There is no interactive y/n prompt — denied actions return as tool
errors so the agent can adapt or ask the user.

**Ordinal scale.** Both levels map to integers; the gap between `high`(2) and
`critical`(4) ensures only `explicit`(4) authorization can approve critical-risk
actions:

| | unknown | low | medium | high | critical |
|---|---|---|---|---|---|
| **rank** | 0 | 1 | 2 | 3 | 4 (auth) / 4 (risk) |

`user_auth_level >= risk_level` yields:
- `low`(0): allowed by any auth level including `unknown`(0).
- `medium`(1): requires at least `low`.
- `high`(2): requires at least `medium`.
- `critical`(4): requires `explicit` — the only level that can approve it.

**Reviewer call.** A separate non-streaming chat-completions call inside the
turn process (mu is per-turn, so there is no persistent reviewer session). The
reviewer uses the same provider and API key as the primary agent; the model
defaults to `default_model` but can be overridden via `guardrail.review_model`.
The reviewer has no tools — it judges from a compact transcript and the action
JSON alone.

**Context sent to the reviewer.** A filtered, budgeted transcript (user +
assistant + tool-call arguments + tool results, skipping the system message),
with the same token caps as Codex: 10 000 for messages, 10 000 for tools, 2 000
per message entry, 1 000 per tool entry, 40 recent non-user entries. Truncation
keeps prefix + suffix with a `<truncated omitted_approx_tokens="N"/>` marker.
The planned action is provided as pretty-printed JSON (capped at 16 000 tokens).

**Reviewer system prompt.** A trimmed port of Codex's Guardian policy prompt
(`src/guardrail/policy.md`), adapted for mu: "terminal-agent" framing, no
sandbox/escalation concepts, no tool-check instructions (the reviewer has no
tools), and no model-emitted allow/deny decision (the decision is computed from
the ordinal comparison). The prompt covers evidence handling (transcript =
untrusted), user authorization scoring (including `explicit`), base risk
taxonomy, risk category rules, and a strict JSON output contract.

**Outcomes.**

- **Allow** (`auth >= risk`): the bash call executes. A `[guardrail: allow]`
  line is rendered before execution.
- **Deny** (`auth < risk`): the bash call does not execute. A `[guardrail: deny]`
  line is rendered, and a tool error is returned to the agent:
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
  "enabled": false,                          // default off (preserves yolo default)
  "review_model": null,                      // null → same as default_model
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

## 13. Startup-speed plan

Startup latency is paid every turn, so it is defended explicitly:

- Single binary; no runtime bootstrap; no line-editor/terminal library to
  initialize.
- Lazy initialization: open the DB, scan skills, and construct the provider
  client only when first needed for the turn.
- Skill scanning reads only front-matter, not bodies, and is cached in the DB
  keyed by directory mtime to skip re-parsing on subsequent launches.
- SQLite in WAL mode; a small prepared-statement set; minimal schema.
- No network calls before the prompt is read; the provider connection opens once
  the turn actually starts.
- Config parsing is a single small file.

Target: process-ready (DB + config + skills resolved) within a few milliseconds
on a warm filesystem, so the only perceptible latency is model time-to-first-token.

---

## 14. High-level implementation phases

Coarse sequencing only (not an execution plan):

1. **Binary skeleton.** Arg parsing, stdin prompt read, config loading,
   plain append-only renderer, fail-fast when no provider configured. Echoes
   prompt; no model.
2. **Provider + loop.** OpenAI-protocol chat-completions (base URL + API key),
   streaming, the agent loop with `bash`, max-iterations guard.
3. **Shell-only tool surface.** Keep `bash` as the sole model-visible tool;
   implement shared output truncation + spill files, literal stdin, per-call
   cwd, timeout cleanup, and process-group teardown.
4. **State.** SQLite store (WAL), message-level persistence, session load,
   `--session`, lazy session creation + runtime-file handshake, per-session
   lock, two-tier compaction, exit turn-summary line.
5. **mu-cli.** Interactive prompt loop, first-turn session capture, `-s`
   attach, `--model`, `--output`, and clean exit commands.
6. **Skills.** Skill scan + cache + system-prompt injection, `AGENTS.md`
   (global + project).
7. **Polish.** Robustness, error-message quality, config surface.
