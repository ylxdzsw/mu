---
name: background-task
description: Start and stop long-running background services with systemd-run.
---

Use this when a long-running service must survive a normal `bash` tool call so
later foreground commands can test or inspect it.

Normal `bash` commands are run in an isolated process group. On timeout,
interrupt, or process exit cleanup, `mu` sends SIGTERM and then SIGKILL to that
group, so `server &` is not a reliable way to keep a service alive.

Prefer a transient user service:

```bash
id="mu-bg-$(date +%s)-$RANDOM"
unit="$id.service"
log="${TMPDIR:-/tmp}/$id.log"
: > "$log"
chmod 600 "$log"

systemd-run --user \
  --unit="$unit" \
  --collect \
  --property=Type=exec \
  --property=KillMode=control-group \
  --property=Restart=no \
  --property=StandardOutput=append:"$log" \
  --property=StandardError=append:"$log" \
  --property=WorkingDirectory="$(pwd)" \
  --expand-environment=no \
  bash -lc 'exec your-server-command-here'

invocation_id=$(systemctl --user show "$unit" -P InvocationID 2>/dev/null || true)
printf 'unit=%s\ninvocation_id=%s\nlog=%s\n' "$unit" "$invocation_id" "$log"
```

Keep the service command in the foreground inside the unit. Do not append `&`.

Read logs through a normal foreground `bash` command such as:

```bash
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

Background logs are raw local files. They may contain secrets at rest, but any
later foreground `bash` read still passes through `mu` output redaction before
the model sees it.
