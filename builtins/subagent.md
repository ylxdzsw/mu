---
name: subagent
description: Delegate independent work by recursively invoking mu in a fresh session.
---

# Subagents

Use this when a task benefits from independent `mu` turns with narrower instructions:
broad reviews, parallel audits, focused investigation, or long-running async checks.

Subagents are ordinary `mu` processes. They run in fresh sessions by default.
Read [the Mu CLI reference](cli.md) before invoking one; this skill adds the
delegation-specific conventions.

Choose the output mode to suit the task:

- `-o final` (or `--output final`) prints only the final assistant message on
  success. Use it when only the result matters, keeping the parent context small.
- `-o concise` (or `--output concise`) streams assistant text, notices, and
  compact tool outcomes. Use it for long-running tasks where progress visibility
  matters. With async delegation, the caller can inspect the log while the
  subagent runs; `final` suppresses that progress output.

## Synchronous Delegation

Increase the outer bash timeout to at least 30 minutes; subagent calls usually need
longer than normal shell probes.

```ts
bash({
  title: "Delegate SPEC staleness review to subagent",
  risk: "readonly",
  command: "mu --output final",
  cwd: "/root/mu",
  timeout: 1800,
  stdin: `You are a focused mu subagent.

Task: Review SPEC.md for stale claims about the current CLI.
Scope: readonly. Inspect /root/mu only.
Do not delegate further.
Fail fast if blocked or uncertain; report the blocker instead of broadening scope.

Return:
- findings, if any
- key sources checked`
})
```

For readwrite delegation, explicitly name the writable scope and ask for an audit trail:

```ts
bash({
  title: "Run readwrite subagent",
  risk: "reversible",
  command: "mu --output final",
  cwd: "/root/mu",
  timeout: 1800,
  stdin: `You are a focused mu subagent.

Task: Apply the agreed README wording change.
Scope: readwrite, limited to README.md only.
Do not delegate further.
Fail fast if the requested edit does not fit the current file.

Return:
- all changes made
- checks run`
})
```

The parent should check the child exit status before trusting the answer.

## Asynchronous Delegation

Async delegation runs Mu as a background task. This example uses
`--output concise` so the caller can monitor progress. Create a session explicitly so it
can be steered later. Pass the prompt through the bash tool's `stdin` field and
launch it with:

```bash
session=$(mu new) || exit
log=$(mktemp "${TMPDIR:-/tmp}/mu-bg.XXXXXX")
setsid mu --session "$session" --output concise <&0 >"$log" 2>&1 & sid=$!
printf 'session=%s sid=%s start=%s log=%s\n' "$session" "$sid" "$(LC_ALL=C ps -o lstart= -p "$sid")" "$log"
```

The explicit `<&0` gives the background command the tool-provided stdin. Use
the `background-task` skill to inspect or stop it, then read the log after it
finishes. Its exit status is not retained. Files needed by the parent must
be saved at reported paths and inspected from a later foreground call.

While it runs, check progress in a later tool call using the recorded log path:

```bash
tail -n 200 /tmp/mu-bg.ABCDEF
```

### Common Operations

Substitute the recorded process SID, Mu session id, and log path in these
examples; shell variables do not persist between tool calls. Before signaling,
inspect the process and verify its PID equals SID, start time matches, and
command is expected:

```bash
LC_ALL=C ps -o pid=,sid=,lstart=,command= -p 12345
```

1. **Interruption:** send `SIGINT` to the Mu process to cancel active work.
   For session-wide cleanup or escalation, follow `background-task`.

   ```bash
   kill -INT 12345
   ```

2. **Steering:** interrupt the mu process, then run the same Mu
   session with a new prompt. Use the original working directory and explicit
   session id, not `--continue`, which another agent may have changed.

   To keep the steered turn asynchronous, reuse the launch recipe with the recorded
   session id instead of calling `mu new`.

3. **Wait:** poll the process in a Bash loop, with a timeout on the tool call.

   ```ts
   bash({
     title: "Wait for async subagent",
     risk: "readonly",
     command: "while kill -0 12345 2>/dev/null; do sleep 2; done; cat /tmp/mu-bg.ABCDEF",
     cwd: "/root/mu",
     timeout: 1800
   })
   ```

   A timeout stops only the polling call, not the detached subagent. Inspect
   it again before waiting longer or interrupting it. Process exit alone does
   not establish success; inspect the log and any reported artifacts.

## Parent Responsibilities

- Include enough context: subagents run in fresh sessions and do not see conversation history.
- Name allowed folders or files for editing when the scope is narrow.
- Ask readonly subagents to list key sources checked.
- Ask readwrite subagents to list all changes made and checks run.
- Verify important findings before editing or reporting them as certain.

`mu` propagates `MU_SUBAGENT_DEPTH` through bash tool calls and rejects recursive
grandchild turns, so subagents can use harmless management commands such as
`mu status` but cannot start further delegated agent turns.
