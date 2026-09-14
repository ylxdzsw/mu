# Mu v0.3 for Windows MSYS2 UCRT64

`msys2-v0.3` is a Windows-only port based on master commit
`e4cd05df0d2060d7155577afc2d207a5e1589296`, not on the `v0.3` tag or the
historical `msys2` branch. Master remains the Unix implementation.

Each release gets a deliberate Windows port. There is no automatic rebase,
trial rebase, scheduled synchronization, or merge-back into master. Future
release ports start from an explicitly selected master revision and reuse
Windows code where appropriate.

## Supported environment

- Windows x86-64 with MSYS2 UCRT64 (`MSYSTEM=UCRT64`).
- Native `x86_64-pc-windows-gnu` Rust, built with MSYS2's UCRT64 toolchain.
- MSYS2 Bash for tools, zsh and jq for prompt mode.
- Windows Terminal/ConPTY for interactive native console output. Plain pipes
  and files retain noninteractive output behavior. Mintty-only native pipe
  interoperability is not an interactive support guarantee.
- Local NTFS storage. Installing Mu does not require symlink privileges;
  creating native symlinks in tests/workspaces requires Developer Mode or the
  appropriate Windows privilege.

PowerShell/cmd integration, Git Bash, Cygwin, WSL, other MSYS2 environments,
ARM64, and Unix builds of this branch are outside the supported target.

## Build and install

In an MSYS2 UCRT64 shell:

```sh
pacman -S --needed base-devel git zsh \
  mingw-w64-ucrt-x86_64-{rust,jq,python,ripgrep,curl}
cargo build --release --features portable
```

The portable `target/release/mu.exe` embeds built-ins and creates applet aliases
in the user cache. It still requires MSYS2; "portable" does not mean a bundled
Bash installation. Provider HTTPS uses Windows native TLS, not a separately
installed OpenSSL DLL.

To build the installable package from a checked-out revision:

```sh
export MU_SOURCE_URL="file://$(cygpath -m "$PWD")"
export MU_SOURCE_COMMIT="$(git rev-parse HEAD)"
cd packaging/msys2
makepkg --noconfirm
pacman -U mingw-w64-ucrt-x86_64-mu-*.pkg.tar.zst
```

The package installs `/ucrt64/bin/mu.exe`, executable applets under
`/ucrt64/libexec/mu`, built-ins under `/ucrt64/share/mu`, and zsh integration
at `/ucrt64/share/zsh/plugins/mu/mu.zsh`. Source the latter from `.zshrc`.
Global configuration remains in MSYS2 `$HOME/.mu`, or `MU_CONFIG_DIR`.

## Implementation contract

This branch uses Windows APIs directly rather than retaining Unix code behind
platform conditionals. The agent loop, provider protocols, tool schema,
configuration, JSONL v4 journals, retry, trapping, and compaction follow the
current Mu product semantics. SQLite sessions from the historical Windows
port are not imported or modified.

Rust consumes native Windows paths. Bash/model-visible paths use MSYS2 paths;
`cygpath` performs explicit boundary conversion, including paths in tool JSON
and patch stdin. Bash and cygpath belong to the same MSYS2 installation.

Every Bash call owns a kill-on-close Windows Job Object. Bash starts suspended
and joins the job before it can create descendants. Timeout and hard
cancellation terminate the job immediately. Remaining ordinary descendants
die when the call ends or Mu exits; there is no background escape or Unix
TERM/grace/KILL emulation. Nested synchronous Mu delegation uses nested jobs.
Large commands use private script files, leaving stdin available for literal
tool input.

Ctrl-C is a hard console interrupt. Ctrl-Break is soft when `soft_interrupt`
is enabled and hard otherwise; delivery depends on the terminal. Ctrl-\\ is
not a Windows substitute. Abrupt process termination can leave an interrupted
journal tail; `/retry` uses the normal conservative recovery rules.

Journal writer ownership is a Windows byte-range lock outside the data range,
so read-only status/transcript inspection remains possible. Locks are released
by handle lifetime, not PID leases. `current-session` is a regular pointer file
published atomically. Attachments use the current manifest/object-store
channel. Private runtime directories use Windows ACLs.

`http+unix` provider endpoints are rejected locally. `background-task` explains
the Windows lifetime restriction rather than recommending Unix detachment.
Fish and the root Arch Linux PKGBUILD are inherited reference files, not
Windows distribution surfaces.

## CI and releases

GitHub uses the workflow in the pushed revision. Consequently the same
`.github/workflows/ci.yml` path has independent implementations on the branches:

| Event | Result |
| --- | --- |
| Push/PR to master | Existing Unix checks on master's workflow |
| Push/PR to `msys2-v*` | UCRT64 build, tests, zsh checks, package and CLI smoke tests |
| Unix `v*` tag | Existing Unix release publication; no Windows dependency |

Windows CI checks out the triggering revision. It does not check out master,
rebase, merge, publish a release, or write repository contents. Successful
builds upload a SHA-labelled artifact containing the portable executable,
package, zsh integration, source commit and executable checksum. Artifacts are
retained for 30 days. A maintainer may download a successful build and attach
its assets to the existing Unix release manually. Never move the Unix release
tag to the Windows branch.

The workflow tests native and portable Rust builds, the zsh script, and
`test.windows.py` against both distributions. The CLI smoke test runs a local
mock provider and checks trap/retry, a command larger than Windows' command
line limit, literal stdin, Unicode/space paths, image attachments, journal
persistence and applets. Console UI behavior should additionally be exercised
in Windows Terminal before treating an interactive release as qualified.

References: [GitHub workflow triggers](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows),
[MSYS2 setup action](https://github.com/msys2/setup-msys2),
[MSYS2 path conversion](https://www.msys2.org/docs/filesystem-paths/).
