---
name: subagent
description: Delegate independent work by recursively invoking mu in a fresh session.
---

# Subagents

Use this when a task benefits from independent `mu` turns with narrower instructions:
broad reviews, parallel audits, focused investigation, or long-running async checks.

Subagents are ordinary `mu` processes. They run in fresh sessions by default.

## Synchronous Delegation

Use `--output final` for normal subagent calls. It prints only the final
assistant message on success, which keeps the parent context small.

Increase the outer bash timeout; subagent calls usually need longer than normal shell probes.

```ts
bash({
  title: "Delegate SPEC staleness review to subagent",
  risk: "readonly",
  command: "mu --output final",
  cwd: "/root/mu",
  timeout: 600,
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
  timeout: 600,
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

Async delegation is a background task whose command is `mu --output final`.
Pass the prompt through the bash tool's `stdin` field and launch it with:

```bash
log=$(mktemp "${TMPDIR:-/tmp}/mu-bg.XXXXXX")
setsid mu --output final <&0 >"$log" 2>&1 & sid=$!
printf 'sid=%s start=%s log=%s\n' "$sid" "$(LC_ALL=C ps -o lstart= -p "$sid")" "$log"
```

The explicit `<&0` gives the background command the tool-provided stdin. Use
the `background-task` skill to inspect or stop it, then read the log after it
disappears. This is one-shot delegation: its exit status and Mu session id are
not retained. Files needed by the parent must be saved at reported paths and
inspected from a later foreground call.

## Parent Responsibilities

- Include enough context: subagents run in fresh sessions and do not see conversation history.
- Name allowed folders or files for editing when the scope is narrow.
- Ask readonly subagents to list key sources checked.
- Ask readwrite subagents to list all changes made and checks run.
- Verify important findings before editing or reporting them as certain.

`mu` propagates `MU_SUBAGENT_DEPTH` through bash tool calls and rejects recursive
grandchild turns, so subagents can use harmless management commands such as
`mu status` but cannot start further delegated agent turns.
