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
CURSOR=0
PROMPT="%# "
RPROMPT="right"
_mu_zsh_enter_mode
[[ "$MU_ZSH_MODE" == mu ]] || fail "enters mu mode"
[[ "$BUFFER" == "echo hello" ]] || fail "preserves buffer in mu mode"
[[ "$CURSOR" -eq 0 ]] || fail "preserves cursor in mu mode"
[[ "$PROMPT" == "$MU_ZSH_PROMPT" ]] || fail "sets mu prompt"

BUFFER="edited in mu"
CURSOR=3
_mu_zsh_exit_mode
[[ "$MU_ZSH_MODE" == shell ]] || fail "exits mu mode"
[[ "$BUFFER" == "edited in mu" ]] || fail "preserves current buffer when exiting mu mode"
[[ "$CURSOR" -eq 3 ]] || fail "preserves current cursor when exiting mu mode"
[[ "$PROMPT" == "%# " ]] || fail "restores prompt"
[[ "$RPROMPT" == "right" ]] || fail "restores right prompt"

typeset -ga mu_test_hooks=()
_mu_zsh_test_enter_hook() {
  mu_test_hooks+=("enter:$MU_ZSH_MODE")
}
_mu_zsh_test_exit_hook() {
  mu_test_hooks+=("exit:$MU_ZSH_MODE")
}

MU_ZSH_ENTER_HOOKS=(_mu_zsh_test_enter_hook)
MU_ZSH_EXIT_HOOKS=(_mu_zsh_test_exit_hook)
ZSH_HIGHLIGHT_HIGHLIGHTERS=(main brackets)
BUFFER="hook prompt"
CURSOR=${#BUFFER}
PROMPT="%# "
RPROMPT="right"
_mu_zsh_enter_mode
[[ "${#ZSH_HIGHLIGHT_HIGHLIGHTERS[@]}" -eq 0 ]] || fail "disables syntax highlighters in mu mode"
[[ "${(j:,:)mu_test_hooks}" == "enter:mu" ]] || fail "runs enter hooks after switching modes"
_mu_zsh_exit_mode
[[ "${(j:,:)ZSH_HIGHLIGHT_HIGHLIGHTERS}" == "main,brackets" ]] || fail "restores syntax highlighters after exit"
[[ "${(j:,:)mu_test_hooks}" == "enter:mu,exit:shell" ]] || fail "runs exit hooks after restoring shell mode"
MU_ZSH_ENTER_HOOKS=()
MU_ZSH_EXIT_HOOKS=()

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
[[ "$MU_ZSH_MODE" == shell ]] || fail "mode exit path returns to shell"
[[ "$BUFFER" == "draft prompt" ]] || fail "mode exit path preserves shell buffer"

MU_ZSH_ORIGINAL_TAB_WIDGET=
_mu_zsh_save_widget_bindings
[[ -n "$MU_ZSH_ORIGINAL_TAB_WIDGET" ]] || fail "saves tab widget fallback"
up_key=$'\e[A'
down_key=$'\e[B'
[[ -n "${MU_ZSH_SHELL_UP_WIDGETS[$up_key]:-}" ]] || fail "saves up-arrow widget fallback"
[[ -n "${MU_ZSH_SHELL_DOWN_WIDGETS[$down_key]:-}" ]] || fail "saves down-arrow widget fallback"

MU_ZSH_HISTORY_BUFFER="draft"
MU_ZSH_HISTORY_CURSOR=2
MU_ZSH_HISTORY_HISTNO=7
_mu_zsh_clear_history_return
[[ -z "$MU_ZSH_HISTORY_BUFFER" ]] || fail "clears saved history return buffer"
[[ "$MU_ZSH_HISTORY_CURSOR" -eq 0 ]] || fail "clears saved history return cursor"
[[ "$MU_ZSH_HISTORY_HISTNO" -eq 0 ]] || fail "clears saved history return histno"

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

interactive_setup="PS1='> '; PATH=${(q)interactive_fake_bin}:\$PATH; export TEST_CAPTURE_ARGS=${(q)interactive_capture_args} TEST_CAPTURE_STDIN=${(q)interactive_capture_stdin} TEST_CAPTURE_CALLS=${(q)interactive_capture_calls}; source ${(q)root}/mu.zsh; bindkey -M mumode '^G' _mu_zsh_interrupt"

interactive_transcript=$tmpdir/transcript
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  print -r -- "$interactive_setup"
  sleep 0.2
  print -rn -- $'\t\r'
  sleep 0.2
  print -rn -- '   '$'\r'
  sleep 0.2
  print -rn -- 'cancel-me'$'\x07'
  sleep 0.4
  print -rn -- 'hello'$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 5 script -qfec 'TERM=xterm-256color zsh -df' "$interactive_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "interactive transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$interactive_transcript" | col -b)
hello_count=0
for line in "${(@f)normalized}"; do
  [[ "$line" == 'mu> hello' ]] && (( hello_count += 1 ))
done
(( hello_count == 1 )) || fail "submitted prompt should appear once, saw $hello_count copies"
[[ "$normalized" == *"Hello! I'm your terminal agent."* ]] || fail "interactive response should be rendered"
[[ "$normalized" == *'mu> cancel-me'* ]] || fail "Ctrl-C should leave the cancelled mu line in scrollback"
[[ $(<"$interactive_capture_calls") == x ]] || fail "interactive fake mu should run exactly once"

interactive_expected_stdin=$tmpdir/expected-stdin
print -rn -- 'hello'$'\n' > "$interactive_expected_stdin"
cmp -- "$interactive_expected_stdin" "$interactive_capture_stdin" || fail "interactive prompt should be passed on stdin"

interactive_args=("${(@f)$(<"$interactive_capture_args")}")
expected_interactive_args=(--output terminal)
[[ "${(j:\0:)interactive_args}" == "${(j:\0:)expected_interactive_args}" ]] || fail "unexpected interactive args: ${interactive_args[*]}"

toggle_transcript=$tmpdir/toggle-transcript
toggle_setup="$interactive_setup; _mu_test_tab_roundtrip() { BUFFER='echo toggled'; CURSOR=0; _mu_zsh_tab; _mu_zsh_tab; }; zle -N _mu_test_tab_roundtrip; bindkey '^T' _mu_test_tab_roundtrip"
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  print -r -- "$toggle_setup"
  sleep 0.2
  print -rn -- $'\x14\r'
  sleep 0.2
  print -rn -- 'exit'
  sleep 0.2
  print -rn -- $'\r'
} | timeout 5 script -qfec 'TERM=xterm-256color zsh -df' "$toggle_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "toggle transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$toggle_transcript" | col -b)
[[ "$normalized" == *$'\ntoggled\n'* ]] || fail "Tab at cursor start should preserve the buffer when returning to shell mode"
[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "Tab toggle transcript should not call fake mu"

history_return_replay=$tmpdir/history-return-replay
history_return_file=$tmpdir/history-return
history_return_transcript=$tmpdir/history-return-transcript
history_return_prompt='agent after history'
print -r -- "print -rn -- shell-history > ${(q)history_return_replay}" > "$history_return_file"
rm -f -- "$history_return_replay" "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"

history_return_setup=" setopt HIST_IGNORE_SPACE; PS1='> '; PATH=${(q)interactive_fake_bin}:\$PATH; export TEST_CAPTURE_ARGS=${(q)interactive_capture_args} TEST_CAPTURE_STDIN=${(q)interactive_capture_stdin} TEST_CAPTURE_CALLS=${(q)interactive_capture_calls}; HISTFILE=${(q)history_return_file}; HISTSIZE=100; SAVEHIST=100; fc -R ${(q)history_return_file}; source ${(q)root}/mu.zsh"
interactive_status=0
{
  print -r -- "$history_return_setup"
  sleep 0.2
  print -rn -- $'\t'"$history_return_prompt"$'\e[A\e[B\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 5 script -qfec 'TERM=xterm-256color zsh -df' "$history_return_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "history return transcript exited with status $interactive_status"
[[ ! -e "$history_return_replay" ]] || fail "history detour should not execute the recalled shell history entry"
[[ $(<"$interactive_capture_calls") == x ]] || fail "history detour should still submit exactly one mu prompt"

interactive_args=("${(@f)$(<"$interactive_capture_args")}")
[[ "${(j:\0:)interactive_args}" == "${(j:\0:)expected_interactive_args}" ]] || fail "unexpected history-detour args: ${interactive_args[*]}"
print -rn -- "$history_return_prompt"$'\n' > "$interactive_expected_stdin"
cmp -- "$interactive_expected_stdin" "$interactive_capture_stdin" || fail "history detour should restore the mu draft before submit"

print -- "ok"
