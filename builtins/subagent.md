---
name: subagent
description: Delegate independent work by recursively invoking mu in a fresh session. Use for broad reviews, parallel audits, or async investigation with --output final or supervised --output plain.
---

# Subagents

Use this when a task benefits from one or more independent `mu` turns with
narrower instructions: broad reviews, parallel audits, focused investigation,
or long-running async checks.

Subagents are ordinary `mu` processes. They run in fresh sessions by default.

## Synchronous Delegation

Use `--output final` for normal subagent calls. It prints only the final
assistant message on success, which keeps the parent context small.

Increase the outer bash timeout; subagent calls usually need longer than normal
shell probes.

```bash
prompt='
You are a focused mu subagent.

Task: Review SPEC.md for stale claims about the current CLI.
Scope: readonly. Inspect SPEC.md, README.md, and relevant src/*.rs files only.
Do not delegate further.
Fail fast if blocked or uncertain; report the blocker instead of broadening scope.

Return:
- findings, if any
- key sources checked
'

mu --output final <<EOF
$prompt
EOF
```

For readwrite delegation, explicitly name the writable scope and ask for an
audit trail:

```bash
prompt='
You are a focused mu subagent.

Task: Apply the agreed README wording change.
Scope: readwrite, limited to README.md only.
Do not delegate further.
Fail fast if the requested edit does not fit the current file.

Return:
- all changes made
- checks run
'

mu --output final <<EOF
$prompt
EOF
```

The parent should check the child exit status before trusting the answer.

## Asynchronous Delegation

Use `--output plain` for async delegation so the child writes a readable
transcript. Do not use `cmd &` inside a normal bash tool call; ordinary bash
children are cleaned up with the tool process group. Use a transient user
service instead.

```bash
id="mu-subagent-$(date +%s)-$RANDOM"
unit="$id.service"
log="${TMPDIR:-/tmp}/$id.log"
prompt_file="${TMPDIR:-/tmp}/$id.prompt"
: > "$log"
chmod 600 "$log"

cat > "$prompt_file" <<'EOF'
You are a focused mu subagent.

Task: Audit the current checkout for duplicate tests.
Scope: readonly.
Do not delegate further.
Fail fast if the task becomes ambiguous.

Return:
- findings, if any
- key sources checked
EOF

systemd-run --user \
  --unit="$unit" \
  --collect \
  --property=Type=exec \
  --property=KillMode=control-group \
  --property=Restart=no \
  --property=StandardOutput=append:"$log" \
  --property=StandardError=append:"$log" \
  --property=WorkingDirectory="$(pwd)" \
  --setenv=MU_SUBAGENT_DEPTH="${MU_SUBAGENT_DEPTH:-1}" \
  --expand-environment=no \
  bash -lc 'exec mu --output plain "$1"' bash "$prompt_file"

invocation_id=$(systemctl --user show "$unit" -P InvocationID 2>/dev/null || true)
printf 'unit=%s\ninvocation_id=%s\nlog=%s\nprompt=%s\n' \
  "$unit" "$invocation_id" "$log" "$prompt_file"
```

Check progress later:

```bash
systemctl --user show "$unit" -P ActiveState -P SubState -P Result
tail -n 200 "$log"
```

Stop only the same unit activation:

```bash
actual=$(systemctl --user show "$unit" -P InvocationID 2>/dev/null || true)
if [ "$actual" = "$invocation_id" ]; then
  systemctl --user stop "$unit"
else
  echo "not stopping: unit missing or invocation changed"
fi
```

## Parent Responsibilities

- Choose `readonly` or `readwrite` scope explicitly.
- Name allowed folders or files when the scope is narrow.
- Ask readonly subagents to list key sources checked.
- Ask readwrite subagents to list all changes made and checks run.
- Verify important findings before editing or reporting them as certain.

`mu` propagates `MU_SUBAGENT_DEPTH` through bash tool calls and rejects recursive
grandchild turns, so subagents can use harmless management commands such as
`mu status` but should not start further delegated agent turns.
