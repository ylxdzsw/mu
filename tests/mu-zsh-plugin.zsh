#!/usr/bin/env zsh
set -eu

source ./shell-plugins/mu.zsh

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

print -- "ok"
