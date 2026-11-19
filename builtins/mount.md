---
name: mount
description: Register a session-scoped executor wrapper and use it to run commands through Bash cwd prefixes.
---

# Executor mounts

A mount routes a Bash call through a local wrapper. It is not a filesystem
mount or a persistent shell: every call starts a new wrapper process, and the
destination interprets the command. Local Bash helpers such as `apply_patch`
and `view_image` are not installed on the destination by mounting it. To attach
an image, the local wrapper can save the returned bytes to a temporary file
and call local `view_image` before exiting. Don't forward Mu's attachment
paths or the local process environment wholesale to the destination.

A timeout stops the local wrapper; it doesn't prove the remote operation was
cancelled. Inspect remote state before repeating an interrupted command.

Register a local argv prefix from the session that will use it:

```sh
mu mount buildbox -- /absolute/path/to/wrapper fixed-argument
mu mount
```

When registration runs inside a Bash call, it is committed when that call
completes. A `mu mount` listing in that call shows the staged registration;
use the new mount in a later Bash call.

`mu mount` with no arguments lists that session's registered executors without
contacting them. A registration resolves the wrapper executable and fixes its
local working directory at registration time; later calls do not resolve a
relative wrapper path or cwd from the agent's current directory. The registered
argv is visible in the list and `mu status`, so keep credentials in the
wrapper's normal credential source, not its arguments.

To select a mount, set the Bash call's `cwd` to `ID:path`, for example
`buildbox:/srv/app`. Mu splits at the first colon and passes the remaining path
unchanged as the wrapper's penultimate argument. The command script is its
final argument, and the Bash call's `stdin` is forwarded literally. `buildbox:`
passes an empty path for the executor's default directory. The executor decides
how to interpret the path and command; Mu does not run mounted commands through
local Bash. An unknown ID is an error, not a local fallback.

Mounts are durable for one session across restarts and compaction. A fresh
session, including a subagent session, starts without its parent's mounts. If a
child needs one, pass the target and wrapper registration recipe, not just the
ID and not credentials. The mount inventory is not inserted into the system
prompt or compaction checkpoint; query it with `mu mount` or `mu status` when
needed.

## SSH host

This wrapper sends the cwd and multiline script as separately quoted arguments
to a remote Bash process. SSH's pseudo-terminal is disabled, so the remote
process receives the Bash call's stdin as data. The remote account must have a
POSIX-compatible login shell and Bash installed; configure SSH keys and host
verification through the usual SSH agent/configuration, not wrapper arguments.
For example, define an SSH config alias named `buildbox` with its hostname and
user, and verify that `ssh buildbox` connects before registering the mount.

Save this on the Mu host as `~/.local/bin/mu-ssh-mount`:

```python
#!/usr/bin/env python3
import os
import shlex
import sys

host, remote_cwd, script = sys.argv[1:]
remote_script = (
    'if [ -n "$1" ]; then cd -- "$1" || exit; fi; '
    'exec bash -c "$2"'
)
remote_command = (
    "exec bash -c "
    + shlex.quote(remote_script)
    + " -- "
    + shlex.quote(remote_cwd)
    + " "
    + shlex.quote(script)
)
os.execvp("ssh", ["ssh", "-T", "-o", "BatchMode=yes", "--", host, remote_command])
```

Make it executable, register the wrapper with an SSH config alias, and list the
mounts:

```sh
chmod 700 "$HOME/.local/bin/mu-ssh-mount"
mu mount buildbox -- "$HOME/.local/bin/mu-ssh-mount" buildbox
mu mount
```

For example, the following is one Bash tool call. Keep `cwd`, `command`, and
`stdin` as separate tool fields; do not paste them into a local `ssh` command:

```text
cwd: buildbox:/srv/app
command: |
  printf 'cwd=%s\n' "$PWD"
  IFS= read -r line
  printf 'stdin=%s\n' "$line"
stdin: |
  $HOME; "this is literal input"
```

The remote command prints its working directory and reads the literal input
line. The `$HOME` and semicolon in `stdin` are data, not shell syntax.

## systemd-nspawn container

Use `systemd-run` on the Mu host to start Bash in a local nspawn machine. The
`--pipe` option connects stdin/stdout/stderr directly; do not use `--pty` or
`-t`. `--expand-environment=no` prevents the service manager from expanding
`$` in the script before Bash receives it. This requires a running machine
managed by systemd and a host `systemd-run` with that option.

Save this on the Mu host as `~/.local/bin/mu-nspawn-mount`:

```bash
#!/usr/bin/env bash
set -euo pipefail

machine=$1
remote_cwd=$2
script=$3
args=("--machine=$machine" --pipe --wait --quiet --expand-environment=no)
if [[ -n $remote_cwd ]]; then
  args+=("--working-directory=$remote_cwd")
fi
exec systemd-run "${args[@]}" /bin/bash -c "$script"
```

Register it with the machine name and inspect the session's mounts:

```sh
chmod 700 "$HOME/.local/bin/mu-nspawn-mount"
mu mount devbox -- "$HOME/.local/bin/mu-nspawn-mount" devbox
mu mount
```

Use a container path in `cwd`; omit it with `devbox:` to leave the service's
default working directory in effect. This Bash call keeps the script, cwd, and
stdin separate:

```text
cwd: devbox:/srv/app
command: |
  printf 'cwd=%s\n' "$PWD"
  IFS= read -r line
  printf 'stdin=%s\n' "$line"
stdin: |
  $HOME; "this is literal input"
```

The container's Bash prints its working directory and reads stdin as data.
