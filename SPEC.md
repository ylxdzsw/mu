# mu — Design Specification

`mu` is a fast agent harness for the terminal. It is built as two pieces: a
small, plain command-line binary that reads a prompt on stdin, runs an agent
loop, and streams logs and responses to stdout/stderr; and a zsh plugin that
adds an "agent mode" to your normal interactive shell. The long-term goal is a
tool you live in day to day, augmenting (and eventually rivaling) the shell as
the primary terminal interface.

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
- **Non-magical.** No TUI. The shell owns the terminal and line editing; `mu`
  just reads a prompt and appends output. Output streams as it is produced (a
  tool line may appear before its output), but once a line is printed it is never
  rewritten or erased.
- **Minimal.** One model-visible tool: `bash`. A flat config directory. A
  single SQLite file for state. The binary itself has no interactive input
  handling.
- **Native shell.** In normal use you are in unmodified zsh — full completion,
  history, job control, aliases, plugins, and interactive programs (vim, htop,
  ssh) all work because `mu` never replaces the shell.
- **Day-to-day general purpose.** Coding is supported but not the focus. The
  agent is a general terminal assistant.

### Non-goals

- **No TUI, no REPL inside `mu`.** No alternate screen, no full-screen layout,
  no widgets, no in-place history editing, no mouse, no line editor in the
  binary. `mu` never puts the terminal into raw mode.
- **No re-rendering.** Lines are written once and never rewritten. Native
  terminal scrollback is the history mechanism.
- **No daemon (V1).** Each turn is a fresh, stateless-on-exit process that
  loads/saves session state from SQLite. No background server.
- **No plugin SDK, no MCP, no sub-agents (initially).** Extensibility is via
  skills (markdown) and `bash` (call any CLI tool).
- **zsh only (V1).** Other shells (bash, fish, nushell) may get plugins later;
  the binary is shell-agnostic, only the interactive integration is zsh-specific.

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

### 2.2 Architecture split: filter binary + zsh plugin

`mu`-the-binary is a **filter**: prompt in (stdin), streamed output out
(stdout/stderr), exit. It has no concept of modes, prompts, or key bindings.

All interactivity lives in a **zsh plugin**. The plugin adds an *agent mode* to
the shell's line editor (ZLE). Entering agent mode changes the prompt and
rebinds a few keys; submitting runs `mu` as an ordinary foreground command.

This split is the central decision (see §3 for the full rationale recap). It
gives 100% real shell fidelity for free, keeps the binary tiny, and reuses
zsh's editor, history, and job control rather than reimplementing them.

### 2.3 Agent mode is a zsh editor mode

Modeled on the Julia REPL's mode switching:

- **Entry (explicit, default):** a dedicated key binding (configurable in
  `config.jsonc`, e.g. `Alt-M` or another combo) enters agent mode. This is
  the default and only entry method.
- **Entry (magic space, opt-in):** when magic space is enabled in config, typing
  `mu` at an empty shell buffer and pressing Space enters agent mode instead of
  inserting a space. Disabled by default because it prevents typing `mu <args>`
  as a normal shell command (e.g. `mu init zsh`). When enabled, call `mu`
  subcommands as `command mu init zsh` (or with a leading space if
  `HIST_IGNORE_SPACE` is set) to bypass the trigger.
- **Exit:** Backspace on an empty agent-mode line returns to normal shell mode
  (symmetric, Julia-like). Esc also exits.
- **Submit:** Enter runs the prompt through `mu`. Because submission happens by
  accepting an ordinary command line, `mu` runs as a normal foreground job — it
  owns the TTY naturally, streaming works, and **Ctrl-C delivers SIGINT
  straight to `mu`** with no signal-routing machinery.

See §6 for the full plugin behavior.

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
(`config.jsonc`, `AGENTS.md`, `skills/`). All runtime state (sessions, messages,
tool calls) is one SQLite database. See §9 and §10.

---

## 3. Architecture overview

`mu` is two cooperating components. There is no client/server split and no
daemon; the "glue" is the zsh process the user already runs.

```
   ┌──────────────────────────── zsh (user's shell) ───────────────────────────┐
   │  Normal mode: unmodified zsh — completion, history, jobs, plugins, vim…    │
   │                                                                            │
   │  mu plugin (ZLE):                                                          │
   │    · dedicated key (default) / magic-space (opt-in) → enter agent mode      │
   │    · agent-mode accept-line → run `mu --session $MU_SESSION_ID <<< PROMPT` │
   │    · empty-line backspace / Esc → exit agent mode                          │
   │    · disables syntax-highlight + autosuggest while in agent mode           │
   └───────────────────────────────────┬────────────────────────────────────────┘
                                        │ spawns (foreground job, owns TTY)
                                        ▼
   ┌──────────────────────────── mu (binary, per turn) ─────────────────────────┐
   │  stdin → prompt                                                            │
   │     │                                                                      │
   │     ▼                                                                      │
   │  Agent loop ──► Provider client (HTTP/SSE)                                 │
   │     │                                                                      │
   │     ├──► Tool registry: bash                                               │
   │     │                                                                      │
   │     ├──► Renderer (append-only TTY UI / portable stdout transcript)        │
   │     │                                                                      │
   │     └──► Store (SQLite: load session on start, append per completed msg)   │
   └────────────────────────────────────────────────────────────────────────────┘
```

### Why this split (recap)

The hard part of "replace bash" is shell fidelity. Having the real shell own the
terminal gives that for free and forever; a REPL-owning binary would have to
reimplement completion, job control, PTY handling, and plugin behavior
indefinitely. The cost is (a) session state must persist across process
invocations — handled by SQLite + a shell variable, §10 — and (b) interactive
shell commands are not automatically visible to the agent, which V1 accepts
(§6.7).

### Binary module responsibilities

- **Entry.** Parse args (`--session`, subcommands), read the prompt from stdin,
  run one turn, persisting each completed message as it lands (§10), exit.
- **Agent loop.** Send context to the provider, stream the response, execute
  tool calls, loop until the model stops requesting tools, yield final text.
  A configurable max-iterations guard bounds runaway loops (§10).
- **Tool registry.** The built-in `bash` tool with a JSON-schema parameter
  definition and an execute function.
- **Provider client.** Streaming HTTP to the model API behind one internal
  interface.
- **Renderer.** Sole writer to the terminal; append-only (§5).
- **Store.** SQLite load/append (§10).

The binary runs on a single `tokio` runtime. There is no input thread or line
editor — stdin is read once, fully, as the prompt.

### Binary CLI surface

The binary is invoked one of two ways: as a **turn** (default, reads a prompt on
stdin) or as a **subcommand** (management, no prompt). The surface is small:

- `mu [--session <id>] [--model <id>]` — run one turn; prompt read from stdin.
  This is what the zsh plugin invokes.
- `mu init zsh` — print the zsh plugin for `eval`.
- `mu session new` — create a session, print its id (used by `mu-new`).
- `mu session list` — list recent sessions (used by `mu-sessions`).
- `mu compact --session <id>` — force compaction (used by `mu-compact`).

The `mu-new`/`mu-attach`/`mu-sessions`/`mu-compact` shell functions (§6.8) are
thin wrappers over these subcommands. No other CLI surface exists in V1.

Subcommands (`init`, `session new/list`, `compact`) do **not** require a
configured provider; only the turn invocation does (§7).

### Turn lifecycle (authoritative end-to-end flow)

This is the exact sequence the binary follows for one turn invocation
(`mu [--session <id>] [--model <id>]`). Implement it in this order:

1. **Parse args**, read the entire prompt from stdin into a string.
2. **Load config** (§9). If the provider's required fields are missing, print an
   error to stderr and exit non-zero (§7). Resolve the effective model:
   `--model` if given, else the session's stored `model`, else the config
   default.
3. **Open the SQLite DB** (create + run migrations if absent).
4. **Resolve the session:**
   - If `--session <id>` is given and the row exists → use it.
   - If `--session <id>` is given and the row does **not** exist → print an error
     to stderr, exit non-zero (do *not* silently create it).
   - If `--session` is absent → create a new session row, and write its id to the
     runtime file named by `$MU_SESSION_FILE` (§10).
5. **Acquire the session `flock`** (§10). If held, print "session busy", exit
   non-zero.
6. **Build the context message list** from the DB: the latest compaction summary
   message (if any) followed by all messages after it, in order.
7. **Pre-turn compaction check** (§10, Tier 1): if the session's stored
   `last_total_tokens` (or bytes÷4 on the first turn) exceeds the configured
   fraction of the model context window, run compaction now, then rebuild the
   context list.
8. **Append the new user message** to the DB and to the context list.
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
      `risk:"read-only"` calls concurrently once renderer interleaving and
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
messages (§10). No tool call begins without first persisting its parent
assistant message. Tool results are persisted in request order immediately
after a sequential call or concurrent batch completes, so an uninterrupted
turn produces API-valid history.

---

## 4. Tools

The model-visible tool surface is exactly:

```ts
bash({
  title: string,
  risk: "read-only" | "reversible" | "destructive",
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
`risk:"read-only"` calls concurrently because per-call bash processes are
isolated.

**Terminal visibility.** `bash` prints `$ <title>`, streams combined output, and
finishes with exit status/duration. Every tool error is visible. TTY output uses
OpenCode-inspired color and glyphs; redirected output uses ANSI-free ASCII
equivalents.

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

The renderer is the sole writer to stdout/stderr and enforces append-only
output. It may style output only when stdout is a TTY. It never moves the
cursor, rewrites a committed line, uses an alternate screen, or emits transient
status lines. Redirected stdout is always ANSI-free.

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

- **Tool presentation.** Bash prints `$ <title>`, streams ANSI-sanitized output,
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

No spinner, no transient lines, no cursor movement, and no re-rendering — ever.

---

## 6. The zsh plugin

The plugin is the entire interactive surface. It is a single `.zsh` file,
sourced last in `.zshrc`, that adds agent mode to ZLE. It must not disturb the
user's existing configuration.

### 6.1 Invocation pattern (how a turn runs)

Submitting a prompt does **not** run `mu` from inside a ZLE widget (which would
make signal and terminal ownership awkward for a long, streaming turn). Instead
the agent-mode `accept-line` widget stashes the typed prompt, replaces the
buffer with a call to a small helper function, and accepts the line. zsh then
runs that helper as an ordinary **foreground command**:

```zsh
# illustrative only
_mu_send() {            # runs as a normal command, owns the TTY
  mu --session "$MU_SESSION_ID" <<< "$_MU_PROMPT"
}
```

Consequences, all desirable:

- `mu` owns the terminal for the turn; streaming output works directly.
- Ctrl-C sends SIGINT directly to the `mu` process — no routing to build.
- After `mu` exits, a `precmd` hook (registered via `add-zsh-hook`, see §6.6)
  re-arms agent mode: restores the agent prompt and keymap so the user is back
  at an agent prompt, ready for the next turn.

### 6.2 Entry and exit

- **Entry (explicit, default):** a dedicated, configurable key binding enters
  agent mode. This is the default path and has no conflict with typing `mu` as
  a command.
- **Entry (magic space, opt-in):** when enabled, Space is bound to
  `_mu_magic_space`; if the buffer is exactly `mu` it clears the buffer and
  enters agent mode, otherwise it self-inserts. Off by default (see §2.3 for the
  `mu <args>` conflict and the leading-space workaround).
- **Exit (backspace):** in the `mu` keymap, Backspace on an empty buffer
  (`$#BUFFER == 0`) exits agent mode; otherwise it deletes normally.
- **Exit (Esc):** also exits agent mode, as a discoverable escape hatch.

Entering agent mode sets an `_MU_AGENT_MODE` flag, swaps `PROMPT`/`RPROMPT` to a
distinct agent prompt, and switches to the `mu` keymap (§6.4). Exiting clears
the flag and restores the saved prompt and the `main` keymap. The `_MU_AGENT_MODE`
flag is what the `precmd` re-arm hook (§6.1) checks to decide whether to return
to an agent prompt after a turn — without it, `precmd` could not tell an
agent-mode turn from a normal shell command.

Note on Ctrl-C stickiness: interrupting a turn with Ctrl-C lands the user back
in agent mode (the flag persists), matching the Julia REPL. This is intentional
— an aborted turn keeps you where you were.

### 6.3 Multi-line prompts

Agent prompts are frequently multi-line, so the plugin supports it natively
(zsh handles multi-line buffers):

- **Ctrl-J** inserts a literal newline (reliable across all terminals).
- **Shift-Enter** inserts a newline **where the terminal reports it distinctly**
  (e.g. terminals supporting the CSI-u / Kitty keyboard protocol such as foot,
  kitty, recent alacritty). Where Shift-Enter is indistinguishable from Enter,
  it submits — Ctrl-J remains the guaranteed multi-line key.
- **Enter** submits the whole buffer.

### 6.4 Keymap isolation

To avoid touching the user's bindings, the plugin clones the active keymap into
a dedicated `mu` keymap once at load (`bindkey -N mu main`) and overrides only:
`accept-line` (submit), `^?`/Backspace (exit-on-empty), Esc (exit), Ctrl-J and
Shift-Enter (newline). All other keys — including the user's custom bindings —
are inherited. Bindings that make no sense for a prose prompt (e.g. an inherited
`Esc Esc → sudo-command-line`) are unbound in the `mu` keymap only.

Note (vi mode): if the user runs `bindkey -v`, insert-mode bindings live in
`viins`, not `main`. The plugin clones from whichever keymap is the user's
insert keymap (detect via `$KEYMAP`/`bindkey -lL main`), so vi users keep their
insert bindings; document this and default to `main` when detection is
ambiguous.

### 6.5 Disabling shell features in agent mode

Syntax highlighting and autosuggestions are meant for shell syntax, not prose,
so the plugin disables them on entry and restores on exit using each plugin's
documented off-switch — no patching of third-party internals:

- **zsh-syntax-highlighting:** save and clear `ZSH_HIGHLIGHT_HIGHLIGHTERS`
  (empty array = no-op), restore on exit.
- **zsh-autosuggestions:** call its `autosuggest-disable` widget on entry and
  `autosuggest-enable` on exit.
- **General fallback:** any stray `line-pre-redraw` hook without an off-switch
  can be detached with `add-zle-hook-widget -d line-pre-redraw <name>` on entry
  and re-added on exit.

Tab completion is simply not invoked in agent mode (Tab is left to self-insert
or unbound in the `mu` keymap), so the completion system is never engaged for
prose.

### 6.6 Coexistence with the user's `.zshrc`

The plugin is written to layer cleanly on existing setups (validated against the
target `.zshrc`):

- **Hooks via arrays.** It registers `precmd`/`preexec` work with
  `add-zsh-hook`, never by redefining `precmd`/`preexec`, so a user-defined
  `precmd` function continues to run alongside it.
- **Load order.** Source the mu plugin **after** zsh-autosuggestions and
  zsh-syntax-highlighting so its widget wrappers and feature-toggles sit on top.
- **Keymap clone.** Because the `mu` keymap is cloned from `main` at load and
  only selectively overridden, the user's global `bindkey`s are preserved both
  in normal mode and (except the deliberate overrides) in agent mode.
- **No global option changes.** The plugin does not flip `setopt`s that affect
  normal-mode behavior.

### 6.7 Shell history and context (V1 scope)

- **Shell history:** the raw prompt text is pushed into zsh history via
  `print -s`, so it appears in normal shell history and is recallable with
  Up-arrow inside agent mode. The internal `_mu_send` wrapper invocation is kept
  *out* of history (the widget builds it with a leading space under
  `HIST_IGNORE_SPACE`, or omits it from `accept-line` history recording), so
  each turn produces exactly one history entry: the prompt itself.
- **Full structured history:** `mu` independently records the complete prompt,
  assistant responses, and tool calls in SQLite (§10) — the authoritative
  transcript.
- **No shell-command sharing (V1):** commands you run in *normal* shell mode are
  **not** automatically fed to the agent. Bridging interactive shell activity
  into the session (e.g. via `preexec`/`precmd` capture) is deferred; V1 keeps
  the boundary explicit and private.

### 6.8 Session management commands

Because there is no in-`mu` REPL, session lifecycle is exposed as plugin-provided
shell functions (thin wrappers over `mu` subcommands), operating on the
`MU_SESSION_ID` shell variable (§10):

- `mu-new` — start a fresh session (clear `MU_SESSION_ID`; next turn creates one).
- `mu-attach <id>` — bind this shell to an existing session.
- `mu-sessions` — list recent sessions to pick from.

### 6.9 Distribution

Shipped as a `.zsh` file embedded in the binary and printed by `mu init zsh`,
installed with a single line in `.zshrc` (the zoxide/starship convention):

```zsh
eval "$(mu init zsh)"
```

This versions the plugin with the binary and keeps install to one line.

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
holding the API key, and a default model id. API keys are read from environment
variables; `mu` does not store secrets in its database.

**No provider, hard fail.** If no provider is configured (no base URL, or the
key env var is unset), a *turn* invocation exits immediately with a non-zero
status and a clear message pointing at `config.jsonc` and the expected env var.
(Management subcommands like `mu init zsh` still work without a provider.) There
is no silent fallback and no built-in default endpoint.

Because the canonical message history is stored in a provider-neutral form
(§10), swapping the base URL/model across turns is supported without migration;
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
are files discovered on startup (and cached, §12).

---

## 9. Configuration

A flat config directory (resolved via `$MU_CONFIG_DIR`, else `$XDG_CONFIG_HOME/mu`,
else `~/.config/mu`):

```
~/.config/mu/
  config.jsonc      # provider base_url + key env var + model; optional tuning
  AGENTS.md         # global agent instructions, appended to system prompt
  skills/
    <skill-name>/
      SKILL.md      # front-matter: name, description; body: instructions
      ...           # optional supporting files
```

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
    "agent_mode_key": "\\eM",                    // optional, default Alt-M; zsh keybinding
    "magic_space": false,                        // optional, default false
    "compaction": { "fraction": 0.75, "keep_recent_turns": 2 },  // optional
    "limits": { "max_iterations": 50, "max_lines": 2000, "max_bytes": 51200, "max_line_bytes": 10240 }
  }
  ```

  Only `provider.*` and `default_model` are required; everything else has the
  defaults shown. `mu` hard-fails on a turn if the required fields are missing
  or the API-key env var is unset (§7). `mu init` can write a starter file.
- **AGENTS.md** — global system-prompt addendum. A project-local `AGENTS.md` in
  the process's current working directory (the cwd `mu` was launched in), if
  present, is appended after the global one. Both are included; only the cwd is
  checked, not parent directories, in V1.

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
   cwd: /home/user/project
   os: linux
   date: 2026-06-18
   </env>
   ```
3. The `<available_skills>` block (§8), or omitted if there are no skills.
4. The global `AGENTS.md` contents, if the file exists.
5. The project-local `AGENTS.md` contents, if present (appended *after* global —
   both are included; "project overrides global" means later text wins by
   convention, not that the global is dropped).

Tool definitions are **not** part of this prompt; they go in the API `tools`
parameter (§7). Frontier models need little scaffolding, so the fixed parts (1–2)
stay terse and `AGENTS.md` carries user customization.

---

## 10. State and persistence

A single SQLite database (under `$XDG_STATE_HOME/mu` or `~/.local/state/mu`,
separate from config so config stays hand-editable and state stays disposable).
SQLite is chosen for zero-setup embedded storage, transactional durability, fast
open, and easy querying. **WAL mode** is enabled so per-turn load/append is fast
and concurrent shells don't block each other.

Conceptual schema (flat and small):

- **session** — `id`, `created_at`, `updated_at`, `cwd`, `model`, `title`,
  `last_total_tokens` (the most recent `usage.total_tokens` reported by the
  provider; used for the pre-turn overflow check, §"Context window and
  compaction"), `cost_total` (accumulated USD, for the turn summary). `model` is
  set at session creation from the effective model (lifecycle step 2); a later
  `--model` overrides for that turn only and does **not** rewrite the stored
  value. `title` is set lazily from the first user prompt (first ~60 chars) and
  is display-only for `mu session list`.
- **message** — `id`, `session_id`, `role` (`user`/`assistant`/`tool`/`summary`),
  `content`, `created_at`, ordering index, and for tool results `tool_call_id`.
  Provider-neutral representation. A `summary` row is a compaction summary
  (§"Context window and compaction"); the context builder starts from the latest
  `summary` row and includes everything after it.
- **tool_call** — `id`, `message_id`, `tool`, `args` (JSON), `output`, `status`,
  timings. Records the agent's tool invocations for inspection and the renderer's
  truncation pointers. (Tool *results* fed back to the model are stored as
  `tool` messages; this table is the structured audit copy.)

### Session ↔ shell mapping

V1 maps **one shell process to one session**, tracked by the `MU_SESSION_ID`
shell variable owned by the plugin:

- **Lazy creation.** On the first agent turn in a shell, `MU_SESSION_ID` is
  unset; the plugin invokes `mu` without `--session`, exporting `MU_SESSION_FILE`
  (a definitive env var name) pointing at a fresh path under
  `$XDG_RUNTIME_DIR/mu/`. `mu` creates the session row and writes the new id to
  that file. Immediately after `mu` exits, the `_mu_send` helper reads the file
  and exports `MU_SESSION_ID` from it. Subsequent turns pass
  `--session "$MU_SESSION_ID"`. The id is never printed to stdout, so the
  transcript stays clean.
- **Rotate / attach.** `mu-new` clears the variable and runtime file (next turn
  starts fresh). `mu-attach <id>` sets the variable to an existing session.
  `mu-sessions` lists candidates.
- **Per-turn lifecycle.** Each turn: open DB → acquire session lock → load
  session messages → run turn (persisting each completed message as it lands) →
  release lock → exit. The connection opens lazily so a turn that errors early
  stays cheap.

Sessions are append-only logs; resuming replays messages into the context
window. Multiple shells holding *different* sessions run concurrently (safe under
WAL).

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

`mu-attach` allows two shells to point at the *same* session, so concurrent
turns against one session are possible and must be serialized. Each turn acquires
a per-session lock for its duration; a second `mu` targeting the same session
finds it held and **fails fast** with a "session busy" message rather than
interleaving writes.

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

**Manual compaction.** `mu-compact` (a shell function wrapping the `mu compact`
subcommand) forces compaction on demand.

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

---

## 11. Safety posture

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

---

## 12. Startup-speed plan

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

## 13. High-level implementation phases

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
5. **zsh plugin.** `mu init zsh`, agent-mode keymap, magic-space + explicit entry,
   backspace/Esc exit, multi-line keys, feature disabling, `precmd` re-arm,
   prompt history via `print -s`, session functions (`mu-new`/`mu-attach`/
   `mu-sessions`/`mu-compact`).
6. **Skills.** Skill scan + cache + system-prompt injection, `AGENTS.md`
   (global + project).
7. **Polish.** Robustness, error-message quality, config surface.
