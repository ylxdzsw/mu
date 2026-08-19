# mu — Product Specification

`mu` is a small, composable terminal agent runtime: one prompt in, one completed
turn out. The core binary reads a prompt, runs an agent loop with one
model-visible `bash` tool, streams the selected presentation, persists durable
session events, and exits. The zsh and Fish integrations build an interactive
experience from that turn primitive without replacing the user's shell.

This document defines durable product behavior, architectural boundaries, and
the rationale for consequential design choices. It is not a changelog,
implementation diary, release manifest, or migration record. Exact
configuration defaults live in [`src/default_config.jsonc`](src/default_config.jsonc);
user-facing command and configuration references live in
[`builtins/cli.md`](builtins/cli.md) and
[`builtins/config.md`](builtins/config.md).

---

## 1. Goals and non-goals

### Goals

- **Fast startup.** A fresh process runs every turn, so native startup must be
  negligible next to provider latency.
- **Responsive output.** Human-facing modes stream stable output as it arrives
  and return control as soon as the turn completes.
- **Composable turns.** `mu` behaves as a Unix command. Shell scripts,
  supervisors, and shell integrations compose turns rather than embedding a
  separate agent runtime.
- **Shell-native interaction.** The user's shell retains line editing,
  completion, history, job control, aliases, and interactive programs.
- **Visible behavior.** Committed terminal output is append-only. Mu may update
  one trailing progress line, but it never rewrites scrollback.
- **Small model surface.** The model sees exactly one function tool, `bash`.
  Skills and Mu's editing/image helpers are files or shell commands, not extra
  function tools.
- **General terminal assistance.** Coding is supported, but Mu is not limited
  to a coding workflow.

### Non-goals

- No TUI, alternate screen, mouse UI, or line editor in the core binary.
- No long-lived daemon or server in the normal turn path.
- No core shell emulation or PTY-based shell replacement.
- No in-process plugin SDK, MCP client, or subagent orchestrator.
- No dynamic model-visible tool registration.
- No sandbox guarantee or per-command interactive approval prompt.
- No support in this codebase for non-Unix process semantics.

---

## 2. Key decisions and rejected alternatives

### 2.1 Native, one-process-per-turn runtime

**Decision.** Mu is a Rust native binary. Each invocation performs one turn or
one management operation and exits.

This keeps startup fast, installation simple, failure containment ordinary, and
CLI composition natural. Session continuity is durable state, not process
memory.

**Rejected: an interpreted core.** Node, bun, and Python impose a recurring
runtime startup cost on every turn.

**Rejected: a daemon-backed core.** A daemon would add lifecycle, upgrade,
authentication, stale-state, and client/server failure modes to a product whose
natural unit is already a short-lived command. Background services remain
possible as ordinary external tools, but they are not part of Mu's turn path.

### 2.2 Interaction belongs to the shell

**Decision.** The zsh and Fish integrations own prompt mode, line editing,
completion, and shell history. Each submitted prompt starts the same foreground
turn runner used by scripts.

The real shell already has mature job control and editing semantics. Keeping
that ownership avoids a second, inevitably incomplete shell implementation.

**Rejected: a core REPL or TUI.** It would have to reproduce shell completion,
history, multiline editing, signals, PTYs, and plugin compatibility while
making Mu less scriptable.

### 2.3 One model-visible tool

**Decision.** The only function tool is `bash`. Mu-owned `apply_patch`, `edit`,
and `view_image` applets are commands available inside Bash. Skills are
Markdown files loaded through Bash.

One stable schema is easier for models to learn, works across providers, and
retains the full Unix tool ecosystem without an adapter per command.

**Rejected: a broad built-in tool catalog.** Separate read, write, search,
process, web, and skill tools would duplicate shell capabilities and enlarge
every provider request.

**Rejected: dynamic plugins or MCP in the core.** External CLIs remain
reachable through Bash. Protocol-specific plugin hosting would expand the
trusted runtime and contradict the fixed-tool contract.

**Rejected: a dedicated skill tool.** Skills are ordinary files. A special
loader would add a tool solely to read content already available through Bash.

### 2.4 Semantic history plus compatible native replay

**Decision.** Mu persists a protocol-neutral semantic transcript for display,
compaction, recovery, and cross-provider continuation. It also retains
completed provider-native responses for exact replay when the current API and
replay compatibility rules allow it.

**Rejected: native payloads as the conversation model.** That would bind a
session to one provider and make transcripts, compaction, and fallback depend
on provider-specific object shapes.

**Rejected: semantic history only.** Responses and Anthropic may require opaque
or signed reasoning state to continue correctly. Discarding all native state
would needlessly weaken same-provider replay.

Partial provider streams are audit evidence only. They never become semantic
assistant history.

### 2.5 Append-only, per-session persistence

**Decision.** Each session is an append-only JSONL journal with immutable large
objects stored by content hash. Session cleanliness, current context, and
retryability are derived from journal events.

Per-session files isolate concurrency, make crash boundaries explicit, and keep
the authoritative record inspectable without a database service.

**Rejected: mutable status rows.** Flags such as `turn_complete` or
`tool_running` can disagree with message state precisely when a crash occurs.
Deriving state from durable events avoids two sources of truth.

**Rejected: one mutable database for all sessions.** Mu does not need
cross-session transactions or relational queries in its turn path. A shared
database would couple unrelated sessions and obscure the append-only audit
record.

### 2.6 Output density is independent of terminal capability

**Decision.** `final`, `concise`, `detail`, and `full` select information
density. TTY detection independently selects terminal styling and live
presentation.

**Rejected: separate `plain` and `terminal` modes.** They mixed two independent
questions—how much information to show and whether the destination is a
terminal. Removed names are rejected rather than kept as aliases.

### 2.7 Unsandboxed execution with a narrow review gate

**Decision.** Bash runs with the invoking user's permissions. Declared
destructive calls are reviewed by the optional guardrail before execution, but
declared risk and model review are not security boundaries.

This matches Mu's role as a transparent terminal agent. A partial sandbox would
create misleading assurance while breaking ordinary shell workflows.

Rejected and deferred safety alternatives are recorded in §12.

---

## 3. Architecture overview

```text
shell script or zsh/Fish prompt mode
                 │
                 ▼
          mu turn runner
  scope/config/session resolution
  semantic context + provider adapter
  agent loop + Bash execution
  renderer + append-only journal
```

The binary has these durable responsibilities:

- **Entry and management commands:** parse invocation, resolve scope and
  session, compose prompt input, and dispatch one operation.
- **Agent loop:** request a provider response, persist accepted assistant
  items, execute Bash claims, and repeat until the assistant completes.
- **Provider adapters:** translate between the semantic transcript and one of
  the supported wire protocols.
- **Bash executor:** run isolated shell calls, enforce time/output bounds,
  capture attachments, and return durable results.
- **Renderer:** remain the sole writer of user-facing turn output.
- **Store:** append and replay session events and immutable objects.

There is no input thread, line editor, or resident process in the turn runner.

### 3.1 Authoritative turn lifecycle

One normal turn follows this sequence:

1. Parse arguments and resolve the active project or global scope from the
   invoking working directory.
2. Load merged configuration, environment values, instructions, and the prompt
   source. Validate and load any image or audio attachments.
3. Open the active-scope store and resolve a selected, continued, or new
   session.
4. Take a nonblocking exclusive lock on that session journal. Concurrent use of
   the same session fails as `session busy`; different sessions remain
   independent.
5. Reject the submission if that session has an unfinished compaction;
   otherwise normalize any interrupted journal tail.
6. Durably queue the submitted prompt, working directory, Git worktree root,
   and attachment references. Only then select the session as
   `current-session`.
7. Build the candidate semantic provider context. If it exceeds the soft
   threshold, complete an out-of-turn compaction before materializing the
   queued prompt as a turn; otherwise materialize it immediately.
8. Repeat the agent loop:
   - persist a reconstructible provider request before network contact;
   - stream and assemble one provider response;
   - on failure, persist a classified failure while keeping partial content out
     of semantic history;
   - on success, persist the completed native response and ordered semantic
     assistant projection before executing any Bash call;
   - execute accepted Bash claims, persist one result for each started claim in
     provider order, and request the provider again;
   - stop on a completed assistant response.
9. Emit the selected completion presentation, release the lock, and exit.

Only `bash` claims with valid object arguments and the provider's tool-use
finish state are executable. Unknown functions, malformed arguments, or calls
attached to another terminal finish state are protocol failures or audit-only
content, never speculative execution.

Contiguous `risk:"readonly"` calls may execute concurrently. Their display,
journal records, tool-result messages, and next provider request retain the
provider's original call order. Any other call is an ordering barrier.

### 3.2 Interruption

An interrupt stops new work, cancels the active provider request when possible,
and terminates every active Bash process group. Mu drains available output and
persists results for calls that began. Incomplete assistant streams do not enter
semantic history.

The next invocation normalizes unmatched requests and result-less Bash claims
before continuing. A new prompt may redirect the work; `mu retry` continues the
interrupted turn without adding a new prompt.

---

## 4. Tools

The complete model-visible schema is:

```ts
bash({
  title: string,
  risk: "readonly" | "reversible" | "destructive",
  command: string,
  cwd?: string,
  timeout?: number,
  stdin?: string
})
```

- `title` is concise human-facing action text.
- `risk` is advisory audit/UI metadata. It selects guardrail review only for
  `destructive`; it does not constrain the process.
- `command` runs as `bash -lc`.
- `cwd` applies to this call only. Calls do not share `cd`, shell variables, or
  exported environment changes.
- `stdin` is written literally to the child and is the preferred path for
  multiline or escaping-sensitive data.
- `timeout` is a positive number of seconds and defaults to 120.

The resolved Mu applet directory is prepended to the post-login `PATH`. Calls
run in separate process groups. Timeout or interruption sends TERM, waits a
short grace period, then sends KILL; Linux also requests a parent-death signal
for the direct child. Ordinary calls are expected not to outlive their tool
result.

Recursive Mu delegation is bounded by `MU_SUBAGENT_DEPTH`: management commands
remain available, but recursive agent turns beyond one nested level are
rejected.

### 4.1 Bash output

Stdout and stderr are combined, redacted, and streamed to human-facing modes.
The model-visible and persisted result is bounded by:

- `limits.max_lines`;
- `limits.max_bytes`;
- `limits.max_line_bytes`.

When a result exceeds a bound, Mu keeps a tail preview and an omission marker.
It best-effort spills the complete redacted output to a private, randomly named
file under `$TMPDIR/mu`; spill files are ephemeral and have no retention
guarantee. A spill failure does not change the command result.

Timeouts, interruptions, and Mu's internal output ceiling use the same bounded
partial-output policy. The failure reason remains separate and visible even
when the partial output is truncated.

### 4.2 Mu applets

These are shell commands, not function tools:

- **`apply_patch`** applies add, update, move, and delete operations from one
  structured patch. It preflights the complete patch before publication,
  rejects conflicting targets and existing add/move destinations, and supports
  repeated non-moving updates to one path as a single final update. Relative
  paths resolve from the Bash call's working directory. Updating through a
  symlink edits its regular-file target; moving or deleting a symlink acts on
  the link.
- **`edit [--relaxed] FILE`** applies one or more uniquely matching
  SEARCH/REPLACE blocks to an existing UTF-8 regular file. Strict mode is
  byte-exact and reports a unique relaxed match as guidance. `--relaxed`
  progressively tolerates line-ending and line-edge whitespace differences,
  never arbitrary internal whitespace. All matches are computed against one
  snapshot and overlapping matches are rejected. The write preserves the
  existing inode, permissions, hard links, and symlink target relationship,
  using an advisory lock and a recoverable sibling backup.
- **`view_image [--detail auto|low|high|original] PATH`** validates and attaches
  a PNG, JPEG, WebP, or GIF to the current Bash result. It works only inside a
  live Mu tool call, snapshots immutable bytes into the session object store,
  and permits at most eight staged images per claim.

User attachments and `view_image` use the same 20 MiB per-file limit. Turn
attachments additionally support WAV and MP3. Provider-specific media support
is defined in §7.

---

## 5. Output and rendering

Output density changes presentation, not agent behavior:

| Mode | Contract |
|---|---|
| `final` | Buffer the turn and write only the final assistant text on success, without an added newline. On unrecovered failure, write `error: ...\n` and exit nonzero. |
| `concise` | Stream assistant text and notices; reduce each Bash call to one committed outcome line. Reasoning progress is ephemeral. |
| `detail` | Normal human transcript: thought status, tool headers, bounded output previews, exits, and turn summary. |
| `full` | Expose available reasoning/summary text and complete redacted tool presentation. Model-context truncation still applies. |

Resolution order is explicit `--output`, merged `config.jsonc`, then `detail` if
no configuration supplied a value.

### 5.1 Terminal and stream contract

- `final` ignores terminal capability.
- In other modes, stdout TTY detection enables ANSI styling, terminal Markdown,
  width-aware wrapping, hyperlinks, and one mutable trailing progress line.
- Redirected stdout is ANSI-free, sequential, and preserves raw assistant
  Markdown.
- The renderer is the sole stdout/stderr writer.
- Committed transcript blocks remain append-only and in provider order.
- Tables may be buffered until their layout is stable; ordinary prose streams
  once its Markdown interpretation is unambiguous.
- Terminal width is sampled at turn start. Mu does not reflow committed output
  after resize.

Assistant text, tool presentation, tool failures, and Bash output go to stdout.
Fatal process diagnostics and the normal `detail`/`full` summary go to stderr.
The summary is shown only for a successful turn when stderr is a terminal.

Top-level interactive transcript blocks have exactly one empty line between
them. The shell integration owns spacing between the submitted `mu>` prompt and
the child's first visible block; the renderer owns later boundaries. A Bash
header, output, omission marker, and exit line form one block.

Interactive `detail` and `full` show complete link destinations; interactive
`concise` shows labels while retaining hyperlinks. Redirected output preserves
the model's Markdown in every non-final mode.

When configured, a terminal bell sounds after a successful non-final
interactive turn whose duration meets the configured minimum.

### 5.2 Reasoning visibility

- Chat Completions `reasoning_content` is open text. `detail` commits a compact
  thought status; `full` streams the text.
- Responses and Anthropic reasoning may be opaque. `detail` may show duration
  and a conservative provider summary title; `full` shows only summary text the
  provider exposed.
- `concise` may use a live reasoning indicator but does not commit it. A
  summary title appears as soon as it is identified and remains on timer
  updates and adjacent reasoning items until another title replaces it or a
  semantic boundary discards the indicator.
- `final` suppresses all reasoning and progress.

Opaque reasoning is never invented for display.

---

## 6. zsh and Fish shell surfaces

The plugins provide the same product contract:

- Tab at cursor zero toggles `mu>` mode while preserving the edit buffer.
- Non-empty Enter submits one foreground Mu turn; empty Enter redraws the
  prompt without creating a turn.
- Ctrl-C cancels an edited draft or interrupts the foreground Mu process using
  ordinary shell signal behavior.
- Ctrl-D retains normal shell EOF behavior.
- Up/Down navigate within multiline input and then browse Mu-tagged shell
  history without mixing ordinary commands. Recalled prompts run with the
  current session, model, scope, and attachment state.
- Shift+Enter inserts a newline when the terminal provides a distinguishable
  key sequence.
- Slash commands and model names use native shell completion.

The plugins never implement provider, store, tool, guardrail, retry, or
compaction semantics.

### 6.1 Shell-owned session bundle

Each shell integration tracks one in-memory bundle:

- active scope;
- optional session id;
- optional sticky model override;
- staged attachments.

Changing directory temporarily masks a bundle from another scope. A submitted
prompt or valid slash action activates the current scope and discards a bundle
owned by another scope. Passive prompt/status/completion reads do not mutate
state.

An unattached plugin creates a model-free session with `mu new` before the first
turn, then passes that id explicitly. `MU_ZSH_SESSION_ID` or
`MU_FISH_SESSION_ID` may seed an existing active-scope session.

### 6.2 Slash behavior

- `/new` clears the session id but preserves the shell model override and
  staged attachments.
- `/load [<id>]` replays a session and attaches only after successful replay.
  Without an id, it selects the active scope's persisted `current-session`.
- `/model <ref>` stores a shell-only override for later turns and retries.
- `/attach <file>` stages a readable attachment; `/attach` lists and
  `/attach --clear` discards the queue. The next ordinary prompt or custom
  command consumes the queue.
- `/retry` and `/compact` call the corresponding management operation.
- Discovered custom commands accept the remainder of the slash input as their
  custom instruction.
- `/goal <goal>` invokes the built-in `goal` custom command. Its required
  custom instruction is the goal.

Unknown or malformed slash input does not activate a new scope or mutate the
bundle.

zsh requires `jq` and supports native ZLE completion/hooks. Fish integration
requires Fish 4 and wraps the user's prompt and editing bindings without
replacing normal shell mode.

---

## 7. Provider and model integration

Mu has three hand-written streaming adapters:

- OpenAI-compatible Chat Completions;
- OpenAI Responses;
- Anthropic Messages.

Each provider has one complete endpoint. A request path ending in
`/chat/completions`, `/responses`, or `/messages` selects the adapter. Any other
path is invalid. HTTP(S) and `http+unix` endpoints are supported; Unix-socket
paths are percent-encoded in the URI authority. Protocol selection never
depends on provider or model names.

Every adapter consumes the same semantic message list and Bash schema and
returns ordered assistant items: reasoning, text, and Bash calls. The renderer,
agent loop, tool executor, guardrail, retries, and compaction remain
protocol-neutral.

### 7.1 Replay boundaries

The semantic transcript is always authoritative. Native replay follows these
additional rules:

- **Chat Completions:** completed open `reasoning_content` attached to tool-use
  responses can replay across Chat providers and models. Empty and omitted
  reasoning remain distinct.
- **Responses:** requests are stateless (`store:false`; no
  `previous_response_id`). Completed output items replay only under the same API
  and effective `replay_key`.
- **Anthropic Messages:** completed content blocks replay only under the same
  API and effective `replay_key`.

A model's effective replay key is its configured non-secret `replay_key`, or
`provider/model` when omitted. Switching API or an incompatible key falls back
to the semantic transcript without rewriting history.

Images are supported by all adapters. Audio is supported only by Chat
Completions; Responses and Anthropic reject it locally before network I/O.

### 7.2 Provider failures, retry, and fallback

Provider errors are classified by meaning rather than raw status text:

| Class | Action |
|---|---|
| `context_length` | Apply context recovery when enabled. |
| `unavailable`, `auth` | Advance a floating provider candidate immediately. |
| `overloaded`, `rate_limit`, `transport` | Retry the candidate, then advance if floating. |
| `request_too_large`, `bad_request`, `protocol` | Fail immediately. |

Classification uses structured provider codes before status/message
heuristics. Invalid or malformed successful responses are protocol failures,
not accepted partial progress.

A fixed provider/model permits five retries after the initial attempt. A
floating model permits three retries per candidate. Delays are deterministic
`1s, 2s, 4s, 4s, 4s`; a valid standard `Retry-After` may increase the wait.
Requests exceeding the 60-second retry-after ceiling advance a floating choice
or fail a fixed/final choice.

One known Anthropic delivery anomaly is intentionally narrow: a delta that
references a content block no longer open is treated as transport failure so
the bounded retry/fallback path can discard the malformed partial response.
Other invalid content-block sequencing remains a fatal protocol error.

### 7.3 Model selection

- `provider/model[:effort]` is fixed.
- `model[:effort]` expands to providers defining that model in merged
  configuration order.
- Model ids cannot contain `:`; it separates the optional effort.
- `supported_efforts` is an ordered status/completion hint, not validation of
  manually entered effort strings.
- A session remembers the forward-only provider position for each floating
  model across agent, compaction, and guardrail requests.
- Fixed choices do not mutate floating positions.
- Missing historical candidates are skipped; Mu never adds compatibility
  aliases or rewrites the journal to recover them.

Without an invocation override, an attached session's latest configured choice
wins, then the prior `current-session` choice for a new session, then the first
configured model.

### 7.4 Automatic resume

An adapter may classify a complete response as resumable. With
`auto_resume:false`, that response is an ordinary clean ending. With
`auto_resume:true`, Mu persists it, derives
`Continue the current task from where you stopped.`, and continues the same
turn using the current retry quota.

Progress resets the quota. Exhaustion advances a floating candidate; a fixed or
final candidate exits nonzero and leaves the turn retryable. A normal new
prompt supersedes an unused resume rather than silently inserting the derived
continuation.

### 7.5 Provider availability

Operations that make provider requests—a turn, a dirty retry, and compaction—
require a configured provider, supported endpoint, model, and any named API-key
environment value. `status` validates and resolves provider/model
configuration but does not contact a provider or require its key. `init`,
`new`, `sessions`, `transcript`, `context`, and `cat` remain available without
a configured provider. API keys are read from the effective environment and
are not written into session provider-request records.

---

## 8. Skills and custom commands

Mu discovers instruction files in the built-in, global, and active-project
roots. Later scopes shadow earlier entries by skill name or command path.

### 8.1 Skills

A skill is either a direct regular file with supported frontmatter or a direct
`folder/SKILL.md`. Its declared name must match the file stem or folder.
Frontmatter contains:

- required `name`;
- required `description`;
- optional comma-separated `requires_env`;
- optional comma-separated `requires_commands`.

A skill is listed only when all requirements are satisfied. Mu injects one
complete Markdown `<skills>` document containing loading guidance and active
skill metadata: name, description, and absolute path. Supporting files are not
indexed.

Before responding, the agent scans the metadata and reads any named or
partially relevant skill in full through Bash. Loading supplies context; it
does not automatically grant the skill's instructions authority over the user
or system prompt.

### 8.2 Custom commands

A regular instruction file becomes a custom command when its first line is a
supported Mu shebang. The shebang accepts no arguments or exactly
`-m|--model <model-ref>`. Command names are relative instruction-root paths,
including extensions.

A file may be both a command and a skill. Command invocation strips the shebang
and supported frontmatter before submitting the prompt. An explicit invocation
model overrides the shebang model; otherwise the shebang is turn-local and does
not rewrite session model state.

The extensionless built-in `goal` is a custom command, not a skill. It requires
a goal as its custom instruction. The command agent acts only as supervisor: it
creates one fresh worker session, repeatedly continues that same session, and
judges completion without planning, diagnosing, or performing the work. The
worker owns current-state inspection, planning, execution, and verification.
Every continuation repeats the original goal verbatim to the worker. The loop
ends when the supervisor verifies the goal or finds a genuine blocker requiring
user input, permission, credentials, or unavailable external state.

---

## 9. Project discovery and resource layout

Mu searches upward from the invoking working directory. The nearest directory
containing `.mu` or `.git` is the active project. If none is found before the
home/root boundary, Mu uses global scope.

A `.mu` marker always wins. Standard linked Git worktrees without a closer
`.mu` share the primary checkout's Mu scope while retaining the linked
worktree's working directory and Git state. Unusual Git layouts fall back to
the discovered checkout without invoking Git solely to resolve scope.

Project configuration, instructions, sessions, and objects are private to the
active scope. Nested project configuration is not merged.

```text
<scope>/.mu/ or global config root
  config.jsonc
  .env
  AGENTS.md
  instruction files and skill folders
  sessions/<session-id>.jsonl
  objects/<sha256>
  current-session
```

Discovery and read-only inspection do not create project files. Runtime paths
appear only when needed. `mu init` explicitly creates minimal project metadata
and refuses an already nested Mu project unless `--force` is supplied.

### 9.1 Installed and portable resources

For an executable under `<prefix>/bin`, native resources live under
`<prefix>/share/mu` and applet links under `<prefix>/libexec/mu`.

The optional `portable` build embeds built-ins. Each resource independently
uses a valid installed directory when present; otherwise Mu materializes it
under the platform user cache. Existing cache directories are authoritative
and are not refreshed or repaired automatically. Mu never falls back to `/tmp`
for package resources.

---

## 10. Configuration and prompt construction

Global configuration lives in `~/.mu` or `$MU_CONFIG_DIR`; project
configuration lives in the active project's `.mu`.

Precedence is:

1. bundled non-provider defaults;
2. global `config.jsonc`;
3. project `config.jsonc`.

Objects merge recursively; scalar and array values replace inherited values.
Provider/model object order is meaningful for defaults, fallback, status, and
completion. Bundled provider entries seed a missing global config but do not
silently merge into an existing one.

[`src/default_config.jsonc`](src/default_config.jsonc) is the source of truth
for field defaults. The durable field groups are:

- `output` and `auto_resume`;
- ordered `providers`, each with `endpoint`, optional `api_key_env`, and
  ordered `models`;
- per-model `context_window`, optional `supported_efforts`, and optional
  `replay_key`;
- `terminal_bell`;
- `compaction`;
- `limits`;
- `redaction`;
- `guardrail`.

Unknown output names and removed aliases are configuration errors.

### 10.1 Environment overlay and redaction

Environment precedence is process, global `.env`, then project `.env`.
Assignments from a later source replace earlier values. Mu parses `.env` as
data and never executes it.

Accepted assignments have an optional `export` prefix, a shell identifier,
`=`, and a bare, single-quoted, or restricted double-quoted value. Blank lines
and full-line comments are allowed. Expansion, command substitution,
multiline values, concatenated quoting, indentation, whitespace around `=`,
inline comments, and other shell syntax are rejected. A file is fully parsed
before any of its assignments apply.

The effective environment supplies provider keys, skill requirements, and Bash
children. Values selected by provider `api_key_env` or `redaction.env` are
exact-value redacted from captured Bash output before it is persisted or shown
to the model. Selectors are exact names or one leading `*` suffix match.

This redaction is deliberately narrow. Mu cannot promise that arbitrary secrets
printed by commands, embedded in prompts, or stored in unrelated files will
never enter a journal.

### 10.2 System prompt

The system prompt is assembled when a session is created and then persisted.
Existing sessions do not silently rebuild it after instruction/config changes.
Its fixed order is:

1. `<system_preamble>` from `src/system_preamble.md`;
2. `<runtime>` with stable host and project facts;
3. one complete Markdown `<skills>` document when active skills exist;
4. global `<agents_md>`;
5. project `<agents_md>`.

Current working directory and Git worktree root are turn facts, not permanent
system facts. Context projection adds location information when retained turns
move between directories.

Tool definitions are sent through the provider's tool parameter, not copied
into the system prompt.

### 10.3 Prompt sources and CLI surface

A default invocation runs one turn:

```text
mu [-s ID | -c] [-m MODEL] [-a FILE ...]
   [-o final|concise|detail|full] [PROMPT_FILE_OR_COMMAND]
```

Without a positional target, stdin is the complete prompt. A positional name
first resolves to a discovered custom command unless it is an explicit path,
then falls back to a prompt file. Exact management subcommand names win.

File-backed prompts strip a supported shebang. Terminal stdin is left unread;
non-terminal stdin, when non-empty, is appended after `\n---\n\n` as a custom
instruction.

Management commands:

| Command | Durable behavior |
|---|---|
| `mu init` | Create minimal project metadata. |
| `mu new` | Create a model-free session without selecting it. |
| `mu sessions` | List recent active-scope sessions. |
| `mu transcript` | Replay the persisted semantic transcript without contacting a provider; `--html` emits an xterm.js document that loads pinned assets when opened. |
| `mu status` | Report resolved scope, selection, model, context, and optional expensive indexes. |
| `mu context` | Show the system prompt Mu would assemble; `--export` emits user instructions and non-built-in skill guidance for a foreign agent. |
| `mu cat` | Resolve and preview exact prompt input without provider contact or session creation. |
| `mu retry` | Normalize and continue an interrupted turn without a new prompt; a clean session reports a no-op. |
| `mu compact` | Force compaction, optionally using non-terminal stdin as a focus instruction. |

Provider-free management commands do not contact a provider.

---

## 11. State, recovery, and context

State lives in one active scope. A session journal begins with immutable
metadata and then contiguous, timestamped events. Readers accept only the
complete newline-terminated prefix; the next writer may truncate an incomplete
final line, while malformed earlier content is corruption.

Mu accepts only the current journal version and contains no session migration
path. Explicit selection and management operations that require a session fail
on an unsupported version. Listing skips incompatible journals with warnings.
When an unselected turn consults `current-session` only to inherit scope state,
it may warn, ignore an incompatible target, and create a new session.
`--continue` uses the same fallback because it does not name a specific
journal.

The durable event model is:

- **`system_prompt`** — exact initial model-visible prompt.
- **`turn_started`** — submitted prompt, working directory, Git worktree root,
  and attachment references.
- **`provider_requested`** — request purpose, resolved provider/model origin,
  and a checksummed recipe sufficient to reconstruct the native request.
- **`provider_completed`** — completed native response and accepted semantic
  projection.
- **`provider_failed` / `provider_interrupted`** — terminal audit outcome with
  no semantic assistant message.
- **`bash_completed`** — unique result and attachment references for one
  durable Bash claim.

Assistant projections contain one ordered item array and a derived turn state:
continue for tool use, resume for preserved incomplete work, or complete.
Array position is authoritative.

There is no mutable session row, title, cached status flag, owner PID, or
separate run entity. Session listings and status are projections of journal
events.

### 11.1 Session selection and ownership

- `--session ID` selects an existing active-scope journal or exits with code 2.
- `--continue` follows `current-session`, creating a session when the pointer is
  absent, broken, or targets an unsupported journal version.
- No selection creates a fresh session for a turn.
- `mu new` creates but does not select a session.
- `current-session` changes only after a submitted turn is durable.

An exclusive, nonblocking advisory lock on the journal owns a mutable
operation. The descriptor remains open for the operation and the kernel
releases it on exit. Mu does not use PID leases or stale-owner recovery.

### 11.2 Cleanliness and interrupted-tail normalization

A latest turn is clean only when its latest accepted assistant state is
complete and all Bash claims have results. Unmatched provider requests,
resume states, and result-less claims make it dirty.

Before a turn or retry, Mu:

- removes an incomplete final record;
- appends interruption outcomes for unmatched provider requests;
- gives each result-less Bash claim a deterministic denied result or a
  conservative interrupted result.

This normalization is idempotent.

Mu deliberately does not guess whether a result-less claim began execution.
Side effects may occur between durable claim publication and process tracking,
so recovery says "possibly executed" and lets the agent verify rather than
risk repeating work.

### 11.3 Semantic context and transcripts

Context projection uses:

- the persisted system prompt;
- the latest committed session checkpoint;
- turn prompts and derived location reminders after that checkpoint;
- accepted assistant items;
- Bash results.

Provider failures and partial native responses remain audit-only.

`mu transcript` is a semantic session transcript, not a dump of the journal.
It includes user prompts, assistant text, paired Bash calls/results, and
derived compaction trigger/result lines. The default transcript also replays
the ordinary synthetic compaction turns, including their prompts and any tool
activity. `--epoch N` limits replay to provider activity in one context epoch.
`final` output omits compaction turns and status lines. All formats omit system
prompts, provider payloads, and journal metadata.
Only open Chat reasoning is displayable, and only in `full`; opaque Responses
and Anthropic reasoning remains omitted. Stored redaction is authoritative.

Transcript output reuses the normal output densities. It reconstructs the
historical prompt model, working directory, and prior context usage when the
journal contains enough information. HTML output is a renderer replay, not a
second transcript model. Its xterm viewport remains 100 columns wide, centers
when space permits, scrolls horizontally on narrower viewports, and debounces
vertical fitting during resizing. Line breaks produced by the fixed-width
renderer are not reflowed after export. Both scroll surfaces use a thin dark
theme rather than the browser default.

### 11.4 Usage accounting

Provider-reported usage is authoritative when available. Context fullness uses
the latest compatible accepted agent response's total, not a sum of every
iteration. Turn input/output figures sum the exchanges in that turn and account
for provider-reported cache reads/writes separately.

Later semantic input adds an estimate to a compatible reported anchor. If API,
model id, or effective replay key changes, Mu estimates the complete active
projection. Text estimation uses a simple bytes/4 approximation; media has a
bounded nonzero estimate. Mu does not ship a tokenizer.

### 11.5 Compaction

Setting `compaction.enabled:false` disables every automatic compaction tier.
Manual `mu compact` remains available.

Automatic context management has three triggers:

1. **Soft new-turn threshold.** Before a new turn's first provider request,
   compact when context is strictly above
   `floor(context_window * soft_fraction)`.
2. **Hard tool-result threshold.** After every concurrently issued Bash call
   has reached a result, and before sending those results back to the model,
   compact when context is strictly above the lower of the configured hard
   fraction and configured headroom boundary.
3. **Emergency overflow.** A classified provider context-length error starts
   emergency compaction. A context-length error during soft or hard compaction
   upgrades that attempt to emergency; the same error during emergency
   compaction is fatal.

Compaction is an ordinary synthetic turn in the current context. Its request
uses the same model context, native replay, Bash tool, guardrail, streaming,
fallback, persistence, and retry machinery as any other agent request. Mu asks
for a plain Markdown checkpoint, accepts the final assistant text without
parsing section syntax, and then appends a structured `compaction_applied`
event containing the complete summary. The event advances the session context
epoch and projects only the original system prompt plus:

```xml
<session_checkpoint mode="await_user|continue_turn" epoch="N">
...
</session_checkpoint>
```

Out-of-turn compaction uses `await_user`, then materializes the previously
queued prompt. In-turn compaction uses `continue_turn`; its requested summary
ends with an `Immediate next step` section and the agent loop continues
immediately from the checkpoint. Manual compaction chooses the mode from
whether the selected session is clean, and optional non-terminal stdin becomes
an additional focus instruction.

The displayed and persisted “before” size is the estimated provider input that
triggered compaction; for a soft trigger it therefore includes the queued
prompt. The “after” estimate is the immediate next provider input. Automatic
`await_user` compaction includes the queued prompt because Mu materializes it
immediately, while `continue_turn` measures the checkpoint continuation.

Every provider request uses cache key `mu:<session>:epoch:<N>`. The compaction
request remains in the old epoch; only a successfully applied checkpoint
advances the epoch. Once a compaction turn is durable, new prompts and another
manual compaction for that session are rejected until `/retry` completes it.
If the final summary was durable but the applied event was not, `/retry`
commits that existing summary without contacting the provider again.

Epoch-filtered transcripts place a compaction request, its streamed activity,
and its result in `from_epoch`, matching the cache key used for that request.
The checkpoint and subsequent provider activity appear in `to_epoch`.

Emergency compaction makes a request-only projection that replaces oldest Bash
results first with `[Bash output unavailable during emergency compaction.]`
and removes their attachments until the estimated reduction reaches the
configured hard headroom.
Calls, arguments, stdin, and journal data are unchanged. The applied event
records the elided durable call IDs. If pruning every Bash result is
insufficient, Mu still makes one emergency request; a context-length error from
that request is fatal.

The summary request output limit is capped by the configured hard headroom.
Mu rejects an empty summary, but it does not apply a post-compaction soft-limit
test. A failed compaction leaves its ordinary turn and provider records
available for `/retry` and does not advance the epoch. Manual compaction of a
session with no conversation history is inapplicable.

### 11.6 Bounds and exit status

`limits.max_iterations` bounds tool round-trips in one turn. Reaching it exits
nonzero while retaining every completed message and Bash result.

Exit status:

- `0`: success, including a clean `mu retry` no-op;
- `1`: general, configuration, or unrecovered provider error;
- `2`: session busy or explicit session not found;
- `128 + signal`: forwarded terminating signal, commonly 130 for SIGINT and
  143 for SIGTERM.

Mu cannot pause and resume a partial provider stream. Resume always restarts
from the last completed semantic boundary.

---

## 12. Safety posture

Mu is deliberately unsandboxed. Bash has the invoking user's filesystem,
network, process, and credential access. The terminal transcript and journal
improve visibility and recovery; they do not make execution safe.

Durable safeguards are:

- append-only visibility of accepted assistant actions and captured results;
- foreground interruptibility and process-group termination;
- output bounds and exact-value redaction for configured environment values;
- a review gate for calls declared `destructive`;
- treating external content as untrusted data in the agent prompt.

The agent can misclassify risk, the reviewer can be wrong, commands can hide
effects, and unselected secrets can enter output. Users must treat Mu as code
execution with their own authority.

### 12.1 Guardrail

When enabled, each Bash call declared `destructive` receives a separate
provider request before execution. The reviewer has no tools and sees a
budgeted semantic transcript plus bounded action JSON. It returns `risk_level`,
`user_auth_level`, and a reason.

Execution requires authorization rank at least risk rank:

| Risk | Rank | Minimum authorization |
|---|---:|---|
| `low` | 0 | `unknown` |
| `medium` | 1 | `low` |
| `high` | 2 | `medium` |
| `critical` | 4 | `explicit` |

Authorization ranks are `unknown` 0, `low` 1, `medium` 2, `high` 3, and
`explicit` 4. The gap before critical ensures only explicit authorization can
approve it.

An allowed call executes. A denial becomes a Bash error so the agent can choose
a less destructive approach or request authorization. Reviewer failure records
that execution did not begin and aborts the turn. Repeated denials are bounded
by `guardrail.max_denials_per_turn`.

Reviewer requests use the configured review model or the active turn model and
follow the same provider protocol, audit, retry, and floating-fallback policy as
other Mu requests. A user's later explicit approval is ordinary session history
that the next review may consider; there is no hidden approval state.

The guardrail reviews only calls declared destructive. It does not inspect or
constrain calls declared readonly or reversible and is not a sandbox.

### 12.2 Rejected and deferred safety designs

**Interactive approval for every action — rejected.** It would turn Mu into an
approval-driven UI, interrupt composition, and still rely on the model's action
description. The current guardrail denies into the transcript and lets the
agent ask for authorization when needed.

**Static Bash parsing to discover privileged or destructive commands —
rejected.** Bash permits variables, functions, aliases, sourced files, nested
interpreters, and generated commands. A partial parser would be complex and
would still miss the cases where confidence matters.

**A syscall-triggered seccomp reviewer — deferred, with no implementation
commitment.** It is attractive because review could occur immediately before a
watched effect, but it does not provide a coherent general Bash boundary:

- syscall names do not reliably classify intent, and important effects bypass
  any practical finite trigger list;
- unprivileged filter installation normally requires irreversible
  `no_new_privs`, breaking later setuid/file-capability elevation such as
  `sudo`;
- every descendant inheriting the filter depends on a live notification
  listener, including background and detached descendants that outlive a turn;
- nested Mu processes cannot independently stack notification listeners in the
  simple model;
- a persistent broker adds supervision and recovery, while a privileged broker
  creates a new high-value security boundary.

Approval cannot remove an inherited seccomp filter, and restarting a command
outside the filter after a trigger could repeat earlier side effects. These are
architectural conflicts, not missing edge-case patches.

**Special routing around seccomp limitations — not selected.** Detecting
`sudo`, `setsid`, or delegation textually is incomplete; adding a special Bash
field or fourth "risk" value would mix recoverability classification with
execution routing. A persistent or privileged broker would require a separate
product and threat model before reconsideration.

---
