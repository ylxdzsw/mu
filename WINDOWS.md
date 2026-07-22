# Mu on Windows with MSYS2 UCRT64

This document defines the Windows port carried by the `msys2` branch. It is
both the implementation guide and the maintenance contract for keeping that
branch rebased on `master`.

The branch is intentionally **Windows/MSYS2/UCRT64-only**. It does not preserve
Unix build compatibility. The unpatched `master` branch remains the Unix
implementation.

Implementation status: the Windows-only source, package recipe, and compile-time
tests live on this branch. The master-owned cross-branch CI workflow is a
separate commit inherited through the branch base, not part of this patch
stack. Native runtime validation is performed by that UCRT64 CI job or on an
MSYS2 host; a Linux cross-check cannot exercise Job Objects, path conversion,
or zsh integration.

## 1. Supported environment

The supported configuration is deliberately narrow:

- Windows on x86-64.
- A native `x86_64-pc-windows-gnu` Rust binary built in the MSYS2 UCRT64
  environment.
- `MSYSTEM=UCRT64` and `MINGW_PREFIX=/ucrt64`.
- MSYS2 `bash` for every agent tool call.
- MSYS2 `zsh` and `jq` for the interactive shell integration.
- UCRT64 `ripgrep`, Python, and curl for the model-visible command baseline.
- The UCRT64 SQLite package for `rusqlite`.

`mu.exe` is a native Windows process. Bash and the commands it launches are
MSYS2 processes. The port must handle that boundary explicitly; the presence
of MSYS2 does not make Unix Rust APIs available to `mu.exe`.

The following are non-goals:

- PowerShell, `cmd.exe`, Git Bash, Cygwin, WSL, or a standalone MinGW install.
- The MSYS, MINGW64, CLANG64, CLANGARM64, or ARM64 MSYS2 environments.
- Compiling the `msys2` branch on Linux or another Unix system.
- Preserving Unix implementations with `cfg(unix)` branches.
- Producing one source tree that supports both `master` and Windows.

Unsupported environments should fail early with a concise diagnostic rather
than continue with partially compatible behavior.

## 2. Branch and patch contract

`master` owns the Unix product. `msys2` is a linear patch stack rebased on
`origin/master`:

```text
origin/master
    |
    +-- WINDOWS.md and Windows-only implementation commits (`msys2`)
```

Never merge `master` into `msys2`. Rebase the branch and update its remote with
`--force-with-lease`. Changes that are useful independent of Windows should be
landed on `master` first, then absorbed into the base by the next rebase. The
Windows branch should contain only its document, implementation, packaging,
and Windows-specific tests.

The preferred patch stack is:

1. Document the MSYS2 UCRT64 port.
2. Replace Unix process, path, identity, and artifact behavior with the native
   Windows implementation.
3. Add MSYS2 packaging and end-to-end tests.

Keep these commits logically coherent, but optimize for a small conflict
surface rather than preserving artificial subsystem boundaries. In particular,
do not add a cross-platform abstraction layer solely to retain code that this
branch cannot run.

## 3. Behavioral compatibility

The port should preserve Mu's product-level behavior where the platform permits
it:

- One native process per turn, with durable SQLite sessions.
- One model-visible `bash` tool using `bash -lc`.
- Isolated working directory, environment, timeout, and stdin per tool call.
- Concurrent execution of eligible readonly calls.
- Output streaming, truncation, redaction, rendering, and persistence.
- The `apply_patch`, `edit`, and `view_image` private applets.
- zsh prompt-mode behavior.

Platform mechanisms do not need to imitate Unix internally. Platform-specific
differences must be stated here and, where they alter the normative product
contract, in the relevant section of `SPEC.md`.

## 4. Process lifecycle

The Unix implementation in `src/bash.rs` uses process groups, POSIX signals,
`pre_exec`, and Linux parent-death signaling. The Windows branch replaces that
code directly with Windows Job Objects.

Each Bash tool call owns one Job Object with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Bash must be assigned to the job before it
can create an untracked descendant. The implementation therefore must use a
race-free creation mechanism, such as creating Bash suspended, assigning it to
the job, and only then resuming its primary thread. Bash also starts in a new
Windows process group so the console event is handled once by Mu; Mu then owns
termination through the Job Object.

Each concurrent Bash worker owns its Job Object. Shared cancellation state is
observed by every worker, so multiple concurrent readonly tool calls are
terminated independently without a global native-handle table.

The console control handler records Ctrl-C or termination in atomic state. The
normal tool polling loop observes that state, terminates each active job, drains
available output, and returns the same user-facing interruption error and exit
code `130` for Ctrl-C.

Windows does not have a reliable equivalent of sending SIGTERM to an arbitrary
MSYS2 process group and later escalating to SIGKILL. On this branch, timeout and
interruption terminate the Job Object immediately. Guaranteed descendant
cleanup is more important than emulating the Unix grace period.

Closing the last job handle must kill remaining descendants if `mu.exe` crashes
or exits unexpectedly.

Mu does not offer an in-tool escape from the Job Object. Persistent services
must be started and owned outside Mu; the Unix `background-task` built-in is
therefore not shipped on this branch.

## 5. Artifact transport

The Unix inherited file descriptor used by `view_image` is not part of the
Windows contract. Passing a native Windows handle through MSYS2 Bash and having
it become a chosen POSIX file descriptor is fragile and unnecessarily couples
Mu to MSYS2 internals.

Use a private artifact spool directory per Bash call instead:

1. Mu creates a uniquely named directory under the native temporary directory
   and passes it as `MU_ARTIFACT_DIR`.
2. `view_image.exe` writes the existing length-framed artifact record to a
   uniquely named temporary file in that directory.
3. The applet closes the file and atomically renames it to a committed suffix.
4. Mu reads committed records after Bash exits, applying the existing record,
   count, type, and size validation.
5. Partial temporary files are ignored. The whole spool is removed after the
   tool result has been collected, including error and cancellation paths.

Names should contain an ordering component plus a collision-resistant suffix so
sequential `view_image` calls retain their order. Concurrent applet completion
order may define the order for genuinely concurrent writers.

The environment variable must be absent outside a Mu Bash call, preserving the
current rule that `view_image` cannot inject an artifact when invoked directly.

## 6. Path boundary

Rust filesystem and process APIs consume native Windows paths. The agent and
commands running inside Bash consume MSYS2 POSIX paths. Mixing those dialects is
a correctness bug.

The branch uses these rules:

- Internal discovery, filesystem access, SQLite state, and child
  `current_dir` use canonical native Windows paths.
- Canonical paths are normalized out of the Win32 `\\?\` namespace before
  comparison or `cygpath` conversion; that prefix is an API detail, not part of
  Mu's project identity or model-visible path.
- Output captured from native UCRT64 tools such as `jq` is normalized from CRLF
  before zsh splits records or compares values.
- Project root and current working directory shown to the model use MSYS2 POSIX
  paths.
- A Bash tool's `cwd` accepts the POSIX paths previously shown to the model and
  is converted to a native path before spawning Bash.
- Command text remains POSIX shell text and is never rewritten as a Windows
  command line.
- `cygpath.exe` is the authoritative converter in both directions. Do not
  assume every MSYS2 mount can be represented by a `/c/...` transformation.
- Converted stable paths may be cached for one Mu invocation. Conversion
  failures must identify the original path.

Session storage may retain canonical native paths, but every path crossing into
the model-visible environment or zsh-visible status must use the POSIX form.

`HOME` from MSYS2 must be converted before native filesystem use. If it is not
set, the fallback is the Windows user profile rather than `/tmp`. Temporary
files use `std::env::temp_dir()`.

## 7. Shell and install layout

At startup, Mu verifies the UCRT64 environment and resolves both `bash.exe` and
`cygpath.exe`. Bash remains the only model-visible tool implementation and is
invoked with `-lc`.

Installed paths are derived from `current_exe()` and the UCRT64 prefix rather
than hardcoded Unix locations:

```text
/ucrt64/bin/mu.exe
/ucrt64/libexec/mu/apply_patch.exe
/ucrt64/libexec/mu/edit.exe
/ucrt64/libexec/mu/view_image.exe
/ucrt64/share/mu/*.md
```

The native equivalents are used for Rust filesystem access; the POSIX
equivalents are used when prepending the private applet directory to Bash's
`PATH`.

The three applets should be hardlinks to `mu.exe` when the package filesystem
supports them, with copies as the fallback. Applet dispatch must compare the
file stem so the mandatory `.exe` suffix does not change the command names
visible to the model.

## 8. Filesystem semantics

Windows replacement and symlink behavior differ from Unix and need explicit
handling in `apply_patch` and `edit`:

- Replacing an existing file must use a Windows replacement operation with
  atomic behavior on the same volume; `std::fs::rename` cannot be assumed to
  overwrite its destination.
- Add operations must retain their no-overwrite guarantee.
- Temporary files must be created beside their destination so replacement does
  not cross volumes.
- Native Windows symlinks are supported.
- MSYS2 emulated symlink files are not silently treated as ordinary files. The
  port should detect and reject them with guidance to enable native symlinks.
- Packaging must not depend on emulated symlinks; use applet hardlinks or
  copies.

Tests must cover replacement of an existing file, no-overwrite creation,
rollback behavior, native symlinks, read-only files, paths containing spaces,
and Unicode paths.

## 9. Identity and command discovery

Remove the Unix `geteuid` dependency. The runtime prompt should identify the
Windows user without inventing a Unix UID. The runtime block may show only the
username when no meaningful numeric identity exists.

Skill `requires_commands` checks must match commands that MSYS2 Bash can
actually execute. The native process receives MSYS2's converted Windows
`PATH`, so Mu searches those directories for the unsuffixed name and Windows
executable suffixes (`.exe`, `.com`, `.bat`, and `.cmd`). Skill metadata keeps
the suffix-free command name shown to the model. Unix executable-bit checks do
not apply on this branch.

The branch has no direct `libc` dependency. Windows API bindings use only the
feature groups required for processes, jobs, console control, handles, and
filesystem replacement. Provider HTTPS uses Windows SChannel through
`native-tls`; the branch does not build the Unix-oriented AWS-LC TLS backend.

## 10. Packaging

Keep the Arch Linux `PKGBUILD` on `master` unchanged. It is removed by the
Windows patch, which instead owns `packaging/msys2/PKGBUILD` and produces
`mingw-w64-ucrt-x86_64-mu`.

The recipe must:

- Build with the UCRT64 Rust toolchain.
- Depend on the UCRT64 SQLite library and the MSYS2 Bash/zsh/jq/ripgrep/Python/
  curl commands Mu exposes or invokes.
- Install the executable, applets, built-in skills, zsh integration, license,
  and documentation in their UCRT64/MSYS2 locations.
- Run unit tests and an installed-layout smoke test before packaging succeeds.

The package and dependency names follow the live MSYS2 UCRT64 repository
conventions; re-check them when maintaining the recipe.

## 11. Cross-branch CI

The workflow that validates both branches belongs on `master` as a separate
change. Once `msys2` rebases onto that commit, both branches carry the workflow
definition without adding it to the Windows patch range.

The CI pairing is intentionally branch-specific:

| Source | Runner | Purpose |
| --- | --- | --- |
| `master` | Ubuntu | Build and test the Unix implementation at `master`. |
| `msys2` patch rebased onto current `master` | Windows with MSYS2 UCRT64 | Verify that the maintained patch still applies and that the Windows port works. |

Do not build the Windows-only branch on Ubuntu and do not treat a successful
cross-compile as Windows validation.

On pushes to either `master` or `msys2`, and on manual runs, CI should:

1. Check out and test `master` on Ubuntu using its normal fmt, clippy, test,
   build, zsh, and package-recipe checks.
2. Fetch both branches in a disposable Windows checkout.
3. Check out `msys2` and rebase it onto the fetched `origin/master` inside the
   CI workspace. A conflict is a CI failure; CI never pushes the result.
4. Build and test that temporary rebased tip inside a real UCRT64 shell.

Testing a disposable rebase means a `master` push immediately detects both
source conflicts and semantic Windows regressions without asking automation to
rewrite the maintained branch. The branch must still be rebased and pushed by
a maintainer afterward.

The Windows job should include:

- Formatting and lint checks appropriate to the Windows-only source.
- Unit tests with one test thread where shared process state requires it.
- Release and debug builds.
- zsh syntax and integration tests.
- Installed-package smoke testing.
- A process-tree test proving that timeout and Ctrl-C remove a grandchild.
- An artifact round-trip through the installed `view_image` applet.
- Working-directory tests covering spaces, Unicode, and MSYS2/native
  conversion.

## 12. Rebase procedure

Before rebasing, require a clean worktree and retain the old tips for recovery
and range comparison. A typical maintenance run is:

```sh
git fetch origin
git switch msys2
old_master=$(git rev-parse origin/master)
old_msys2=$(git rev-parse msys2)
git branch "msys2-before-rebase-$(date +%Y%m%d)" "$old_msys2"
git rebase origin/master
git range-diff "$old_master..$old_msys2" "origin/master..msys2"
```

After the Linux and UCRT64 checks pass, update the remote branch with a precise
`--force-with-lease`. Never use an unconditional force push.

Enable Git rerere in this repository so repeated conflict resolutions can be
reused. Rebase after meaningful `master` changes, before publishing Windows
work, and periodically while the port is otherwise idle. Do not use an
unattended bot to resolve or push rebases.

During conflict resolution, compare behavior rather than mechanically choosing
one side. `src/bash.rs`, `src/paths.rs`, `src/system_prompt.rs`, `src/skills.rs`,
`src/artifact.rs`, `src/applets.rs`, `Cargo.toml`, `README.md`, and `SPEC.md` are
expected conflict hotspots.

## 13. Completion criteria

The first supported port is complete when all of the following work from an
installed UCRT64 package:

- One-shot turns and session continuation.
- Project discovery and global/project state.
- Bash commands with stdout, literal stdin, exit status, timeout, and redaction.
- Concurrent readonly tool calls.
- Ctrl-C and timeout cleanup of descendant processes.
- Parent-exit cleanup through Job Objects.
- `apply_patch`, `edit`, and `view_image` from the private applet directory.
- Built-in and user skill discovery, including command requirements.
- Attachments, provider requests, rendering, and SQLite persistence.
- zsh prompt mode, attachments, slash commands, and session continuity.
- Paths with spaces and non-ASCII characters.

At that point `README.md` must identify the branch as MSYS2 UCRT64-only, and
the platform-specific statements in `SPEC.md` must agree with this document.

## 14. Decision log

Update this section when a maintenance decision changes the port contract.

- The branch supports only native Windows GNU/UCRT64, not a Cygwin Rust target.
- The branch does not preserve Unix compilation.
- Bash remains the only agent shell; PowerShell is not added.
- Windows Job Objects replace Unix process groups.
- Persistent background services must be owned outside Mu.
- Windows cancellation prioritizes guaranteed tree cleanup over a SIGTERM-like
  grace period.
- A per-call spool directory replaces the inherited artifact descriptor.
- `cygpath` defines the native/MSYS2 path boundary.
- Install paths are executable-relative within the UCRT64 prefix.
- Applets use hardlinks or copies and dispatch from `.exe` file stems.
- Native symlinks are supported; MSYS2 emulated symlinks are rejected.
- Provider HTTPS uses Windows SChannel through `native-tls`.
- The cross-branch CI workflow lives on `master` and tests a disposable rebase;
  it never pushes the branch.
