#!/usr/bin/env zsh
set -eu

root=${0:A:h:h}
source "$root/mu.zsh"

fail() {
  print -u2 -- "FAIL: $*"
  exit 1
}

[[ "$MU_ZSH_MODE" == shell ]] || fail "starts in shell mode"

BUFFER="echo hello"
CURSOR=${#BUFFER}
PROMPT="%# "
RPROMPT="right"
_mu_zsh_enter_mode
[[ "$MU_ZSH_MODE" == mu ]] || fail "enters mu mode"
[[ "$BUFFER" == "" ]] || fail "clears buffer in mu mode"
[[ "$PROMPT" == "$MU_ZSH_PROMPT" ]] || fail "sets mu prompt"

_mu_zsh_exit_mode
[[ "$MU_ZSH_MODE" == shell ]] || fail "exits mu mode"
[[ "$BUFFER" == "echo hello" ]] || fail "restores shell buffer"
[[ "$PROMPT" == "%# " ]] || fail "restores prompt"
[[ "$RPROMPT" == "right" ]] || fail "restores right prompt"

BUFFER="draft prompt"
CURSOR=${#BUFFER}
_mu_zsh_clear_prompt
[[ "$BUFFER" == "" ]] || fail "clears prompt buffer"
[[ "$CURSOR" -eq 0 ]] || fail "resets prompt cursor"

BUFFER="draft prompt"
CURSOR=${#BUFFER}
PROMPT="%# "
RPROMPT="right"
KEYMAP=main
_mu_zsh_enter_mode
[[ "$MU_ZSH_SAVED_KEYMAP" == main ]] || fail "saves current keymap"
_mu_zsh_exit_mode
[[ "$MU_ZSH_MODE" == shell ]] || fail "ctrl-d path can exit mu mode"
[[ "$BUFFER" == "draft prompt" ]] || fail "ctrl-d path restores shell buffer"

MU_ZSH_ORIGINAL_TAB_WIDGET=
_mu_zsh_save_widget_bindings
[[ -n "$MU_ZSH_ORIGINAL_TAB_WIDGET" ]] || fail "saves tab widget fallback"

MU_ZSH_BIN=mu
MU_ZSH_OUTPUT=terminal
MU_ZSH_SESSION_ID=abc123
cmd=$(_mu_zsh_build_command)
[[ "$cmd" == "mu --output terminal -s abc123" ]] || fail "builds attached command: $cmd"

MU_ZSH_SESSION_ID=
cmd=$(_mu_zsh_build_command)
[[ "$cmd" == "mu --output terminal" ]] || fail "builds new-session command: $cmd"

quoted=$(_mu_zsh_quote_prompt $'quote " dollar $ backslash \\ newline\nnext')
eval "roundtrip=$quoted"
[[ "$roundtrip" == $'quote " dollar $ backslash \\ newline\nnext' ]] || fail "quoted prompt roundtrips"

tmp=${TMPDIR:-/tmp}/mu-zsh-test-${$}.session
print -r -- "session-from-file" > "$tmp"
MU_ZSH_SESSION_FILE=$tmp
MU_ZSH_SESSION_ID=
_mu_zsh_read_session_file
[[ "$MU_ZSH_SESSION_ID" == "session-from-file" ]] || fail "reads session file"
rm -f "$tmp"

fake_dir=${TMPDIR:-/tmp}/mu-zsh-test-bin-${$}
mkdir -p "$fake_dir"
cat > "$fake_dir/mu" <<'EOF'
#!/usr/bin/env zsh
print -r -- "$*" >> "$MU_ZSH_FAKE_LOG"
prompt=$(cat)
print -r -- "prompt=$prompt" >> "$MU_ZSH_FAKE_LOG"
if [[ -n "$MU_SESSION_FILE" ]]; then
  print -r -- "created-session" > "$MU_SESSION_FILE"
fi
EOF
chmod +x "$fake_dir/mu"

export MU_ZSH_FAKE_LOG=${TMPDIR:-/tmp}/mu-zsh-test-${$}.log
rm -f "$MU_ZSH_FAKE_LOG"
MU_ZSH_BIN=$fake_dir/mu
MU_ZSH_OUTPUT=plain
MU_ZSH_SESSION_FILE=${TMPDIR:-/tmp}/mu-zsh-test-submit-${$}.session
MU_ZSH_SESSION_ID=
_mu_zsh_submit_prompt "first prompt"
[[ "$MU_ZSH_SESSION_ID" == "created-session" ]] || fail "captures session id after first submit"

_mu_zsh_submit_prompt "second prompt"
grep -q -- "--output plain" "$MU_ZSH_FAKE_LOG" || fail "passes output mode"
grep -q -- "-s created-session" "$MU_ZSH_FAKE_LOG" || fail "passes session id on later submit"
grep -q -- "prompt=first prompt" "$MU_ZSH_FAKE_LOG" || fail "sends first prompt on stdin"
grep -q -- "prompt=second prompt" "$MU_ZSH_FAKE_LOG" || fail "sends second prompt on stdin"

rm -rf "$fake_dir" "$MU_ZSH_FAKE_LOG" "$MU_ZSH_SESSION_FILE"

for dependency in script timeout perl col cmp; do
  command -v "$dependency" >/dev/null || fail "missing test dependency: $dependency"
done

tmpdir=$(mktemp -d)
TRAPEXIT() {
  local exit_code=$?
  if (( ZSH_SUBSHELL == 0 )); then
    if (( exit_code )); then
      print -u2 -- "test artifacts: $tmpdir"
    else
      rm -rf -- "$tmpdir"
    fi
  fi
  return $exit_code
}

interactive_fake_bin=$tmpdir/bin
interactive_capture_args=$tmpdir/args
interactive_capture_stdin=$tmpdir/stdin
interactive_capture_calls=$tmpdir/calls
interactive_transcript=$tmpdir/transcript
mkdir -p -- "$interactive_fake_bin"

cat > "$interactive_fake_bin/mu" <<'EOF'
#!/bin/sh
printf x >> "$TEST_CAPTURE_CALLS"
printf '%s\n' "$@" > "$TEST_CAPTURE_ARGS"
cat > "$TEST_CAPTURE_STDIN"
if [ -n "$MU_SESSION_FILE" ]; then
  printf '%s\n' "created-session" > "$MU_SESSION_FILE"
fi
printf '%s\n\n' "Hello! I'm your terminal agent. How can I assist you today? Feel free to ask me to run commands, search files, read/write files, fetch web content, or perform other tasks."
EOF
chmod +x "$interactive_fake_bin/mu"

interactive_setup="PS1='> '; PATH=${(q)interactive_fake_bin}:\$PATH; export TEST_CAPTURE_ARGS=${(q)interactive_capture_args} TEST_CAPTURE_STDIN=${(q)interactive_capture_stdin} TEST_CAPTURE_CALLS=${(q)interactive_capture_calls}; source ${(q)root}/mu.zsh"
interactive_status=0
{
  print -r -- "$interactive_setup"
  sleep 0.2
  print -rn -- $'\t''hello'$'\r'
  sleep 0.4
  print -rn -- $'\x04'
  sleep 0.2
  print -rn -- 'echo shell-after'
  sleep 0.2
  print -rn -- $'\r'
  sleep 0.2
  print -rn -- 'exit'
  sleep 0.2
  print -rn -- $'\r'
} | timeout 5 script -qfec 'TERM=xterm-256color zsh -df' "$interactive_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "interactive transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$interactive_transcript" | col -b)
hello_count=0
for line in "${(@f)normalized}"; do
  [[ "$line" == 'mu> hello' ]] && (( hello_count += 1 ))
done
(( hello_count == 1 )) || fail "submitted prompt should appear once, saw $hello_count copies"
[[ "$normalized" != *$'mu>\nmu> hello'* ]] || fail "should not print a blank mu prompt before the submitted line"
[[ "$normalized" != *$'\nhello\n'* ]] || fail "submitted prompt should not be echoed again after the agent finishes"
[[ "$normalized" == *"Hello! I'm your terminal agent."* ]] || fail "interactive response should be rendered"
[[ "$normalized" == *'echo shell-after'* ]] || fail "Ctrl-D should return to the shell prompt"
[[ "$normalized" == *$'\nshell-after\n'* ]] || fail "shell command after Ctrl-D should run"
[[ $(<"$interactive_capture_calls") == x ]] || fail "interactive fake mu should run exactly once"

interactive_expected_stdin=$tmpdir/expected-stdin
print -rn -- 'hello'$'\n' > "$interactive_expected_stdin"
cmp -- "$interactive_expected_stdin" "$interactive_capture_stdin" || fail "interactive prompt should be passed on stdin"

interactive_args=("${(@f)$(<"$interactive_capture_args")}")
expected_interactive_args=(--output terminal)
[[ "${(j:\0:)interactive_args}" == "${(j:\0:)expected_interactive_args}" ]] || fail "unexpected interactive args: ${interactive_args[*]}"

print -- "ok"
