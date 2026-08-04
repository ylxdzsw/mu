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
  terminal UI, or project manager. The zsh and Fish plugins and shell scripts
  coordinate turns; they do not host a separate agent loop.
- **Non-magical.** No TUI. The shell owns the terminal and line editing; `mu`
  just reads a prompt and appends output. Output streams as it is produced (a
  tool line may appear before its output), but once a line is printed it is never
  rewritten or erased.
- **Minimal.** One model-visible function tool: `bash`, with a small Mu-owned
  command suite available inside it. A flat config directory. A
  per-session journal for state in the active scope. The core binary itself has no
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
  process that replays/appends session state from JSONL.
- **No plugin SDK, MCP, or in-process subagent orchestrator.** Extensibility is
  via skills (markdown) and `bash` (call any CLI tool, including another `mu`
  process when independent delegation is useful).
- **No core shell emulation.** The core `mu` binary does not ship shell behavior,
  raw terminal editing, completion, or prompt rendering. The zsh and Fish
  plugins are thin shell surfaces that own their native line editors and call
  `mu` for each turn.
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
- One physical binary to install and update. Private `apply_patch`, `edit`, and
  `view_image` symlinks dispatch back into it by `argv[0]`.
- Mature ecosystem for everything needed: async runtime (`tokio`), HTTP/SSE
  (`reqwest`), JSONC/serde, and Unix file locking.
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

- The zsh and Fish plugins are the preferred interactive surfaces. Each owns its
  shell's line editing, prompt mode, and keybindings, then submits each entered
  prompt by spawning `mu` for one foreground turn.

This single-binary shape is the central decision (see §3 for the full rationale
recap). It keeps the agent semantics small and scriptable while leaving the
shell responsible for interaction.

The default Cargo build is native and uses `native-tls`: system OpenSSL on
Linux and Apple Security on macOS. For an executable at
`<prefix>/bin/mu`, built-ins are always `<prefix>/share/mu/` and applets are
always `<prefix>/libexec/mu/`. Native startup derives those paths without
checking, creating, or modifying package-owned resources.

The single additive `portable` feature enables `reqwest/native-tls-vendored`
and compile-time `include_str!` entries for every
shipped built-in. On OpenSSL platforms this replaces the system OpenSSL linkage
with a vendored build; on macOS native TLS continues to use Apple Security.
Portable resolution treats built-ins and applets independently. If the
executable is under `bin/` and the corresponding native directory exists, that
directory wins. Otherwise the resource uses a fixed directory under one
selected cache root:

- absolute `$XDG_CACHE_HOME/mu` when `XDG_CACHE_HOME` is set;
- `$HOME/Library/Caches/mu` on macOS;
- `$HOME/.cache/mu` on other Unix systems.

A relative `XDG_CACHE_HOME`, missing usable home, conflicting object, or any
creation/population error is fatal; `/tmp` is never a fallback. The fixed
resource directories are `<cache-root>/builtins` and
`<cache-root>/applets`. On first creation Mu writes the embedded built-in
strings directly into the former and absolute symlinks to the current
executable into the latter. Existing directories are authoritative regardless
of their contents. Mu does not validate, refresh, repair, roll back, clean up,
or atomically stage them; failed creation may therefore leave a partial
directory that subsequent runs trust. Moving or upgrading the binary does not
refresh cached paths. The user must remove the applicable resource directory
to regenerate it. Applet `argv[0]` dispatch occurs before portable
initialization.

Version-tag artifacts add portable Linux x86-64 musl and macOS ARM64/Intel
archives with SHA-256 checksums. The archives omit external built-ins because
they are embedded. Linux statically links musl and vendored OpenSSL
with no dynamic library dependency; macOS retains only Apple system-library
linkage. The existing Windows MSYS2 UCRT64 package and release archive remain
unchanged and are published alongside them.

### 2.3 Interactive mode lives in shell surfaces

The zsh and Fish plugins are the built-in interactive surfaces. They own only
line collection, prompt mode, keybindings, and session continuity; every
non-empty submitted line still runs as a fresh foreground `mu` process.

Consequences:

- `mu` remains scriptable and stateless on exit.
- Shell plugins never duplicate provider, tool, store, or agent-loop semantics.
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
and makes skills "just files". Native built-ins live in
`<prefix>/share/mu/`; portable builds use that directory when installed and
otherwise materialize their embedded built-ins in the user cache. They have the
lowest precedence; shipped built-ins may include self-customization guidance
such as `customize-mu` or delegation guidance such as `subagent`, but user and
project instructions can shadow them by name.
Skills may declare optional `requires_env` and `requires_commands` frontmatter
keys. Each key is a comma-separated list, and every listed requirement must be
met before the skill is injected.

### 2.6 Flat config, per-session append-only journals

All user-facing configuration and instruction files live under a flat `.mu`
directory (`config.jsonc`, `AGENTS.md`, prompt/skill/command files). Runtime
state is one JSONL journal per session plus content-addressed objects in the
active global/project scope. See §9, §10, and §11.

---

## 3. Architecture overview

`mu` has one executable and small zsh and Fish integrations around it. The CLI
turn runner remains the core unit.

```
   ┌──────────────────────────── shell surfaces ────────────────────────────────┐
   │  shell scripts: `mu [opts]` with PROMPT on stdin                          │
   │  zsh/Fish plugins: prompt mode; each entry spawns one `mu` turn           │
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
   │                    store (JSONL journals in active global/project scope) │
   └───────────────────────────────────────────────────────────────────────────┘
```

### Why this split (recap)

The hard part of "replace bash" is shell fidelity. Having the real shell own the
terminal gives that for free and forever; a core binary that owns a long-lived
REPL would have to reimplement completion, job control, and PTY handling
behavior indefinitely. The cost is (a) session state must persist across process
invocations — handled by append-only journals in the active scope, §11 — and (b)
interactive shell commands are not automatically visible to the agent (§6.3).
Shell integration is preferred because it is only a line-editing surface around
repeated turn invocations, not a replacement runtime.

### Binary module responsibilities

- **Entry.** Resolve project/config/session scope, parse args (`--session`,
  `--continue`, `--attach`, `--output`, subcommands), read the prompt from
  stdin, run one turn, persisting each completed message as it lands (§11), exit.
- **Agent loop.** Send context to the provider, stream the response, execute
  tool calls, loop until the model stops requesting tools, yield final text.
  A configurable max-iterations guard bounds runaway loops (§11).
- **Tool registry.** The built-in `bash` tool with a JSON-schema parameter
  definition and an execute function.
- **Provider client.** Streaming HTTP to the model API behind one internal
  interface.
- **Renderer.** Sole writer to output; apply the selected output density and
  automatically detected interactivity (§5).
- **Store.** JSONL replay/append in either project-local or global scope (§11).

The binary runs on a single `tokio` runtime. There is no input thread or line
editor. Bare `mu` reads stdin once as the prompt; file-backed turns read
non-terminal stdin once as an optional custom instruction.

### Binary CLI surface

The core binary is invoked one of two ways: as a **turn** (default, reads a
prompt on stdin) or as a **subcommand** (management; manual compaction alone
accepts optional non-terminal stdin as a custom focus). The surface is small:

- `mu [-s <id>] [-c] [-m|--model <id>] [-a <file>] [-o|--output final|concise|detail|full]`
  — run one turn; prompt read from stdin. `-a/--attach` is repeatable and accepts
  supported image or audio files.
- `mu [-s <id>] [-c] [-m|--model <id>] [-a <file>] [-o|--output final|concise|detail|full] <prompt-file>`
  — run one turn from a prompt file; if the first line starts with `#!`, drop
  it before sending the prompt. A `mu` shebang may contain exactly
  `-m|--model <id>` as a turn-local default. Non-terminal stdin is appended as a
  custom instruction. `-a/--attach` is repeatable.
- `mu [-s <id>] [-c] [-m|--model <id>] [-o|--output final|concise|detail|full] <custom-command>`
  — run a discovered shebang command from the active project/global `.mu`
  instruction index. Command names are relative `.mu` paths including
  extensions; built-in subcommands and explicit prompt paths win.
- `mu.zsh` — zsh prompt mode; each accepted prompt runs one foreground `mu`
  turn and keeps using the same session. `MU_ZSH_SESSION_ID=<id>` seeds
  attachment to an existing session.
- `mu.fish` — Fish 4 prompt mode with the same turn/session contract.
  `MU_FISH_SESSION_ID=<id>` seeds attachment to an existing session.
- `mu project inspect --path <dir>` — report whether a directory resolves to a
  project scope, and which marker (`.mu` or `.git`) was found.
- `mu project init [--path <dir>] [--force]` — create minimal `.mu/` project
  metadata in the current directory by default, or in an explicitly chosen
  directory.
- `mu status --json [--include-git] [--include-session-details]
  [--include-models] [--include-commands] [--include-skills]` —
  machine-readable shell state for prompt rendering and completion. The default
  projection omits Git and detailed session metadata because prompt rendering
  does not consume them; the corresponding `--include-*` flags add them.
  `context_tokens` is the latest provider-reported context size when
  `context_usage_source` is `reported`, or the bytes÷4 projection when it is
  `estimated` before a first turn or immediately after compaction. Consumers
  derive a percentage from `context_tokens / context_window`. The resolved
  model object includes its current effective `replay_key`; `--include-models`
  adds the effective key for every configured model.
- `mu context [--export]` — introspect the agent context. By default it prints
  the assembled system prompt mu itself would use: the role preamble, the
  `<runtime>` block, the full skills index (built-in, global, and project), and
  the merged `AGENTS.md` — a faithful mirror of the persisted system message, so
  it never contacts a provider. `--export` instead prints a portable projection
  for a *foreign* agent to ingest: an explanatory preamble (noting the content
  was authored for mu, and pointing at the `customize-mu` reference when that
  built-in is present, while listing the absolute paths of existing global and
  active-project `.env` files and warning that they may contain API keys or
  other secrets; the preamble lists paths as comment-safe JSON strings and
  explains Mu's restricted shell-compatible `.env` syntax and global-to-project
  precedence so a foreign agent can parse and load them when a skill needs their
  values, without displaying the files or exposing secret values in output)
  followed by the user's own merged `AGENTS.md` (each wrapped with its scope and
  absolute source path) and non-built-in skills; the role preamble, `<runtime>`
  block, and built-in skills are omitted. In `--export` mode, when the user has no
  `AGENTS.md`, non-built-in skills, or `.env` files, the output is empty (exit
  0), so a `SessionStart`-style hook injects nothing in a project with no mu
  configuration. Neither mode loads
  a provider; scope resolves from the working directory like other introspection
  commands. See the README for a Claude Code hook example.
- `mu cat [<prompt-file-or-command>]` — resolve and load the same user-prompt
  text as a turn without contacting a provider or creating session state. With
  no target, stdin is the complete prompt. With a file-backed target, terminal
  stdin is left unread and non-terminal stdin is appended verbatim after
  `\n---\n\n`. Interactive stdout shows a resolved-source line and rendered
  Markdown; redirected stdout contains only the exact composed prompt text.
- `mu session new` — create a model-free session and print its id. `--model` is
  rejected; model selection belongs to an actual turn. Creation does not update
  `current-session`.
- `mu session list` — list recent sessions.
- `mu session transcript [--session <id>]` — print a persisted session
  transcript, defaulting to the last selected session in the active scope.
- `mu compact --session <id>` — force compaction. Terminal stdin is not read;
  non-terminal stdin is an optional verbatim custom focus instruction.
- `mu retry [-s <id>] [-c] [-m|--model <id>] [-o|--output final|concise|detail|full]`
  — resume an interrupted (unclean) turn: normalize the tail and continue the
  agent loop with no new prompt. `--model` overrides the latest attempted model
  and `--output` overrides the merged config default for the retry. No-op on a
  clean session.

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
`cat` is therefore also reserved as a top-level subcommand name.
`mu session list`, `mu session transcript`, and project inspection/init do
**not** require a configured provider. `mu session new` neither resolves nor
stores a model and also does not require a configured provider. Turn invocation
and `mu compact` require a configured provider because they can contact the
provider (§7).

### Turn lifecycle (authoritative end-to-end flow)

This is the exact sequence the binary follows for one turn invocation:

1. **Parse args**, resolve the active scope from the invoking `pwd` (§9), read
   the resolved prompt source (stdin, prompt file, or custom command), and load
   any repeatable `-a/--attach` image or audio files. Each attachment must be at
   most 20 MiB and must be PNG, JPEG, WebP, GIF, WAV, or MP3 content matching
   its filename extension.
2. **Load config** (§9): global first, then project config over it when a
   project is active.
3. **Open the active-scope journal store:** project-local when inside a
   project, global otherwise. Ensure `sessions/` and `objects/` exist.
4. **Resolve the session:**
   - If `--session <id>` is given and its journal exists in the active scope → use
     it.
   - If `--session <id>` is given and its journal does **not** exist in the active
     scope → print an error to stderr, exit non-zero (do *not* silently create
     it or fall back to a global session).
   - If `-c/--continue` is given → follow `current-session`, or create a session
     if the pointer is absent or broken.
   - Otherwise create `sessions/<id>.jsonl` atomically, retrying a fresh short
     random ID on collision, then sync its meta and system-prompt records.
   Resolve the model choice from an explicit `--model`, else the attached
   session's latest agent/compaction provider request, else the old
   `current-session` target's latest choice, else the first configured model.
   An attached session restores its own floating provider position. A new
   session inherits only the choice and starts at candidate zero.
5. **Acquire session ownership** (§11) with nonblocking exclusive `flock` on
   the journal. If it is already held, print `session busy` and exit non-zero.
6. **Normalize any interrupted tail, then build the context list.** If the
   previous turn left an unmatched provider request or a Bash claim without a
   result, append an interruption marker or conservative result. Then project
   semantic context from the system prompt, latest compaction, turn prompts,
   accepted assistant projections, and Bash results.
7. **Append `turn_started`** with the submitted prompt, cwd, and current Git
   worktree root. Persist attachment bytes in `objects/` before their journal
   references. Only while still holding the journal lock, update
   `current-session` by atomic symlink replacement after `turn_started` is
   durable.
   (`mu retry` skips this step, restores the original turn cwd, and resumes the
   same turn.)
8. **Pre-turn compaction check** (§11): use the latest still-current reported
   usage, otherwise estimate the projected context. If it exceeds the
   configured fraction, compact and rebuild.
9. **Agent loop** — repeat until the model returns `finish_reason: "stop"` or the
   max-iterations cap is hit:
   a. Persist and sync a reconstructible `provider_requested` recipe, then send
      the context list + tool definitions to the provider (streaming).
   b. Accumulate the streamed assistant message (text deltas and tool-call
      deltas; see §7 for the delta-accumulation rules).
   c. On failure, append `provider_failed`; partial content remains audit-only.
   d. On success, persist assembled native response JSON in `objects/`, then
      append one `provider_completed` event containing usage, the accepted
      semantic assistant projection, and every Bash claim. Reject a
      response naming any function other than `bash` before persisting it;
      malformed Bash argument JSON is still a valid claim and later receives an
      error result. Accumulated tool calls are accepted as executable claims
      only when the provider finish reason is `tool_calls`; calls present on a
      length, filtering, or other terminal reason remain audit-only.
   e. If `finish_reason` is `tool_calls`: split the calls into maximal
      contiguous batches of eligible readonly work. All output densities may
      execute contiguous `risk:"readonly"` `bash` calls concurrently, but
      **persist `bash_completed` results** (serialized back to providers as
      `role: "tool"`, with their provider `tool_call_id`) in the model's
      original call order before looping back to (a). Any non-readonly call,
      unknown tool, or call that requires guardrail review is a sequential
      barrier.
   f. If `finish_reason` is `stop`: the loop ends.
10. **For `detail` and `full`, print the turn summary line** to stderr (§5),
    release the journal lock, and exit 0. `concise` and
    `final` omit it.

**Usage accounting.** Each provider response in the loop carries its own `usage`.
For the **context fullness** figure use only the latest still-current accepted
agent response's `total_tokens` — because
`prompt_tokens` already includes the entire prior context, the last response's
`total_tokens` is the true current context size (summing across iterations would
double-count). For the `in`/`out` token display, sum `prompt_tokens` and
`completion_tokens` across all iterations of the turn. Subtract provider-reported
cache reads and writes from `in`, and display those cache figures separately.
Any later semantic event invalidates that exact figure. Until another normal
provider response supplies an exact total, status uses the bytes÷4 projection
and marks the value as estimated.

**Interruption.** Step 9d persists only *after* a message is fully formed. If
SIGINT / a dropped connection / a provider error occurs mid-stream, the partial
assistant remains outside semantic history, although a partial native object may
be retained for audit (§11).
No tool call begins without first persisting its parent assistant message, and a
result is persisted for every call that begins execution. Once tool execution
has started, interrupts fan out to every active tool process group, stop
launching new tools, drain partial output, and still persist tool results in
request order, so Ctrl-C stops work without making already-produced tool output
disappear. Nothing else is written on interruption: the process just exits. Any
dangling claim left by the interruption is repaired by step 6 of the *next*
invocation.

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

`bash` prepends the resolved applet directory to its post-login `PATH`: always
`<prefix>/libexec/mu/` in a native build; in a portable build, that installed
directory when present or the cached `applets/` directory otherwise. Before
normal CLI parsing or portable initialization, `mu` checks the basename of
`argv[0]` and dispatches these applets:

- **`apply_patch`** accepts one patch argument or reads it from stdin. Its
  `*** Begin Patch` / `*** End Patch` format supports add, update, move, and
  delete operations with context hunks. Relative paths resolve from the shell
  call's working directory; absolute paths are used as written. It preflights
  the whole patch, rejects conflicting operations and existing add/move
  destinations, then applies validated file changes. Updating through a
  symlink edits its regular-file target while preserving the link; deleting a
  symlink removes only the link; moving a symlink renames the link. Dangling
  links can therefore be deleted or moved but cannot be updated.
- **`edit [--relaxed] [--all] FILE`** reads one or more replacement blocks from
  stdin. Each block has marker-only framing lines and this shape:
  ```text
  <<<<<<< SEARCH
  exact existing text
  =======
  replacement text
  >>>>>>> REPLACE
  ```
  The line endings adjacent to the three marker lines are framing, not body
  content; an additional empty body line represents a leading or trailing line
  ending. Internal body line endings are preserved literally.
  An empty SEARCH section is invalid; an empty REPLACE section deletes the
  matched text. Matching is byte-exact by default. When an exact SEARCH has no
  matches, strict mode probes the relaxed tiers without writing: line-ending
  equivalence, ignored trailing line whitespace, then ignored leading and
  trailing line whitespace. A unique relaxed candidate produces an actionable
  error suggesting either exact text or `--relaxed`; ambiguous candidates ask
  for more context. `--relaxed` applies the first tier that finds candidates.
  It does not collapse or ignore internal whitespace, and it converts REPLACE
  line endings to the dominant style around each matched range so an LF edit
  does not introduce mixed endings into a CRLF file.

  Without `--all`, every SEARCH must occur exactly once at the selected tier.
  With `--all`, every occurrence at that tier is replaced. All matches are
  calculated against the original UTF-8 file snapshot, and overlapping matches
  are rejected. Relative paths resolve from the shell call's working directory;
  absolute paths are used as written.
  Updating through a symlink edits its regular-file target while preserving
  the link and its target's permissions. The entire document is preflighted
  before the target is opened; Mu then takes a non-blocking advisory lock on
  the existing inode and revalidates the original snapshot before writing.
  Mu syncs a transient random sibling backup, overwrites and syncs the same
  inode, and removes the backup after success. Ordinary write failures attempt
  to restore the original bytes into that inode; a crash or failed restoration
  may leave the backup for manual recovery. This preserves inode-bound
  metadata and hard links but cannot give atomic visibility to readers that
  ignore the advisory lock. New-file creation still publishes a fully synced
  sibling with a no-clobber operation. Success is reported compactly as
  `Done!`, an `M PATH` line, and an applied block/replacement count; failures
  identify the responsible block and corrective action. The updated file or a
  full diff is not returned.
- **`view_image [--detail auto|low|high|original] PATH`** loads a validated PNG,
  JPEG, WebP, or GIF through the same attachment loader and 20 MiB limit used by
  `mu -a`. `--detail` is optional and defaults to `auto`. The command writes a
  text summary normally and, when invoked inside a live Mu tool call, stages
  immutable bytes plus one entry in that session's private attachment manifest.
  It fails outside a live tool call or after eight entries for that claim.

These are ordinary commands called through `bash`, not additional model-visible
function tools. Mu passes only the private manifest path, object directory, and
internal Bash-call id to the child. `view_image` locks the manifest across its
limit check and append, snapshots the source bytes, makes the SHA-256 object
durable, then appends and syncs one manifest entry. Mu verifies staged objects
once before committing `bash_completed`; staging without a durable result is
ignored during recovery.
Responses adapters serialize images in the native `function_call_output`;
Anthropic Messages adapters serialize them inside the native `tool_result`;
Chat Completions adapters retain the tool text and add a labeled multimodal
user-message projection on the wire only.

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

**Detailed visibility.** In `detail` output, while a tool call has started but
its title has not begun streaming, interactive output shows a mutable
`[preparing toolcall]` indicator. The indicator is cleared when `bash` begins
committing its `# <title>` line, followed by a `$ <command>` line. In interactive
output, `#` shares the title styling and `$` shares the command's risk color;
redirected output instead includes an explicit `[risk]` label. If the call
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
  stating how much was elided, and the full output is best-effort written to an
  exclusive, randomly named file in the private flat `$TMPDIR/mu` runtime
  directory. The marker points the model at that ephemeral path so it can
  inspect the result with another `bash` call while it exists. Byte limiting is
  applied backward from the actual tail so the final exit-status line is
  retained; nothing is lost, it just is not forced into context.
- The spill is **best-effort**: the command has already run by the time its
  output is clamped, so a failed spill write (unavailable temporary directory,
  permission error, or disk full) must never fail the tool result. The preview
  is returned with a note that the full output could not be saved. Mu does not
  actively remove spill files, and it makes no retention promise; the OS or an
  administrator may remove one at any time. A missing spill is likewise
  harmless — the model just gets a shell error when it tries to read the path.

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

`mu` supports four output densities: `final`, `concise`, `detail`, and `full`.
They are different renderings of the same agent turn and must not imply
different agent behavior. The effective density is an explicit `--output`, then
the merged `config.jsonc` `output` value, then `detail`.
The removed `plain` and `terminal` values are rejected rather than retained as
aliases.

- **Final output** is for supervisor agents invoking `mu` as a subagent. It
  does not stream. On success, stdout is exactly the final raw assistant message
  content from the completed turn, written once after the turn finishes and
  without an added newline. Tool output, intermediate assistant tool-call
  messages, reasoning/progress, automatic retry notices, summaries, and bells
  are suppressed. Automatic retries and per-completed-message persistence still
  behave the same as in human-facing modes. On fatal failure after retry
  exhaustion or any other unrecovered error, stdout is `error: <message>`
  followed by one newline and the process exits non-zero.
- **Concise output** keeps assistant text and ordinary notices but reduces every
  bash call to one `=> <title> · exit <code>` line. Calls that fail without a
  numeric exit code end in `· error`. Command, cwd, stdin, output, duration, and
  successful guardrail detail are suppressed. Reasoning progress is ephemeral
  and never becomes committed transcript output.
- **Detail output** is the normal human transcript. It preserves the existing
  thought line, streamed/capped tool header, output preview, and exit line.
- **Full output** streams all provider-visible reasoning trace or reasoning
  summary without a thought indicator, and shows the complete command, stdin,
  and redacted tool output. Model-facing tool-result truncation remains in force.

Interactivity is independent from density. Whether stdout is interactive is
always detected from its TTY status and is not configurable. Stdout TTY
detection enables ANSI styling, parsed terminal Markdown, and the single mutable
live line in every non-final density. Redirected stdout is sequential and
ANSI-free. `final` ignores interactivity. Interactive rendering keeps normal
scrollback and never uses an alternate screen, clears the screen, or requires
mouse interaction.

The top-level `line_wrapping` setting controls only interactive presentation and
defaults to `true`. When enabled, the renderer samples the stdout terminal width
once at turn startup, reserves the final terminal column, and uses 80 visible
cells if width detection fails. It wraps assistant prose and bounds rendered
tables to that ruler. Compact renderer-owned rows such as tool titles, command
previews, and mutable live lines are ellipsized to the same ruler. Mu does not
handle `SIGWINCH`, reflow committed output, or rewrite scrollback after a
terminal resize.

When `line_wrapping` is `false`, assistant prose is left for the terminal to
hard-wrap and table cells are not split into continuation rows. Renderer-owned
single-row presentation is still ellipsized to a fixed 80 visible cells.
Neither value changes redirected output, `final` output, persisted messages, or
model-visible content.

**Concurrency contract.** All output modes may run contiguous readonly
`bash` calls concurrently. Interactive output keeps append-only scrollback and
the one-live-line rule: at most one bash call owns live terminal presentation at
a time, even while later readonly calls are already running in the background.
Redirected output follows the same ordered display without live-line redraws.
`final` suppresses the transcript while preserving execution, ordering, and
persistence semantics.

The renderer is the sole writer to stdout/stderr and independently enforces the
selected density and detected interactivity.

Assistant Markdown is parsed on TTYs. The renderer commits only output whose
terminal representation is stable: ordinary prose streams as soon as it is not
being held for an inline span, while headings, quotes, and list items stream once
their line prefix is unambiguous. With `line_wrapping` enabled, the renderer
retains at most five visible cells so an approaching ruler can replace the most
recent retained whitespace with a newline. If no such whitespace exists, it
hard-wraps at a Unicode grapheme boundary. ANSI and hyperlink controls consume
zero cells; combining sequences, emoji ZWJ sequences, and other extended
graphemes are never split. CJK text therefore falls back to grapheme-boundary
wrapping. This is intentionally not full Unicode line breaking, dictionary
segmentation, or hyphenation.

A heading prefix waits for the space after the full opening `#` run, so `##` is
not rendered as h2 until it cannot still become h3. Closing heading hashes are
not special-cased and are rendered literally.
Inline links, inline code, emphasis, strong text, and double-tilde
strikethrough wait for the current span to complete; fenced code starts terminal
code styling at the opening fence, streams code lines without printing fence
markers, and resets styling at the closing fence or response boundary. Emphasis
uses regular cyan, while strong text uses bold. Block quotes use gray italics
without a visible gutter marker. Underscores within words remain literal, so
identifiers such as `CAP_SYS_ADMIN` are not interpreted as emphasis. Markdown
tables are buffered until the table is complete enough to align and commit once,
so columns never require rewriting prior output. With wrapping enabled, table
layout counts every border and padding cell, caps any one content column at 80
cells, and shrinks the widest columns until the complete grid fits the sampled
terminal ruler. Cells wrap with the same five-cell whitespace heuristic as
prose. If a grid cannot give every column three content cells, the table becomes
a stacked header/value grid; terminals too narrow even for that use a linear
header/value layout. Every emitted table row stays within the ruler unless one
indivisible grapheme is itself wider than the terminal. With wrapping disabled,
columns use their natural widths and cells remain on one renderer row.

While a confirmed table is buffered, interactive output shows a mutable
`[table ~N tokens]` live indicator; the completed table clears and overwrites
that indicator instead of committing a final table-status line. Markdown
features outside this supported terminal subset are emitted as raw Markdown
rather than partially rendered. When stdout is piped or redirected, assistant
deltas pass through byte-for-byte as the model produced them, preserving raw
Markdown for downstream consumers.

### 5.1 TTY block-spacing contract

Interactive output is structured as a sequence of top-level transcript blocks:
the shell's `mu>` prompt, assistant text, committed thought lines, bash tool blocks,
notices, and similar human-facing sections. Spacing has exactly one owner at
each boundary: the active shell plugin owns the transition from a submitted
`mu>` prompt to the child process's first visible block, and the renderer owns
subsequent renderer-to-renderer block transitions.

- Top-level transcript blocks are separated by exactly one empty line.
- After submission, the canonical normalized prefix is
  `mu> prompt\n\n[first visible block]`. Neither a missing empty line
  (`mu> prompt\n[first visible block]`) nor two empty lines
  (`mu> prompt\n\n\n[first visible block]`) are valid.
- The renderer never adds leading spacing before its first visible block. That
  block may be a live thought indicator, assistant text, a tool call, or a
  notice. In styled TTY output, provider-emitted whitespace before it is
  boundary noise: it does not render and does not mark a block as committed.
  Blank lines inside visible assistant content remain intact, and redirected
  output continues to preserve raw assistant deltas.
- The *next* top-level block owns that separator. Committed block formatters
  should end with exactly one newline; they must not rely on trailing blank
  lines baked into their own text.
- Starting a tool-call block first finishes the current assistant Markdown
  stream, including any cells retained for wrapping, before reserving or
  committing tool-call presentation.
- Live status lines such as the updating `[thought ...]` line or the
  `[preparing toolcall]` indicator may reserve the top separator on first
  render, but subsequent ticks only redraw that one mutable trailing line. A
  first live status line does not add spacing on behalf of a preceding shell
  prompt.
- A bash tool block includes its header, streamed preview/output, omission
  marker, and final exit line; those pieces are not separated from each other by
  extra blank lines.
- In `detail` and `full`, the turn summary is its own final transcript block.
  When a turn produced transcript output, it has exactly one empty line before
  the summary and one empty line between the summary and the next shell prompt.
  Concise omits the summary but keeps one empty line before the next shell
  prompt.

This contract applies to `concise`, `detail`, and `full` output.

**Stream routing (explicit).** The conversation transcript goes to **stdout**:
tool presentation, tool failures, Bash output, and assistant text. Fatal process
errors and the `detail`/`full` turn summary go to **stderr**. Thus
`mu <<< prompt > out.txt`
captures the complete portable transcript while fatal diagnostics/summary
remain visible. Stdout TTY detection selects rich versus portable rendering;
stderr TTY detection suppresses the summary when redirected.

- **Detail tool presentation.** Interactive output shows `[preparing toolcall]`
  as its one mutable live line before title bytes arrive; redirected output
  omits it. Bash then streams `# <title>` and `$ <command>` in order. Interactive
  output colors the command by risk, while redirected output includes an
  explicit label such as `[readonly]`. The command is capped to its first
  decoded line, optional stdin is summarized as `< [stdin N bytes]`, command
  output uses the ordered head/omission/tail preview, and completion prints the
  matching exit line. Headers already streamed are not duplicated at execution.
- **Concise tool presentation.** Interactive output replaces
  `[preparing toolcall]` once both title and risk are complete with a risk-colored
  live `=> <title>`, except that a call awaiting guardrail review transitions
  directly from `[preparing toolcall]` to `[guardrail] <title>…`. Completion
  clears it and commits exactly
  `=> <title> · exit <code>` or `=> <title> · error`, without duration.
  Redirected output emits only the completed ANSI-free line and does not add a
  risk label. Risk colors are cyan for readonly, yellow for reversible, red for
  destructive, and dim for missing/unknown risk. Command, cwd, stdin, output,
  duration, and successful guardrail detail are suppressed. Consecutive calls
  form one block with no empty lines between them.
- **Full tool presentation.** Full retains the detailed title, risk, cwd, and
  exit/error presentation, but `$ ` contains the complete decoded multiline
  command, `< ` contains complete stdin instead of a byte summary, and every
  redacted/sanitized output byte is displayed without a screen preview or
  omission marker. Model-context truncation and spill files from §4 are
  unchanged.
- **Tool ordering.** Multiple calls are displayed in provider order. In
  `detail` and `full`, concurrent readonly batches still present one active bash
  stream at a time; later calls may already run in the background. Concise
  buffers each outcome and commits its one-line record in the same order.
- **Assistant text.** Redirected output streams raw Markdown deltas unchanged.
  Interactive output commits parsed Markdown as soon as the relevant unit is
  stable: prose streams token-by-token unless an inline span is open,
  list/heading/quote content streams after the prefix is stable, tables wait for
  completion, and unsupported Markdown stays raw.
- **Reasoning progress.** Detail creates the interactive mutable thought line.
  Chat reasoning uses `[thought <duration>, <tokens> tokens]`; opaque Responses
  or Anthropic reasoning uses `[thought <duration>]` with an optional
  conservative title from the first bold-only or ATX-heading summary line.
  Detail commits that line when
  reasoning finishes, including when no exposed reasoning text exists. Concise
  uses the same interactive indicator but erases it at completion and is silent
  when redirected. A Responses title received while reasoning continues appears
  on the next periodic refresh, remains for that reasoning item, and does not
  reset its timer. A title received only as reasoning completes is not briefly
  flashed or committed. Immediately after a concise tool the indicator occupies
  the next line without an empty separator, and ephemeral reasoning does not
  break a consecutive tool block. Full streams Chat reasoning deltas and every
  exposed Responses or Anthropic summary part directly in provider order,
  without a live or committed thought indicator; providers exposing neither
  produce no reasoning output.
- **Errors.** Always printed and clearly prefixed, with TTY styling when
  available. Fatal turn failure produces a non-zero process exit code so the
  shell's `$?` is meaningful.

**Turn summary line.** When `mu` exits normally (turn complete) in `detail` or
`full`, it prints a single structured summary line to stderr:

```
[mu] tokens: 1234 in (567 cache read, 89 cache write) / 456 out  context: 12%
```

All figures come from the provider's reported `usage` for the turn: `in` is
`prompt_tokens` excluding cache reads and writes, `out` is `completion_tokens`,
and cache usage is shown parenthetically when reported. Cache write is omitted
when the provider does not report it; `context` is the new
`total_tokens` ÷ model context window. This is the *only* stderr output in the normal case. It appears after all
stdout, and goes to stderr so it stays out of a captured stdout transcript. It
is suppressed in `concise` and `final`, and when stderr is not a TTY
(piped/redirected), since it would pollute log files. In `detail` and `full` it
is followed by one blank line so the next shell prompt is visually separated
from the completed turn.

Redirected stdout avoids terminal-only control sequences so every density
remains suitable for scripts. Interactive output may show progress for
in-flight work, but committed transcript content is never erased from
scrollback. When `terminal_bell.enabled` is true, interactive non-final output
also emits a BEL (`\a`) after successful turn completion once total turn
duration meets `terminal_bell.min_duration_ms` (default 10s). In `detail` and
`full`, the summary is written before the bell.

---

## 6. zsh and Fish shell surfaces

The zsh and Fish plugins expose the same shell-native interaction contract.
Each behaves like a shell editing mode: Tab with the cursor at the beginning of
the line toggles the current prompt into or out of `mu>` mode while preserving
the current buffer. Enter submits the current buffer as one `mu` turn when it
contains non-whitespace text and otherwise just draws a fresh `mu>` prompt;
Ctrl-C cancels the `mu>` draft but leaves the cancelled line in scrollback;
Backspace remains an ordinary delete key; and Ctrl-D keeps normal shell EOF
behavior even while `mu>` mode is active. Up and Down first move within the
current multiline buffer, then browse tagged Mu submissions from shell history
while skipping ordinary shell commands. Mu history is not project-scoped;
recalled input runs against the current shell-managed session, model, and
attachments. A plugin must not duplicate agent-loop, provider, store, or tool
semantics.

The zsh plugin requires zsh, `jq`, and the `mu` binary on `PATH`. Setting
`MU_ZSH_BIN` to a specific executable overrides the binary name/path used by
the plugin.

### 6.1 Invocation pattern

Submitting a non-empty prompt runs `mu` as an ordinary foreground child process.
When needed, the plugin first creates a session with the management command;
every turn, including the first, receives `--session`. The plugin forwards an
explicit shell output override when configured, writes the prompt to the child
process's stdin, waits for the turn to finish, and then redraws `mu>` with the
same session id.
`MU_ZSH_OUTPUT` or `MU_FISH_OUTPUT` optionally overrides the density; when
unset, the child inherits the active `config.jsonc` default. It does not control
whether the child is interactive.
The prompt omits the context field while no session is attached and the next
turn will create one. Once a session exists, it shows the rounded context
percentage, including `0%` for a short session.
The status line always shows the invoking `pwd`. When the active project root
is not literally the same path, it also shows that project root in parentheses;
this keeps a repository or worktree checkout visible while working in one of
its subdirectories. In global scope it shows `(global)` instead.
Each plugin keeps one in-memory tracked bundle containing its scope, optional
session id, optional sticky model override, and staged attachments. Merely
changing directory masks a bundle owned by another scope; returning without a
Mu action restores it. Prompt rendering plus status, command-discovery, and
completion lookups are passive observations and do not invalidate it. An
accepted prompt or slash action activates its current scope, atomically
discarding a bundle owned by another scope before the action runs. Malformed or
unknown slash input, an unsupported model, and an unreadable attachment are
rejected before activation; a later runtime failure does not restore discarded
state. Within the same scope, `/new` clears only the session id and preserves the
model override and attachments.
After the native line editor commits the submitted prompt line to scrollback,
the plugin prints one empty line before child-process output starts, independent
of whether the child uses `concise`, `detail`, or `full` output.

Consequences:

- `mu` owns the terminal while each turn is running; streaming output works
  directly.
- Ctrl-C while editing in `mu>` mode cancels the current draft, leaves that
  prompt line visible in scrollback, and redraws `mu>` like a shell prompt
  interrupt. Ctrl-C while a foreground `mu` turn is running uses ordinary Unix
  signal behavior for the foreground process.
- After each turn exits, the shell returns to `mu>` mode with the same session
  id.

### 6.2 Entry and exit

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
  multiline buffer. At its first or last line, respectively, browse only
  version-tagged Mu submissions in shell history, skipping ordinary commands.
  The first Up preserves the live draft and cursor; Down past the newest Mu
  entry restores them. History recall is unscoped and does not change the
  current session, model, or pending attachments.
- Shift+Enter inserts a newline without submitting when the terminal sends the
  CSI-u sequence `Esc [ 13 ; 2 u`. Terminals that send ordinary Enter for this
  key combination cannot be distinguished by the shell and require a matching
  key configuration.
- Typing `/` at the start of a `mu>` line proactively lists slash commands.
  After that, Tab performs shell-native candidate matching and listing.
- A buffer beginning with `/` is a slash command. Known custom commands take
  everything after their name as a custom instruction, including inserted
  newlines; `/compact` accepts the same instruction syntax as a custom focus.
  Unknown names report a slash-command error. Other built-in slash commands keep
  their own argument rules.
- `/attach <file>` resolves and stages one readable regular file in shell
  memory for the next user message and may be repeated. It creates no session
  message itself. `/attach` lists pending files and `/attach --clear` discards
  them. Attachments belong to the tracked scope, and the prompt shows the count
  only while that scope is active. Empty Enter, draft cancellation,
  mode changes, `/model`, `/new`, `/retry`, and `/compact` do not consume the
  queue; the next ordinary prompt or custom command passes every staged file as
  a repeatable `-a` argument and clears the queue before launching `mu`.
- `/model <model>` validates and stores a shell-only sticky override in the
  tracked bundle. It is forwarded as `--model` to later turns and `/retry`; it
  does not mutate persisted session state.
- Completing a model that supports effort levels leaves the bare model in the
  buffer and immediately opens the shell's effort candidates. Completion does
  not append `:` or choose an effort until the user continues through the
  shell's native completion UI.
- Ctrl-D is the normal terminal EOT key (`^D`). xterm-style and browser-terminal
  input paths forward it as input when the browser or OS has
  not intercepted the key before the terminal receives it.

### 6.2.1 zsh-specific integration

- Source `mu.zsh` from `.zshrc`.
- Tab completion delegates matching, candidate lists, and menu selection to the
  user's normal zsh completion settings.
- While `mu>` mode is active, conflicting line-editor plugins should be
  suspended. Common ZLE helpers such as syntax highlighting and autosuggestions
  may be disabled automatically; additional plugin toggles may be attached with
  mode enter/exit hooks. The arrays `MU_ZSH_ENTER_HOOKS` and
  `MU_ZSH_EXIT_HOOKS` contain zsh function names; enter hooks run after prompt
  mode is active, and exit hooks run after the normal shell prompt is restored.

### 6.2.2 Fish-specific integration

- Source `mu.fish` near the end of `config.fish`. A package may install it as
  `vendor_conf.d/mu.fish`; sourcing it again after user prompt/key
  configuration is supported.
- Fish 4, `jq`, and `mu` on `PATH` are required. `MU_FISH_BIN` overrides the
  executable and `MU_FISH_OUTPUT` overrides output density. On an older Fish,
  the plugin reports the version requirement and does not install its
  integration.
- The plugin copies and wraps the active `fish_prompt`, `fish_right_prompt`, and
  `fish_mode_prompt`. Normal shell mode continues to call those saved
  functions with the prior command status intact; Mu mode replaces them with
  its status and `mu>` prompt.
- Mu editing uses a dedicated `mumode` initialized from Fish's complete default
  editing bindings. On exit, the prior `$fish_bind_mode` is restored. The
  arrays `MU_FISH_ENTER_HOOKS` and `MU_FISH_EXIT_HOOKS` provide additional
  function hooks.
- In normal shell mode, Tab at cursor zero remains the Mu-mode toggle. Away from
  cursor zero, it delegates to the binding that was active when `mu.fish` was
  last sourced, separately for Fish's `default` and `insert` modes.
- Slash/model completion uses Mu's status candidates and Fish filename
  completion. Multiple candidates are listed with Fish repaint semantics;
  completion does not promise zsh `zstyle` behavior.
- Each accepted prompt is added to shell history as a version-tagged entry whose
  trailing `printf ... | mu ...` command remains directly replayable. Slash
  commands use the same tag so prompt-mode history can recall them.
- `MU_FISH_SESSION_ID=<id>` seeds an existing session. Session, model,
  attachment, prompt color, hook, executable, and output state otherwise use
  `MU_FISH_*` variables corresponding to the zsh variables.

### 6.3 Context boundaries

- **Full structured history:** `mu` records prompts, assistant responses, and
  tool calls in the session journal (§11). Tool output is stored with the shared
  truncation/spill policy, so the journal keeps the structured transcript and an
  ephemeral temporary file may hold oversized raw command output.
- **No shell-command sharing:** commands run outside `mu` or the shell
  plugin are
  not automatically fed to the agent. `mu` keeps the boundary explicit and
  private.

### 6.4 Session management

Session lifecycle is exposed through CLI commands:

- A shell plugin without a session explicitly runs `mu session new` before its
  first submitted prompt, then passes any shell model override to the first
  actual turn and reuses that session for later prompts in the same shell.
- Exporting the shell-specific `MU_ZSH_SESSION_ID=<id>` or
  `MU_FISH_SESSION_ID=<id>` before entering `mu>` attaches the plugin to an
  existing session.
- `mu -c` continues the last selected session in the active scope for a
  one-shot turn.
- `mu session new` creates a session and prints its id.
- `mu session list` lists recent sessions.
- `mu compact --session <id>` compacts a session on demand.

---

## 7. Provider / model integration

Mu supports exactly three hand-written HTTP/SSE protocols: OpenAI-compatible
Chat Completions, OpenAI Responses, and Anthropic Messages. Each configured
provider has a required complete `endpoint`. After URL parsing and optional
trailing-slash normalization, a case-sensitive path ending in
`/chat/completions` selects Chat Completions, `/responses` selects Responses,
and `/messages` selects Anthropic Messages. Query parameters are preserved but
do not affect classification. Every other path fails during configuration
loading; Mu never infers a protocol from a hostname, provider id, or model
name. A gateway exposing multiple protocols is represented by one provider
entry per endpoint.

All adapters accept the semantic transcript and Mu's `bash` function schema,
stream protocol-neutral text/reasoning/tool-call events, and return a semantic
assistant result plus usage. The renderer, tool executor, guardrail, retries,
and compaction remain protocol-neutral.

**Chat Completions.** Mu posts directly to the configured endpoint with
`messages`, the Chat function wrapper, `stream:true`, and
`stream_options:{include_usage:true}`. It accumulates indexed
`delta.tool_calls`, assistant text, and optional `reasoning_content`. A resolved
effort is sent as top-level `reasoning_effort`. Complete reasoning attached to
an assistant tool-call response is persisted and replayed verbatim when the
current Chat Completions model has the same effective `replay_key` as its
origin. This supports explicitly compatible DeepSeek thinking tool loops across
provider fallback and model switches without model-name heuristics.

**Responses.** Mu posts directly to the configured endpoint with `stream:true`,
`store:false`, `include:["reasoning.encrypted_content"]`, locally reconstructed
`input`, and a flat Responses function-tool definition. It never sends
`previous_response_id` or a conversation identifier. Every request opts into
reasoning summaries with `reasoning:{summary:"auto"}` and adds `effort` to that
object when one is resolved. Providers that reject the summary option fail the
request normally; Mu does not retry without it. Typed SSE events provide
reasoning-item boundaries, optional reasoning-summary text, output text, and
function-call argument deltas. Mu accumulates complete output items from
`response.output_item.added` and `response.output_item.done`, then merges them
by `output_index` with the terminal response snapshot. Terminal fields win
when both forms provide a field, while stream-only fields such as
`encrypted_content` are retained. This assembled successful `response.output`
array is stored in the native response object and replayed as input when the
current Responses model has the same effective `replay_key` as its origin.
Semantic tool results become `function_call_output` items connected by
`call_id`.

**Anthropic Messages.** Mu posts directly to the configured endpoint using
`x-api-key` and `anthropic-version:2023-06-01`, with `stream:true`,
`max_tokens:64000`, adaptive thinking displayed as summaries, and automatic
five-minute prompt caching. A resolved effort is sent as
`output_config:{effort}`. The leading semantic system message becomes the
top-level `system`; user text and images become content blocks; assistant Bash
claims become `tool_use`; and consecutive Bash results become `tool_result`
blocks in one user message. Image detail is intentionally omitted because
Messages has no equivalent field. Audio is rejected while assembling the
provider request, before network I/O.

Anthropic text, thinking summaries, signatures, citations, tool input, usage,
and stop reasons are accumulated from indexed SSE content-block events.
Complete successful assistant content arrays are stored unchanged, including
`thinking`, `redacted_thinking`, signatures, text, citations, and `tool_use`,
and replayed when the current Anthropic Messages model has the same effective
`replay_key` as its origin. The adapter assumes current adaptive-thinking
models; it has no manual thinking-budget mode, old-model compatibility matrix,
or model-name heuristics.

The semantic transcript remains authoritative for display, compaction, and
cross-model continuation. Native replay requires both the same API and equal
effective replay keys. A model's optional configured `replay_key` is resolved
from the latest effective config for every request; omission means the literal
`provider/model`, excluding effort. Changing config therefore changes how all
retained history is interpreted without rewriting the session. Request recipes
record the replay origins actually included, rather than keys, so historical
request reconstruction remains exact. Switching protocols keeps semantic
messages and reconstructs function calls/results, but omits incompatible native
payload variants. Compaction excludes native state before the active summary
boundary and retains it with the recent semantic suffix.

Text and images are supported by all adapters. Images serialize as Chat
`image_url`, Responses `input_image`, or Anthropic `image` blocks. Existing
audio inputs serialize as Chat `input_audio`; Responses and Anthropic endpoints
reject audio locally with a clear error. Only successfully completed streams
produce replay state, so retries never depend on a partial or remote response
chain.

**Model context window.** The 75% threshold needs the model's max context size.
Source it from `config.jsonc`: each configured model entry carries a
`context_window` integer. mu does not fetch model cards. If a model has no
configured `context_window`, the threshold-based tiers (Tier 1 pre-turn and Tier
2 in-loop) are skipped for it and the Tier 3 API-error fallback is the only
guard.

Model and provider selection come from `config.jsonc`: a complete `endpoint`, optional
env var holding the API key, and ordered provider/model definitions. If the
global config file is missing, `mu` creates a starter `~/.mu/config.jsonc`
automatically before loading configuration. The starter's first provider is a
keyless OpenCode Zen free model (`api_key_env: ""`), so a freshly built `mu`
runs a turn with no additional setup; it also ships a commented keyed provider
example.

`provider/model[:effort]` is fixed. A bare `model[:effort]` expands to every
configured provider containing that model in literal merged config order.
Fallback is forward-only and per-session. Provider-request history derives one
remembered position per floating model id across agent, compaction, and
guardrail calls. Effort is request metadata and does not reset the provider;
switching models and returning resumes that model's position, while fixed
references neither update nor erase floating positions. A new session starts
each floating model at candidate zero. If a remembered provider disappears,
that history entry is ignored and the next older valid position for the model
is used; if none exists, the rebuilt chain starts at candidate zero. If the
model has no candidates, resolution fails. Status and provider origins render
floating choices as `(provider)/model[:effort]`.

Without an explicit override, model selection follows the attached session's
latest eligible choice, then the `current-session` target's choice without its
floating position, then the first configured fixed model. API keys are read
from environment variables and are never persisted.

**No provider, hard fail.** If no provider is configured, a provider has no
valid supported endpoint, or a non-empty configured key env var is unset, a *turn* invocation
exits immediately with a non-zero status and a clear message pointing at
`config.jsonc`. `mu compact` follows the same rule because it calls the
provider. Valid runtime provider-availability failures may use a floating
choice's next candidate; deterministic configuration errors never do.

Because semantic message history is stored separately from API-specific native
replay (§11), swapping endpoint/model across turns is supported.

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
- Before responding, the model actively scans the listed skills. When the user
  names a skill or one is even partially relevant, the model reads the full
  file via `bash`, using the **absolute path** from the injected block. Loading
  is context acquisition only; it does not require the model to follow the
  skill or any instruction in it. Relative paths written inside a skill file
  resolve against that file's containing directory.

The same file may also be a custom command when its first line is a permissive
`mu` shebang. The shebang may contain no arguments or exactly
`-m|--model <model[:effort]|provider/model[:effort]>`; all other arguments are rejected when the
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

Standard linked Git worktrees share the primary checkout's project scope. If
the discovered `.git` marker is a worktree pointer file and there is no closer
`.mu`, Mu resolves its `commondir`; when that is the primary checkout's `.git`
directory, the primary checkout is the project root. Project configuration,
instructions, sessions, and runtime state therefore come from the primary
checkout's `.mu`, while the invoking `pwd` and Git branch/dirty state remain
tied to the linked checkout. The stable prompt names the Mu project root; each
turn records and exposes its active Git worktree root and working directory.

A `.mu` directory always wins over Git discovery, including at a linked
worktree root; this provides an explicit independent project scope. Bare
repositories, separate Git directories, malformed pointers, and other layouts
without a standard primary `<project>/.git` fall back to the linked checkout as
their project root without invoking Git to resolve it.

The shell tool's working directory defaults to the process working directory,
not the project root.

The project-local directory is `.mu`. It may contain:

- `config.jsonc`, optional project configuration.
- `.env`, optional local environment values.
- `AGENTS.md`, the project-local agent instructions.
- optional instruction files that may be plain references, custom commands,
  skills, or both.
- `sessions/`, containing one append-only `<session-id>.jsonl` journal per
  session.
- `objects/`, containing immutable SHA-256-addressed attachments, native
  provider JSON, toolsets, and large semantic content.
- `current-session`, a relative symlink to the last selected journal.
- `.gitignore`, which ignores those runtime paths.

Applet attachment staging and oversized-output spills use private paths under
`$TMPDIR/mu`; they are ephemeral and have no retention promise.

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
  sessions/         # one append-only JSONL journal per session
  objects/          # immutable content-addressed bytes
  current-session   # last selected session
```

When a project is active, global configuration is loaded first and project
configuration is merged over it. Project values take precedence. Parent project
configuration is not merged because nested projects are not supported. When the
upwalk reaches home or root without finding a project, only global
configuration is used.

Optional `.env` files are loaded with the same scope precedence:
process environment first, then global `.env`, then active-project `.env`.
The resulting effective environment is used for provider API-key lookup and is
passed to every `bash` tool process. Each file is parsed completely before any
of its assignments are applied; duplicate assignments use the last value.

The `.env` format is a restricted, source-compatible subset of shell assignment
syntax. It is parsed as data and never executed:

```text
LINE       := BLANK | COMMENT | ASSIGNMENT
ASSIGNMENT := ("export" [ \t]+)? NAME "=" VALUE
NAME       := [A-Za-z_][A-Za-z0-9_]*
VALUE      := BARE | SINGLE_QUOTED | DOUBLE_QUOTED
BARE       := [A-Za-z0-9_./:@%+,=-]*
```

Blank lines may contain spaces or tabs. Comments are full lines whose first
non-whitespace character is `#`. Assignments cannot be indented and cannot have
whitespace around `=`, trailing whitespace, inline comments, or trailing
tokens. Bare values cover common tokens and paths; other values must be quoted.
Single quotes preserve their contents literally and have no escape syntax.
Double quotes support only `\"`, `\\`, `\$`, and ``\` ``; unescaped `$` and
backticks are rejected. Quoting forms cannot be concatenated. Expansion,
multiline values, line continuation, tilde expansion, globbing, shell operators,
and ANSI-C quoting are unsupported. Invalid UTF-8, NUL, lone carriage returns,
and all unsupported syntax are errors. LF and CRLF line endings are accepted,
and the final line need not end in a newline.

Every accepted assignment produces the same string value when the file is
sourced by Bash or Zsh. Mu treats `export` as an accepted, source-friendly prefix
but otherwise ignores it because every loaded value is passed to child
processes. A shell reader can use `set -a` while sourcing to export assignments
that omit the prefix.

Configuration and session storage are related but separate concepts. Config is
merged across scopes; sessions live in exactly one scope under the discovered
project's `.mu/sessions/` or global `~/.mu/sessions/`. Sessions from one scope
are not visible in another.

- **config.jsonc** — JSON with comments and trailing commas. Concrete shape
  (field names are normative):

  ```jsonc
  {
    "output": "detail",                         // optional default density
    "line_wrapping": true,                      // interactive presentation only
    "providers": {
      "openai": {
        "endpoint": "https://api.openai.com/v1/responses", // required complete POST URL
        "api_key_env": "OPENAI_API_KEY",         // optional: env var NAME, not the key
        "models": {
          "gpt-5.6-terra": {
            "context_window": 1050000,           // needed for Tier-1 compaction & context%
            // Optional ordered suggestions for status output and shell completion.
            "supported_efforts": ["none", "low", "medium", "high", "xhigh", "max"],
            // Optional non-secret native-replay compatibility group.
            // Defaults to "openai/gpt-5.6-terra".
            "replay_key": "openai-gpt-5.6"
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
      "env": ["*_API_KEY", "*_API_TOKEN", "*_AUTH_TOKEN"] // optional; these are the defaults
    }
  }
  ```

  At least one provider and one model are required; everything else has the
  defaults shown. `output` accepts `final`, `concise`, `detail`, or `full`; an
  explicit CLI `--output` overrides it. `line_wrapping` is a boolean and has no
  CLI override. Provider and model order is meaningful:
  project config entries are listed before inherited global entries, and model
  suggestions and bare-model fallback candidates follow that order.
  `supported_efforts` contains arbitrary
  provider-defined strings and is advisory: it drives status output and shell
  completion but does not restrict manually entered effort suffixes.
  `replay_key` is an optional, non-empty, non-secret compatibility label for
  protocol-native replay. Models share native replay only when their APIs and
  current effective keys match; omission defaults to the literal
  `provider/model`. If global
  `config.jsonc` is missing, `mu` creates a starter file automatically. `mu`
  hard-fails on a turn if the required fields are missing or the API-key env var
  is unset (§7). Effective configuration is a recursive overlay of bundled
  defaults, global config, then project config. Bundled provider entries are
  starter examples only and are not inherited by an existing global config.
- **.env** — optional restricted shell-compatible assignment data. Values are
  visible to `bash`; this is
  convenience, not sandboxing. Values from provider `api_key_env` and
  `redaction.env` are exact-value redacted from bash output before the output is
  stored or shown to the model. Each `redaction.env` selector is either an exact
  environment-variable name or a leading `*` followed by a non-empty literal
  suffix, such as `*_TOKEN`. The suffix form matches all effective environment
  variable names ending in that suffix. Other wildcard placements, multiple or
  consecutive wildcards, and wildcard-only selectors are invalid. Matching is
  case-sensitive. The default selectors are `*_API_KEY`, `*_API_TOKEN`, and
  `*_AUTH_TOKEN`; an explicit empty list disables these defaults. Empty
  redaction values are ignored with a warning. Short
  redaction values are still redacted with a warning.
- **AGENTS.md** — system-prompt addendum. Global instructions are loaded first;
  active-project instructions are appended after them when a project is active.
  Each file is wrapped in an `<agents_md>` element whose `scope` is `global` or
  `project` and whose `path` is the absolute source path. Both are included;
  "project overrides global" means later text wins by convention, not that
  global instructions are dropped.

The system prompt is intentionally minimal. It is assembled once when a session
is created, persisted as the first message, and then loaded from session history
for later turns. Existing sessions do not rebuild it when files or config change.
The assembled prompt has this fixed order:

1. A short role/behavior preamble (a few sentences). Illustrative:
   > You are mu, a terminal agent. Exactly one function tool is available:
   > `bash`; do not call any other function tool. Inside `bash`, Mu provides
   > `apply_patch` for structured file edits, `edit` for exact replacements,
   > and `view_image` for loading an image into the tool result. These are shell
   > commands, not function tools.
   > Each bash call is isolated; pass `cwd` explicitly when needed. Keep
   > responses concise.
   The exact wording lives in `src/system_preamble.md`; keep it short.
2. A `<runtime>` block of host-stable facts only, as plain `key: value` lines:
   ```
   <runtime>
   os: linux (Ubuntu 24.04.2 LTS)
   date: 2026-06-18
   user: alice (uid 1000)
   mu project root: /work/project
   </runtime>
   ```
   On Linux, Mu appends the distribution's `PRETTY_NAME` from the standard
   `os-release` file when available, falling back to `NAME`, then `ID`.
   The Mu project root is included when project-scoped. Current working
   directory and active Git worktree root are turn-level facts; semantic replay
   derives the first location block and later change reminders from
   `turn_started` (§11, "Agent environment context").
3. The `<available_skills>` block (§8), or omitted if there are no skills. Skill
   metadata is merged from built-in, global, and active-project instruction
   indexes. Priority is project > global/user > built-in for same-name skills
   and commands.
4. The global `AGENTS.md`, wrapped in `<agents_md scope="global"
   path="/absolute/path/to/AGENTS.md">`, if the file exists.
5. The project-local `AGENTS.md`, wrapped in `<agents_md scope="project"
   path="/absolute/path/to/AGENTS.md">`, if a project is active and the file
   exists.

Tool definitions are **not** part of this prompt; they go in the API `tools`
parameter (§7). Frontier models need little scaffolding, so the fixed parts (1–2)
stay terse and `AGENTS.md` carries user customization.

---

## 11. State and persistence

State is stored in exactly one active scope: `<project>/.mu` when a project is
active, or `$MU_CONFIG_DIR` (normally `~/.mu`) otherwise. Each session is one
append-only `sessions/<session-id>.jsonl` journal. Immutable bytes live in the
flat `objects/<sha256>` store, and `current-session` is a relative symlink to
the last selected journal.

The first journal line is immutable metadata with format/version, scoped
session ID, and creation time. Later lines have contiguous sequence numbers,
timestamps, and a tagged event. Readers accept only the complete
newline-terminated prefix. The next writer truncates an incomplete final line;
malformed earlier data is corruption.

Conceptual event model:

- **`system_prompt`** stores the exact initial model-visible prompt. Its runtime
  block includes the stable Mu project root; there is no environment seed.
- **`turn_started`** creates one submitted turn and owns its prompt, cwd, and
  current Git worktree root. Retry reuses this turn and location.
- **`provider_requested`** is synced before contact and identifies the turn,
  purpose (`agent`, `compaction`, or `guardrail`), exchange, canonical model
  reference, provider/API/endpoint/wire model, effort, and a versioned request
  recipe. Recipes reference semantic context by sequence and exact toolsets by
  object hash; their checksum verifies reconstructed native request JSON.
- **`provider_completed`** stores one assembled native response object plus the
  semantic projection accepted at that time. Assistant projections contain
  text/reasoning/native replay and all immutable Bash claims; compaction
  projections contain summary and boundary; guardrail projections contain the
  parsed authorization decision. The stored projection is authoritative during
  normal loading and recovery; Mu never reruns a newer provider parser while
  replaying a journal.
- **`provider_failed`** and **`provider_interrupted`** terminate an exchange
  without adding semantic assistant history. A failure records a stable error
  class and may retain partial native JSON for audit.
- **`bash_completed`** is the unique result for a durable Bash claim, including
  outcome, output, exit code/duration where applicable, and ordered attachment
  references.

There is no session row, message table, run/attempt entity, mutable title,
updated timestamp, context-token cache, or owner PID. Session listing derives a
short first-prompt preview and activity time by scanning journals. Version 1 is
a fresh format: old SQLite files are neither inspected nor migrated.
If a future parser defect is recoverable from retained native data, correcting
the projection requires an explicit whole-session journal-format migration;
ordinary open and recovery never reinterpret it.

### Session mapping

`mu` maps each interactive shell instance to at most one active session:

- **First-turn creation.** When a shell plugin has no attached session, it first
  invokes `mu session new`, captures and validates the single id printed by that
  management command, and remembers it for the current scope. It then invokes
  the first turn with `--session <id>` and any shell-owned model override. There
  is no rendezvous file or inherited descriptor, and the id is never printed by
  the turn itself.
- **Attach / continue.** `MU_ZSH_SESSION_ID=<id>` and
  `MU_FISH_SESSION_ID=<id>` seed their respective plugins with an existing
  session, while `mu -s <id>` and `mu -c` handle one-shot re-entry from the
  command line. `mu session list` lists recent candidates.
- **Per-turn lifecycle.** Each turn: open and nonblockingly lock the selected
  journal → normalize its interrupted tail → replay semantic context → append
  the turn and provider/tool events as they become durable → unlock on exit.

Sessions are append-only logs; resuming replays events into the context window.
Multiple shells holding different session files run concurrently.

### Agent environment context

The stable system prompt names the Mu project root when project-scoped. Every
`turn_started` records the submitted cwd and current Git worktree root.
Context projection renders a full location block before the first retained
turn and an XML system reminder whenever either value changes between retained
turns. These reminders are derived model input, not stored messages, so
compaction cannot split a prompt from its location. Retry restores the original
turn cwd before provider or Bash work.

### Message-level persistence and interruption

Persistence is at domain-event granularity. A completed assistant projection
and every Bash claim it creates share one synced event before any claim
executes. Each Bash result is a later synced event. Partial/in-flight assistant
content may be retained only on a failed audit exchange and is never projected
into semantic history.

### Interrupted turns and retry

There is no stored turn-status flag. A latest turn is clean only when it has a
final accepted assistant projection and all of its Bash claims have results.
A `turn_started` with no provider exchange, a failed/unmatched request, or an
unresolved claim is dirty.

**Rationale — derive, don't store.** A separate boolean can drift out of sync
with the messages (precisely in the crash cases that matter) and would risk
"retrying" a turn that actually completed. The log is the single source of
truth, so cleanliness is read from it and cannot desync.

**Normalizing an interrupted tail.** Before any turn or retry runs, Mu truncates
an incomplete final line, appends `provider_interrupted` for each unmatched
request, and resolves every result-less Bash claim. A durable guardrail denial
gets a deterministic error result; every other claim gets a conservative
interrupted result. Calls that finished remain untouched. This is idempotent.

**Rationale — treat result-less calls uniformly.** We do **not** try to tell a
call that "never started" from one "started but killed": the window between
persisting a tool-call request and spawning the process is sub-millisecond, and
a write may have realized side effects. Assuming "maybe executed" and asking the
agent to verify is the safe, simple choice — it removes the need for any
per-call running marker.

**Recovery is not a special mode.** On the next invocation:

- A **new prompt** normalizes the tail, then appends on top and runs. This makes
  the common "Ctrl-C to redirect" flow work: after interrupting, the user can
  just type the next instruction; the agent sees the interrupted results and the
  new prompt and continues or redirects. No forced retry, no stuck session.
- **`mu retry`** normalizes the tail and re-runs the loop with *no* new prompt,
  so the model continues the interrupted turn. `--model` overrides the latest
  attempted model and `--output` overrides the merged config default for that
  retry; each shell plugin's `/retry` command forwards active shell overrides.
  Without an override, retry uses the session's latest requested canonical
  model. It restores the original submitted cwd and refuses on a clean session
  ("nothing to retry").

### Session concurrency ownership

Two processes targeting one session are serialized by
`flock(LOCK_EX | LOCK_NB)` on that journal itself. Contention fails immediately
with `session busy`; the descriptor remains open for the active operation, and
the kernel releases it on exit or crash. No lock file, PID, lease, or stale
owner recovery exists. Read-only commands may parse the locked journal's
complete prefix without taking the advisory lock. Different sessions use
different files and do not contend.

There is no special atomic create-and-lock API. A freshly created session is
not published through `current-session` before a turn owns its journal, and
standalone `session new` never selects it. Mu does not add coordination for the
vanishingly rare case where another process scans and explicitly targets that
otherwise unpublished ID between creation and the first lock.

### Context window and compaction

**Token counting (source of truth).** Mu does not run a tokenizer. Adapters map
native usage into input, output, total, cache-read, optional cache-write, and
reasoning-output fields on each provider completion. The latest accepted agent
completion's `total_tokens` is reported exactly only while no later semantic
event has changed the projected context; otherwise Mu estimates.

A `bytes ÷ 4` approximation (`approx_tokens(s) = ceil(len_bytes(s) / 4)`) is
used only where no API figure exists yet:
- before any still-current agent usage exists;
- estimating the size of **not-yet-sent** content (e.g. which messages to keep
  when building a compaction), where the provider has not yet returned a count.

Context management then uses a **three-tier strategy**, from most to least
graceful:

**Tier 1 — graceful pre-turn compaction (75% threshold).** At the start of each
turn, Mu compares current reported usage or the projected bytes÷4 estimate
against the model's context window. If it
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

**Tier 3 — one reactive compaction on API overflow.** If the provider returns a
context-length error and the current semantic context has not already been
compacted, mu compacts once and retries. If compaction cannot remove history,
the compaction request itself overflows, or the unchanged post-compaction
request still overflows, the turn aborts without provider fallback. New
assistant/tool content permits a later recovery cycle. Overflow is recognized
from an HTTP `413`, a structured error
`code`/`type` of `context_length_exceeded`, or a known overflow phrase in a 4xx
body (e.g. "prompt is too long", "maximum context length", "context window") —
message matching is gated to client errors so an unrelated 5xx body is not
misclassified.

**Compaction algorithm** (same in all tiers): summarize everything up to a
cut point into one compaction projection, keeping the most recent N
`turn_started` events (default 2, configurable) verbatim after it. Derived
location reminders do not consume the retention budget. The current dirty turn
is retained even when N is zero. If there is no older complete turn to
summarize, compaction is a reported no-op. The cut is always immediately before
a turn, so a prompt/location pair or tool claim/result pair is never split.

The summarizer uses a small compaction-specific system prompt rather than the
session's agent system prompt, so tool, skill, runtime, and service inventories
are not duplicated into the summary unless the user's work made them relevant.
The summarization *input* clamps each entry (tool results hardest) so a huge
history cannot make the summarize request itself overflow; the stored
transcript is untouched. The next context projection loads the latest summary
plus later semantic events, so compacted history is naturally excluded without
deleting anything. Earlier journal events remain available for audit. When a
prior summary exists, only semantic events after its boundary and before the
new cut are incorporated into the updated summary.

**Manual compaction.** `mu compact --session <id>` forces compaction on demand.
Like a prompt file or custom command, it leaves terminal stdin alone and reads
non-terminal stdin through EOF as an optional verbatim custom instruction. The
instruction gives relevant material more of the available detail and summary
budget, while the summarizer must still preserve every important fact needed to
continue correctly. In either shell prompt mode, `/compact <instruction>` pipes
the text after the command through this same stdin path. Automatic compaction
never supplies a custom focus. Interactive compaction shows one mutable
`[compacting <duration>]` line. Automatic compaction clears it before normal
turn output; manual compaction replaces it with one result line containing the
reported pre-compaction percentage, estimated post-compaction percentage, and
elapsed time. Redirected output emits one plain status line. Provider failure
or an empty summary exits non-zero and never reports success.

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
error. When enabled by the output density, the summary line is printed only on
exit `0`.

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
  the session journal provide the audit trail.
- **Interruptibility.** Because `mu` runs as a foreground job, Ctrl-C is the
  practical "stop" button: it stops launching new work, interrupts every active
  tool process group, drains visible output where possible, persists completed
  messages/tool results, and exits non-zero.
- **Secrets** are never persisted by `mu`; provider keys come from the
  environment or `config.jsonc`, never the session journal.
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

- **Allow** (`auth >= risk`): the bash call executes. Detail/full terminal
  output renders `✓ guardrail allowed · risk ≤ auth — reason` after the command
  header and before execution output.
- **Deny** (`auth < risk`): the bash call does not execute. Detail/full terminal
  output renders `✗ guardrail denied · risk > auth — reason`, and a tool error
  is returned to the agent:
  > guardrail: action rejected — risk_level X exceeds user_auth_level Y (reason).
  > Do not work around this; stop and ask the user to authorize, or choose a
  > less destructive approach.
  The agent can then adapt its approach or stop and ask the user.
- **Reviewer failure** (timeout, malformed JSON, network error after 3 retry
  attempts): an explicit Bash error records that execution never began and the
  turn is **aborted**. Re-authorizing would likely fail again since the reviewer
  itself is malfunctioning.

In interactive concise output, a call awaiting review changes directly from
`[preparing toolcall]` to `[guardrail] TITLE…` while review is in progress,
without briefly displaying the destructive tool line. An allow restores the
normal live tool line and adds no permanent review line; completion remains
`=> TITLE · exit N`. A denial or reviewer failure commits
`=> TITLE · guardrail denied` or `=> TITLE · guardrail error`. Redirected
concise output has no mutable live line and emits only the committed outcome.
Final-only output remains silent about guardrail activity.

**User authorization via history.** There is no dedicated "re-prompt" mechanism.
When the agent asks the user and the user responds with explicit approval
("yes, force push"), the user's message becomes part of the session history. On
the next turn, the reviewer sees this in the transcript and can score
`user_auth_level: "explicit"`, which permits even `critical`-risk actions.

**Denial limit.** The turn counts all guardrail denials and aborts with a clear
notice when `guardrail.max_denials_per_turn` is reached. The default is 3. This
prevents repeated destructive attempts without a second sliding-window policy;
the general iteration limit remains an independent bound.

**Retry.** Reviewer provider calls use the same availability policy as agent
calls: one initial request plus three transient retries per candidate, then
forward fallback for a bare review model. Parse failures retain a separate
three-attempt semantic budget. Context-length errors are not retried. A
floating guardrail request reads and advances the same per-session, per-model
position as agent and compaction requests; a fixed review model leaves every
floating position unchanged.

**Config.**

```jsonc
"guardrail": {
  "enabled": true,                           // default on; set false to opt out
  "review_model": null,                      // null -> same as active turn model
  "timeout_seconds": 120,
  "max_denials_per_turn": 3
}
```

**Audit.** Before each reviewer request, Mu syncs a `provider_requested` event
with purpose `guardrail`, the Bash call ID and attempt, canonical model origin,
and a checksummed request recipe. Completion stores assembled native response
JSON and the parsed risk/auth/outcome/reason in one `provider_completed` event;
failure stores a classified terminal event. An unmatched request remains
visible and is normalized as interrupted on the next open.

**Concurrency.** Guardrail only targets `destructive` calls, which are always
sequential (concurrent batches only run `readonly` tools). There is no
interaction with the concurrent execution path.

**Undecided design — runtime-triggered review.** This is a design note, not
current behavior or an implementation commitment. A possible extension would
keep the declared-risk gate above while also running Linux Bash children under
a seccomp user-notification filter. When a selected syscall is attempted, the
kernel would block it and notify `mu`; the reviewer would assess the original,
already-persisted Bash tool call and conversation context, not the syscall or
its pointer arguments. An allow would approve the whole tool call, resume the
blocked syscall, and automatically pass later watched syscalls from that call.
A deny or reviewer failure would stop the running tool call before the
triggering syscall executes. Effects completed before the first watched
syscall would remain and must be reported as possible partial effects.

Candidate triggers include destructive filesystem operations, host-control
operations, and `setsid`. Watching `setsid` would review the supported
background-task recipe immediately before it detaches. It would not identify
every possible daemonization technique, and syscall selection remains a
coverage-versus-false-trigger tradeoff: for example, `connect` cannot
distinguish a read-only HTTP request from an upload, while unlink and rename
also occur in benign compiler and atomic-save workflows. Only the first
watched syscall would invoke the reviewer for a tool call.

Seccomp user notification is Linux-only but does not inherently require root:
an unprivileged child can install the filter after setting `no_new_privs`.
That setting prevents later privilege gain through setuid/setgid executables
and file capabilities, so it can change commands such as `sudo`. Any design
must be optional, compile to the existing behavior on unsupported platforms,
and define whether an explicitly enabled but unavailable runtime trigger fails
open or closed. Detached descendants inherit the filter, so approved
background calls also require a listener process that remains available to
pass later notifications after the per-turn `mu` process exits.

**Candidate review-policy map.** Instead of combining the current
`guardrail.enabled` switch with a second deferred-review switch, configuration
could map each declared risk independently to one of three timings:

```jsonc
"guardrail": {
  "policy": {
    "readonly": "skip",
    "reversible": "skip",
    "destructive": "immediate"
  },
  "review_model": null,
  "timeout_seconds": 120,
  "max_denials_per_turn": 3
}
```

`skip` means no review or notification filter, `defer` means review only after
a selected runtime trigger, and `immediate` means review before execution and
then run without the filter. Setting every class to `skip` could replace the
global disable switch. Open questions are whether this should replace
`enabled` outright, what bundled defaults should be, and whether an unavailable
`defer` policy should promote to `immediate`, fail the call, or weaken to
`skip`. Promotion preserves protection and cross-platform behavior but can add
unexpected latency; weakening to `skip` can silently defeat the user's intent.

**Candidate per-call escalation.** Some commands must avoid the deferred path
even when their declared risk maps to `defer`: privilege-changing commands can
be broken by `no_new_privs`, and detached processes inherit the filter. One
candidate is an optional `review_before_execution` Boolean on the Bash call. It
would default false and only promote `defer` to `immediate`; it could not relax
an `immediate` policy or override a user-selected `skip`. This keeps the
declared risk accurate, but adds a niche, platform-motivated field to every Bash
schema. Its name is also misleading when the configured policy is `skip`,
because setting it would still not cause review.

Another candidate is a fourth Bash `risk` value such as `needs-review` or
`review-required`. It would request immediate review and filter-free execution
without calling the action destructive. This keeps the common call shape to
one classification field and works independently of deferred-review platform
support. The tradeoff is semantic impurity: `readonly`, `reversible`, and
`destructive` describe recoverability, while `needs-review` describes routing
and loses the agent's recoverability assessment. The reviewer can still record
the assessed risk. Audit storage could project this as `declared_risk = NULL`
plus a review-request marker rather than treating it as a fourth risk rank.

If `needs-review` is configurable, its policy should accept only `skip` or
`immediate`; `defer` contradicts why the value exists. If it is hard-coded to
`immediate`, an all-`skip` policy no longer fully disables reviews. That
authority question remains open. `unsandbox` is a poor candidate name: this
feature is not a sandbox, the term exposes an implementation detail, and it
frames the value as a general escape hatch rather than a request for review.

**Candidate automatic routing.** Mu could statically match or parse explicit
commands such as `sudo`, `doas`, `pkexec`, and `setsid` and promote them to
immediate review. This avoids a tool-schema addition, and false positives only
cause earlier review. It cannot reliably cover dynamic Bash (`"$runner"`,
functions, aliases, or nested scripts), adds a shell-analysis subsystem for
niche cases, and has been identified as an undesirable direction. It must not
be presented as complete enforcement.

**`sudo` and privilege elevation.** Once an unprivileged child sets
`no_new_privs`, it cannot clear the flag, and setuid/setgid executables and file
capabilities cannot elevate it. A deferred command therefore cannot encounter
`sudo` and then transparently switch to an unfiltered execution path. Candidate
approaches are:

- route the whole call to immediate review before spawn via a Boolean or
  `needs-review` sentinel;
- use best-effort static command detection, accepting missed dynamic cases;
- let missed cases fail under `no_new_privs` with a targeted diagnostic;
- introduce an unfiltered execution broker that recreates the command's cwd,
  environment, stdio, process group, and exit status outside the filtered
  process tree.

The broker is substantially more complex and creates a deliberate filter escape
surface. Killing and automatically restarting the whole Bash call after
discovering `sudo` is also unsafe: effects may already have occurred and would
be repeated. A privileged helper that installs seccomp without `no_new_privs`
would be an even larger security and deployment commitment.

**`setsid` and detached descendants.** `setsid` can itself be watched and
reviewed immediately before detachment, but allow cannot remove an inherited
seccomp filter. Candidate outcomes are:

- route known background launches to immediate review before spawn;
- reject a deferred `setsid` call before detachment and require a new
  immediate-review call;
- hand the listener to a persistent drainer/broker that outlives the Bash tool
  call and automatically continues later notifications after the one whole-call
  approval.

The first two keep listener lifetime bounded but require an explicit routing
mechanism. The broker supports transparent detachment but adds process
supervision, cleanup, crash recovery, and shutdown concerns. Watching `setsid`
does not identify every daemonization technique.

**Candidate observe mode.** A continue-only mode can measure syscall frequency,
latency, and false-trigger rates without invoking the reviewer. It is not truly
behavior-neutral: it still installs seccomp, sets `no_new_privs`, exercises the
supervisor, and can break commands if the implementation is wrong. It could be
a user-visible `shadow` configuration, but that adds permanent configuration and
audit surface. A narrower candidate is a standalone spike or development-only
observe harness that is removed or kept out of the product after measurement.

**Reframed fundamental constraints.** Two constraints define the realistic
scope of deferred review:

1. An ordinary unprivileged child must set `no_new_privs` before installing a
   seccomp filter. A caller with `CAP_SYS_ADMIN` in its user namespace can
   install the filter without that flag, so `no_new_privs` is not an intrinsic
   seccomp-notification requirement; it is the normal unprivileged deployment
   requirement. The flag is irreversible and inherited, so it prevents later
   privilege gain through all setuid/setgid and file-capability programs, not
   only `sudo` and `pkexec`.
2. A listener must remain available until every task inheriting the filter has
   exited. It need not be the original Mu thread or process—the fd can be handed
   to a broker—but approval does not remove or weaken the filter. Every later
   watched syscall still needs a response, and listener loss makes it fail with
   `ENOSYS`.

The second constraint is broader than `setsid`. A descendant can outlive the
launching shell through ordinary backgrounding with redirected stdio,
double-forking, process-group changes, or another daemonization technique. A
tool call can therefore appear complete while a filtered descendant remains
dependent on the listener.

**Additional kernel and lifecycle edge cases.**

- Only one filter installed with `SECCOMP_FILTER_FLAG_NEW_LISTENER` can exist
  in a thread's inherited filter tree. A synchronous inner Mu that inherits an
  outer Mu notification filter cannot simply install its own independent
  listener; the second installation fails with `EBUSY`. This invalidates the
  simple stacked-supervisor model. Candidates are to review the whole outer
  delegation and let that listener drain inner work, route delegation outside
  deferred review, or design explicit listener handoff/cooperation.
- Closing the last listener does not cleanly cancel descendants; their later
  watched syscalls receive `ENOSYS`. Conversely, a blocking notification
  receive can remain blocked after a target exits, so supervisor shutdown needs
  pollable cancellation and explicit reaping rather than relying on cross-thread
  fd close.
- A signal can interrupt a notified syscall, invalidate the notification, and
  restart the syscall, producing another notification for the same logical
  operation. One-review semantics must tolerate `ENOENT`, revalidate ids, and
  drain duplicate/restarted notifications without reviewing twice.
- Reviewer latency occurs while the target is suspended. Command deadlines must
  define whether review time counts, and concurrent triggers can leave several
  commands blocked while reviews are serialized.
- Existing container or service-manager seccomp filters can deny installation
  or return a higher-precedence action for a watched syscall. Capability probing
  must occur in the actual child execution environment and cannot promise that
  Mu's notification action wins every filter stack.
- Syscall coverage remains incomplete independently of these lifecycle issues.
  `io_uring`, inherited writable descriptors, shared mappings, device `ioctl`s,
  local IPC, alternate syscall variants, and future interfaces can produce side
  effects without a selected notification trigger.

**Resulting architecture candidates.**

1. **Scoped in-process deferred review.** Support only non-privilege-changing
   process trees whose descendants are guaranteed to end with the tool call.
   Exceptional, detached, and recursive Mu calls use immediate review or skip.
   This is the smallest design but requires a reliable routing contract and
   deliberately does not support arbitrary Bash.
2. **Persistent unprivileged broker.** Transfer listener fds to a process that
   can outlive tool calls and turns. This solves descendant lifetime, including
   approved detached work, but does not solve `no_new_privs`, privilege
   elevation, or the inherited-listener conflict for independently reviewing
   nested Mu calls.
3. **Privileged persistent launcher/broker.** A tightly controlled launcher with
   `CAP_SYS_ADMIN` could install the filter without `no_new_privs`, drop to the
   invoking user, spawn Bash, and retain the listener. This is the only
   candidate that addresses both fundamental constraints generally, but it
   introduces a privileged service/helper, authentication and command-binding
   requirements, deployment and upgrade concerns, and a substantially larger
   security boundary than Mu currently has.

The existing size estimate applies only to a scoped in-process design. Either
broker architecture would require a new estimate and a separate threat model.

Open decisions therefore include the policy-map shape and defaults, whether
per-call routing uses a Boolean or a `needs-review` sentinel, the authority of an
all-`skip` configuration, unsupported-platform promotion, the initial syscall
set, Linux dependency strategy (libseccomp versus direct BPF), `sudo`
compatibility, listener lifetime for every surviving descendant, nested Mu
semantics, interaction with concurrent readonly calls and command timeouts,
audit fields, whether observe mode exists only for development, and whether the
feature's benefit justifies a persistent or privileged broker. A scoped
in-process implementation is estimated at roughly 780–1,220 production lines
plus 530–890 lines of tests and documentation; a narrow proof of concept would
be smaller but would not establish the complete runtime contract.

---
