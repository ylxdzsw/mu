---
name: background-task
description: Start, inspect, and stop a long-running command outside one bash call.
---

Use this when a command must keep running after its launching `bash` call returns.

```bash
log=$(mktemp "${TMPDIR:-/tmp}/mu-bg.XXXXXX")
setsid your-command </dev/null >"$log" 2>&1 & sid=$!
printf 'sid=%s start=%s log=%s\n' "$sid" "$(LC_ALL=C ps -o lstart= -p "$sid")" "$log"
```

Use plain `setsid`, never `setsid -f`: under Mu's non-interactive bash, `$!`
then remains the command's PID, process-group ID, and session ID. Keep the
command in the foreground of that session; it must not daemonize or call
`setsid` itself. CWD and environment are inherited normally.

An empty start time means launch was not confirmed. Later, inspect the recorded
SID immediately before acting and manually verify that PID equals SID, start
time matches, and the command is expected:

```bash
LC_ALL=C ps -o pid=,sid=,lstart=,command= -p 12345
tail -n 200 /tmp/mu-bg.ABCDEF
```

Stop every process still in the verified session with `pkill -TERM -s 12345`;
inspect again before escalating to `pkill -KILL -s 12345`. Remove the log when
it is no longer needed.

Redirect all three standard streams as shown. For fixed launch input, replace
`/dev/null` with an input file. This method reports running or gone, but cannot
recover the command's exit status. Background commands cannot return tool
attachments; save ordinary files and inspect them in a later foreground call.

This recipe requires a compatible `setsid` executable (normally util-linux on
Linux). If it is unavailable, report that background launch is unsupported
instead of improvising another process manager.
