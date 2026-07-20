---
name: background-task
description: Start and stop long-running background services with systemd-run.
---

Use this when a long-running service must survive a normal `bash` tool call so
later foreground commands can test or inspect it, e.g., launching a dev server.

Normal `bash` commands are run in an isolated process group. On timeout,
interrupt, or main process exit, `mu` sends SIGTERM and then SIGKILL to that
group, so `server &` does not reliably keep a service alive.

Prefer a transient user service over `setsid` or other hacks:

```bash
id="mu-bg-$(date +%s)-$RANDOM"
unit="$id.service"
log="${TMPDIR:-/tmp}/$id.log"
: > "$log"
chmod 600 "$log"

systemd-run \
  --unit="$unit" \
  --property=Type=exec \
  --property=KillMode=control-group \
  --property=Restart=no \
  --property=StandardOutput=append:"$log" \
  --property=StandardError=append:"$log" \
  --property=WorkingDirectory="$(pwd)" \
  --expand-environment=no \
  -E MU_SUBAGENT_DEPTH="${MU_SUBAGENT_DEPTH:-0}" \
  bash -lc 'exec your-server-command-here'

invocation_id=$(systemctl show "$unit" -P InvocationID 2>/dev/null || true)
printf 'unit=%s\ninvocation_id=%s\nlog=%s\n' "$unit" "$invocation_id" "$log"
```

Run the service command in the foreground inside the unit. Do not append `&`.
The `MU_SUBAGENT_DEPTH` variable prevents infinite recursion of delegation when
running `mu` subagents. It is auto managed by `mu` in direct invocation but will
be cleared by systemd unless we pass it explicitly as in the example.

If the runtime user is not `root`, add `--user`.

Read logs through a normal foreground `bash` command such as:

```bash
tail -n 200 "$log"
```

Stop the service:

```bash
actual=$(systemctl show "$unit" -P InvocationID 2>/dev/null || true)
if [ "$actual" = "$invocation_id" ]; then
  systemctl stop "$unit"
else
  echo "service already disappeared"
fi
```

You should always cleanup services when no longer needed. They are not automatically killed by `mu`.
