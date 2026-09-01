# mu

`mu` is a small, composable agent for the terminal: read one prompt from stdin, write outputs to stdout, then exit. All state
are persisted in append-only session logs.

**Actual CLI**&emsp; `mu` lives in your shell as a shell mode (currently supports `zsh` and `fish`). Pressing Tab to
switch between normal shell mode and `mu` mode. In `mu` mode, the prompt are send to `mu` instead of being interpreted
as shell commands. You can literally find all executed `mu` commands in `.zsh_history`, just like normal shell commands.

**Standard Shell Interaction**&emsp; Each prompt is executed by an ordinary `mu` process. All normal shell interations just work:
Ctrl+Z to suspend, `fg` to resume, resizing without refreshing, scrollback with interleaved shell history and `mu` history, and
whatever cursor movement keybindings you configured for your shell. `mu` is in your shell, after all.

**Absolute Minimum Toolset**&emsp; `mu` comes with only one tool: `bash`. That's 75% less than
[pi](https://github.com/earendil-works/pi/tree/main/packages/coding-agent).

**"Coding Mode"**&emsp; If you doubt how a single `bash` suffice: this idea is closely related to [code mode](https://blog.cloudflare.com/code-mode/).
The difference is that we choose `bash`, a language that has stood 50 years test of composibility, over `javascript`, and we use
POSIX tools, the maximum common divisor across agents that all models born to master, instead of a custom tool set. Since POSIX
commands need no teaching, the total system prompt of `mu` is < 1k tokens.

**"Applets"**&emsp; `apply_patch`, `edit`, and `view_image` are provided as "applets" (symlinks to `mu` that dispatched with `argv[0]`), as
a workaround for reliable file editing without escaping (with the `stdin` argument of the `bash` tool) and taking multi-modal inputs.

**Advisory Risk Labeling**&emsp; The `bash` tool requires the model to label every invocation as `readonly`, `reversible`, or `destructive`.
It affects concurrent execution and retry (`readonly` calls are run in parallel and can auto retry), as well as trapping (`destructive` actions
pause the execution by default).

**Shebang Support**&emsp; Add `#!/usr/bin/env -S mu` to reusable prompt files to run them as regular commands. You can also specify the model,
pin on a particular session, etc.

**Skill System**&emsp; Skills are prompt files that automatically loaded on demand by the agent, indicated by a front-matter. It can co-exist
with shebang, allowing a prompt file both manually invocable and automatically loadable.

**Multi-provider**&emsp; Supports chat completion, response, and anthropic messages APIs, with optional automatic fallbacking.
Common quirks, like cache keys, opaque reasoning replay, context length errors, etc. are handled properly.

**Stateless**&emsp; `mu` processes are shortlived: they exit after processing one prompt. The shell tracks only a session id, as a plain string in
`$MU_SESSION_ID`. Memory leaking is completely eliminated. `mu` also supports transparent `mu retry`, that resends request to LLM without
an extra "continue" user message.

**Cache Frendly**&emsp; `mu` session history is strict append-only. Refreshing of AGENTS.md or skill catalog only happens after compaction.

**Compaction**&emsp; `mu` has two compaction thresholds: It prefers compaction after a complete turn, triggered by soft threshold, but can
also compacts during turn, triggered by either hard threshold or provider context length errors.

**Simple Installation**&emsp; `mu` is one single static-linked binary plus one single-file shell plugin. You can just drop it anywhere and run.

## Quick start

On Linux x86-64, download the latest portable binary and put it on `PATH`:

```sh
mkdir -p "$HOME/.local/bin"
curl https://github.com/ylxdzsw/mu/releases/latest/download/mu-linux-x86_64-musl -o "$HOME/.local/bin/mu"
chmod +x "$HOME/.local/bin/mu"
export PATH="$HOME/.local/bin:$PATH"
```

Now ask it something:

```sh
mu <<< 'Introduce yourself. Including CLI usage and configuration guide.'
```

Out of the box `mu` bundles a free model from [OpenCode Zen](https://opencode.ai/zen/), so you can try it
immediately without configuring anything.

The intended way to use `mu` is the shell plugins. For zsh:

```zsh
source <(curl -fsSL https://github.com/ylxdzsw/mu/releases/latest/download/mu.zsh)
```

For Fish 4 or newer:

```fish
source <(curl -fsSL https://github.com/ylxdzsw/mu/releases/latest/download/mu.fish)
```

At an empty shell prompt, press **Tab** to enter `mu>` mode, type a request, and press **Enter**:

```
mu> what changed in the last three commits?
```

Type `/` to list prompt-mode commands. The common ones:

- `/new` starts a new session while keeping the current model, trap level, and attachments.
- `/load [<session-id>]` renders an existing session's transcript and attaches
  the shell to it for later turns. Without an id, it loads the active scope's
  last selected session. It uses the current output density.
- `/model` selects a configured model for later turns in this shell scope.
  Use Tab to auto complete the provider, model name, and effort levels.
- `/trap <off|destructive|reversible|all>` persistently selects the Bash trap
  level for this shell scope. `/trap default` returns to configuration.
- `/attach <file>` adds an image or audio file to the next turn.
- `/retry` resumes a turn interrupted by Ctrl-C, a crash, a lost connection, or
  exhausted automatic resume attempts.
- `/compact` checkpoints a long session through a synthetic summary turn,
  optionally with a focus instruction.
- When `soft_interrupt` is enabled (the default), press **Ctrl+\\** while Mu is
  working to stop at next safe boundary (steering).

## Sharing mu's context with other agents

`mu context --export` emits `mu`'s context for a *foreign* agent: it include a short preamble,
the effective `AGENTS.md`, and `mu`'s skills. It can be given to other agents as instruction files.

For Codex, add a `SessionStart` hook at `~/.codex/hooks.json`:

```json
{
  "description": "Load mu instructions and skills.",
  "hooks": {
    "SessionStart": [
      {
        "matcher": "startup|compact",
        "hooks": [{ "type": "command", "command": "mu context --export", "timeout": 10, "statusMessage": "Loading mu context" }]
      }
    ]
  }
}
```

For Claude Code, add a `SessionStart` hook to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "mu context --export" }] }
    ]
  }
}
```

## Complete Guide

Run this command to get a up-to-date, complete, and example-rich guide:

```bash
mu <<< 'Find and read your source code on Github, then give me a complete guide for your usage, covering all aspects in detail, with plenty of examples.'
```
