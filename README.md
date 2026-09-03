# mu

`mu` is a small, composable agent for the terminal: read one prompt from stdin, write outputs to stdout, then exit. All state
is persisted in append-only session logs.

**Actual CLI**&emsp; `mu` lives in your shell as a shell mode (currently supports `zsh` and `fish`). Press Tab to
switch between normal shell mode and `mu` mode. In `mu` mode, the prompt is send to `mu` instead of being interpreted
as shell commands. You can literally find all executed `mu` commands in `.zsh_history`, just like normal shell commands.

**Standard Shell Interaction**&emsp; Each prompt is executed by an ordinary `mu` process. All normal shell interactions just work:
Ctrl+Z to suspend, `fg` to resume, resizing without refreshing, scrollback with interleaved shell history and `mu` history, and
whatever cursor movement keybindings you configured for your shell. `mu` is in your shell, after all.

**Absolute Minimum Toolset**&emsp; `mu` comes with only one tool: `bash`. That's 75% fewer than
[pi](https://github.com/earendil-works/pi/tree/main/packages/coding-agent).

**"Code Mode"**&emsp; If you doubt how a single `bash` suffices: this idea is closely related to [code mode](https://blog.cloudflare.com/code-mode/).
The difference is that we choose `bash`, a language with battle-tested composability and conciseness, over `javascript`, and we use
POSIX tools, the common denominator across agents that all models are born to master, instead of a custom tool set. Since POSIX
commands need no teaching, the total system prompt of `mu` is < 1k tokens.

**"Applets"**&emsp; `apply_patch`, `edit`, and `view_image` are provided as "applets" (symlinks to `mu` that dispatch based on `argv[0]`), as
a workaround for reliable file editing without escaping (with the `stdin` argument of the `bash` tool) and taking multi-modal inputs.

**Advisory Risk Labeling**&emsp; The `bash` tool requires the model to label every invocation as `readonly`, `reversible`, or `destructive`.
It affects concurrent execution and retry (`readonly` calls are run in parallel and can auto retry), as well as trapping (`destructive` actions
pause the execution by default).

**Shebang Support**&emsp; Add `#!/usr/bin/env -S mu` to reusable prompt files to run them as regular commands. You can also specify the model,
pin to a particular session, etc.

**Skill System**&emsp; Skills are prompt files that auto loaded on demand by the agent, indicated by front matter. They can coexist
with shebang, allowing a prompt file both manually invocable and automatically loadable.

**Multi-provider**&emsp; Supports Chat Completion, Responses, and Anthropic Messages APIs, with optional automatic fallback.
Common quirks, like cache keys, opaque reasoning replay, context length errors, etc. are handled properly.

**Stateless**&emsp; `mu` processes are short-lived: they exit after processing one prompt. The shell tracks only a session id, as a plain string in
`$MU_SESSION_ID`. Memory leaks are eliminated entirely. `mu` also supports transparent `mu retry`, that resends a request to LLM without
an extra "continue" user message.

**Cache Friendly**&emsp; `mu` session history is strictly append-only. Refreshing of AGENTS.md or skill catalog only happens after compaction.

**Compaction**&emsp; `mu` has two compaction thresholds: It prefers compaction after a complete turn, triggered by the soft threshold, but can
also compacts during a turn, triggered by either the hard threshold or provider context length errors.

**Simple Installation**&emsp; `mu` is one single statically linked binary plus one single-file shell plugin. It can be droped anywhere and run.

## Quick start

On Linux x86-64, download the latest portable binary and put it on `PATH`:

```sh
mkdir -p "$HOME/.local/bin"
curl -fsSL https://github.com/ylxdzsw/mu/releases/latest/download/mu-linux-x86_64-musl -o "$HOME/.local/bin/mu"
chmod +x "$HOME/.local/bin/mu"
export PATH="$HOME/.local/bin:$PATH"
```

MacOS binaries are available at [releases](https://github.com/ylxdzsw/mu/releases).

Now ask it something:

```zsh
mu <<< 'Introduce yourself. Including CLI usage and configuration guide.'
```

Out of the box `mu` bundles a free model from [OpenCode Zen](https://opencode.ai/zen/), so you can try it
immediately without configuring anything.

The intended way to use `mu` is through the shell plugins. For zsh:

```zsh
source <(curl -fsSL https://github.com/ylxdzsw/mu/releases/latest/download/mu.zsh)
```

For Fish 4 or newer:

```fish
curl -fsSL https://github.com/ylxdzsw/mu/releases/latest/download/mu.fish | source
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
  working to stop at the next safe boundary (steering).

## Sharing mu's context with other agents

`mu context --export` emits `mu`'s context for a *foreign* agent: it includes a short preamble,
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

Run this command to get an up-to-date, complete, and example-rich guide:

```bash
mu <<< 'Find and read your source code on GitHub, then give me a complete guide for your usage, covering all aspects in detail, with plenty of examples.'
```
