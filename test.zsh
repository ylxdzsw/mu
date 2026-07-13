#!/usr/bin/env zsh
set -eu

root=${0:A:h}
source "$root/mu.zsh"

fail() {
  print -u2 -- "FAIL: $*"
  exit 1
}

submitted_display_before_response() {
  local transcript=$1 stream
  stream=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$transcript" | col -b)
  REPLY=${stream%%"Hello! I'm your terminal agent."*}
}

assert_command_reply() {
  local label=$1
  shift
  local -a expected
  expected=("$@")
  if [[ "${(j:\0:)MU_ZSH_COMMAND_REPLY}" != "${(j:\0:)expected}" ]]; then
    fail "$label: ${(q)MU_ZSH_COMMAND_REPLY[@]}"
  fi
}

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

prompt_fake_bin=$tmpdir/prompt-bin
mkdir -p -- "$prompt_fake_bin"
export MU_ZSH_TEST_PROJECT_ROOT=$root
cat > "$prompt_fake_bin/mu" <<'EOF'
#!/usr/bin/env zsh
if [[ "$1" == "status" ]]; then
  model=prompt-test-model
  include_models=0
  include_commands=0
  while (( $# )); do
    case "$1" in
      --model)
        shift
        model=$1
        ;;
      --include-models)
        include_models=1
        ;;
      --include-commands)
        include_commands=1
        ;;
    esac
    shift
  done
  [[ "$model" == gpt ]] && model=openai/gpt
  [[ "$model" == invalid/* ]] && exit 1
  provider=${model%%/*}
  model_id=${model#*/}
  [[ "$provider" == "$model" ]] && provider=test
  model_json="\"model\":{\"provider_id\":\"$provider\",\"model_id\":\"$model_id\",\"effort\":null,\"canonical\":\"$model\"}"
  if (( include_models )); then
    print -r -- "{$model_json,\"context_percent\":25.0,\"project_root\":\"$MU_ZSH_TEST_PROJECT_ROOT\",\"available_models\":{\"providers\":[{\"id\":\"local\",\"models\":[{\"id\":\"local/solo\",\"model_id\":\"solo\",\"supported_efforts\":[\"max\"]},{\"id\":\"local/shared\",\"model_id\":\"shared\",\"supported_efforts\":[]}]},{\"id\":\"openai\",\"models\":[{\"id\":\"openai/gpt\",\"model_id\":\"gpt\",\"supported_efforts\":[\"low\",\"high\"]},{\"id\":\"openai/shared\",\"model_id\":\"shared\",\"supported_efforts\":[\"medium\"]}]}]}}"
  elif (( include_commands )); then
    print -r -- "{$model_json,\"context_percent\":25.0,\"project_root\":\"$MU_ZSH_TEST_PROJECT_ROOT\",\"commands\":[{\"name\":\"review.md\",\"path\":\"$MU_ZSH_TEST_PROJECT_ROOT/.mu/review.md\",\"scope\":\"project\"}]}"
  else
    print -r -- "{$model_json,\"context_percent\":25.0,\"project_root\":\"$MU_ZSH_TEST_PROJECT_ROOT\"}"
  fi
  exit 0
fi
if [[ "$1" == "--output" && "$3" == "review.md" ]]; then
  print -r -- "$*" >> "$MU_ZSH_FAKE_LOG"
  if [[ ! -t 0 ]]; then
    prompt=$(cat)
    [[ -n "$prompt" ]] && print -r -- "prompt=$prompt" >> "$MU_ZSH_FAKE_LOG"
  fi
  if [[ -n "$MU_SESSION_FILE" ]]; then
    print -r -- "created-session" > "$MU_SESSION_FILE"
  fi
  exit 0
fi
if [[ "$1" == "compact" ]]; then
  print -r -- "$*" >> "$MU_ZSH_FAKE_LOG"
  if [[ -p /dev/stdin ]]; then
    prompt=$(cat)
    [[ -n "$prompt" ]] && print -r -- "prompt=$prompt" >> "$MU_ZSH_FAKE_LOG"
  fi
  exit 0
fi
if [[ "$1" == "retry" ]]; then
  print -r -- "$*" >> "$MU_ZSH_FAKE_LOG"
  exit 0
fi
print -r -- "$*" >> "$MU_ZSH_FAKE_LOG"
prompt=$(cat)
print -r -- "prompt=$prompt" >> "$MU_ZSH_FAKE_LOG"
if [[ -n "$MU_SESSION_FILE" ]]; then
  print -r -- "created-session" > "$MU_SESSION_FILE"
fi
EOF
chmod +x "$prompt_fake_bin/mu"
MU_ZSH_BIN=$prompt_fake_bin/mu

[[ "$MU_ZSH_MODE" == shell ]] || fail "starts in shell mode"

BUFFER="echo hello"
CURSOR=0
PROMPT="%# "
RPROMPT="right"
_mu_zsh_enter_mode
[[ "$MU_ZSH_MODE" == mu ]] || fail "enters mu mode"
[[ "$BUFFER" == "echo hello" ]] || fail "preserves buffer in mu mode"
[[ "$CURSOR" -eq 0 ]] || fail "preserves cursor in mu mode"
escaped_pwd=${PWD//\%/%%}
expected_prompt="%F{45}prompt-test-model%f %F{244}25%%%f %F{39}${escaped_pwd}%f
mu> "
[[ "$PROMPT" == "$MU_ZSH_PROMPT" ]] || fail "sets mu prompt"
[[ "$PROMPT" == "$expected_prompt" ]] || fail "renders two-line mu prompt"

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

MU_ZSH_MODE=mu
BUFFER="first second"
CURSOR=5
_mu_zsh_insert_newline
[[ "$BUFFER" == $'first\n second' ]] || fail "Shift+Enter inserts a newline at the cursor"
[[ "$CURSOR" -eq 6 ]] || fail "Shift+Enter advances the cursor past the newline"

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

saved_pwd=$PWD
builtin cd "$root/src"
nested_prompt=$(_mu_zsh_build_mode_prompt)
builtin cd "$saved_pwd"
escaped_root=${root//\%/%%}
nested_pwd=$root/src
escaped_nested_pwd=${nested_pwd//\%/%%}
[[ "$nested_prompt" == *"%F{39}${escaped_nested_pwd}%f %F{245}(${escaped_root})%f"* ]] || fail "shows project root when cwd differs"

global_fake_bin=$tmpdir/global-bin
mkdir -p -- "$global_fake_bin"
cat > "$global_fake_bin/mu" <<'EOF'
#!/usr/bin/env zsh
if [[ "$1" == "status" ]]; then
  print -r -- '{"model":{"provider_id":"test","model_id":"global-model","effort":null,"canonical":"global-model"},"context_percent":5.0,"project_root":null}'
  exit 0
fi
exit 1
EOF
chmod +x "$global_fake_bin/mu"
global_pwd=$tmpdir/global-scope
mkdir -p -- "$global_pwd"
saved_pwd=$PWD
MU_ZSH_BIN=$global_fake_bin/mu
builtin cd "$global_pwd"
global_prompt=$(_mu_zsh_build_mode_prompt)
builtin cd "$saved_pwd"
MU_ZSH_BIN=$prompt_fake_bin/mu
escaped_global_pwd=${global_pwd//\%/%%}
[[ "$global_prompt" == *"%F{39}${escaped_global_pwd}%f %F{245}(global)%f"* ]] || fail "shows global marker outside project scope"

MU_ZSH_ORIGINAL_TAB_WIDGET=
MU_ZSH_ORIGINAL_SLASH_WIDGET=
_mu_zsh_save_widget_bindings
[[ -n "$MU_ZSH_ORIGINAL_TAB_WIDGET" ]] || fail "saves tab widget fallback"
[[ -n "$MU_ZSH_ORIGINAL_SLASH_WIDGET" ]] || fail "saves slash widget fallback"
scope_discovery_dir=$tmpdir/scope-discovery
mkdir -p -- "$scope_discovery_dir"
saved_pwd=$PWD
saved_home=${HOME:-}
HOME=$tmpdir
builtin cd "$scope_discovery_dir"
[[ "$(_mu_zsh_current_scope_key)" == "global" ]] || fail "starts uncached global"
mkdir -p -- .mu
[[ "$(_mu_zsh_current_scope_key)" == "project:$scope_discovery_dir" ]] || fail "scope detection refreshes project markers"
builtin cd "$saved_pwd"
HOME=$saved_home

MU_ZSH_BIN=mu
MU_ZSH_OUTPUT=terminal
MU_ZSH_SESSION_ID=abc123
MU_ZSH_SESSION_SCOPE=$(_mu_zsh_current_scope_key)
_mu_zsh_base_command_reply
assert_command_reply "builds attached command" mu --output terminal -s abc123

MU_ZSH_SESSION_ID=
MU_ZSH_SESSION_SCOPE=
_mu_zsh_base_command_reply
assert_command_reply "builds new-session command" mu --output terminal
MU_ZSH_BIN=$prompt_fake_bin/mu

MU_ZSH_MODEL=openai/gpt
MU_ZSH_MODEL_SCOPE=$(_mu_zsh_current_scope_key)
_mu_zsh_base_command_reply
assert_command_reply "builds pending-model command" "$prompt_fake_bin/mu" --output terminal --model openai/gpt
status_json=$(_mu_zsh_status_json)
[[ "$status_json" == *"\"canonical\":\"openai/gpt\""* ]] || fail "status uses pending model"
MU_ZSH_SESSION_ID=abc123
MU_ZSH_SESSION_SCOPE=$(_mu_zsh_current_scope_key)
_mu_zsh_base_command_reply
assert_command_reply "builds attached pending-model command" "$prompt_fake_bin/mu" --output terminal -s abc123 --model openai/gpt
_mu_zsh_clear_model_state
_mu_zsh_clear_session_state

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

export MU_ZSH_FAKE_LOG=${TMPDIR:-/tmp}/mu-zsh-test-${$}.log
rm -f "$MU_ZSH_FAKE_LOG"
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

rm -f "$MU_ZSH_FAKE_LOG" "$MU_ZSH_SESSION_FILE"

MU_ZSH_BIN=$prompt_fake_bin/mu
MU_ZSH_OUTPUT=plain
MU_ZSH_SESSION_ID=
MU_ZSH_SESSION_SCOPE=
command_candidates=("${(@f)$(_mu_zsh_slash_command_candidates)}")
[[ "${(j:,:)command_candidates}" == "/attach,/model,/review.md" ]] || fail "hides session commands without a valid session"
MU_ZSH_SESSION_ID=tracked-session
MU_ZSH_SESSION_SCOPE=$(_mu_zsh_current_scope_key)
command_candidates=("${(@f)$(_mu_zsh_slash_command_candidates)}")
[[ "${(j:,:)command_candidates}" == "/attach,/model,/new,/retry,/compact,/review.md" ]] || fail "shows session commands with a valid session: ${(j:,:)command_candidates}"
BUFFER="/ret"
CURSOR=${#BUFFER}
completion_candidates=("${(@f)$(_mu_zsh_completion_candidates)}")
[[ "${(j:,:)completion_candidates}" == "/attach,/model,/new,/retry,/compact,/review.md" ]] || fail "offers zsh the complete slash-command set: ${(j:,:)completion_candidates}"
BUFFER="/M"
CURSOR=${#BUFFER}
completion_candidates=("${(@f)$(_mu_zsh_completion_candidates)}")
[[ "${(j:,:)completion_candidates}" == "/attach,/model,/new,/retry,/compact,/review.md" ]] || fail "leaves case matching to zsh: ${(j:,:)completion_candidates}"
BUFFER="/unknown"
CURSOR=${#BUFFER}
completion_candidates=("${(@f)$(_mu_zsh_completion_candidates)}")
[[ "${(j:,:)completion_candidates}" == "/attach,/model,/new,/retry,/compact,/review.md" ]] || fail "keeps freeform slash input advisory: ${(j:,:)completion_candidates}"
_mu_zsh_is_known_slash_command /attach || fail "recognizes attach slash command"
_mu_zsh_is_known_slash_command /model || fail "recognizes built-in slash command"
_mu_zsh_is_known_slash_command /review.md || fail "recognizes custom slash command"
if _mu_zsh_is_known_slash_command /unknown; then
  fail "unknown slash text should not dispatch as a command"
fi

model_candidates=("${(@f)$(_mu_zsh_model_completion_candidates "")}")
[[ " ${(j: :)model_candidates} " == *" openai/gpt "* ]] || fail "offers provider-qualified model"
[[ " ${(j: :)model_candidates} " == *" gpt "* ]] || fail "offers unique unqualified model"
[[ " ${(j: :)model_candidates} " == *" local/solo "* ]] || fail "offers second provider-qualified model"
[[ " ${(j: :)model_candidates} " == *" solo "* ]] || fail "offers second unique unqualified model"
[[ " ${(j: :)model_candidates} " == *" openai/shared "* ]] || fail "offers ambiguous model qualified"
[[ " ${(j: :)model_candidates} " == *" local/shared "* ]] || fail "offers other ambiguous model qualified"
[[ " ${(j: :)model_candidates} " != *" shared "* ]] || fail "does not offer ambiguous unqualified model"
[[ " ${(j: :)model_candidates} " != *":low "* ]] || fail "does not show variants before colon"
model_candidates=("${(@f)$(_mu_zsh_model_completion_candidates "gpt")}")
[[ " ${(j: :)model_candidates} " == *" gpt "* ]] || fail "keeps all base models available for zsh matching"
[[ " ${(j: :)model_candidates} " != *":high "* ]] || fail "does not show variants until colon"
model_candidates=("${(@f)$(_mu_zsh_model_completion_candidates "gpt:")}")
[[ " ${(j: :)model_candidates} " == *" gpt:low "* ]] || fail "shows unqualified variants after colon"
[[ " ${(j: :)model_candidates} " == *" openai/gpt:high "* ]] || fail "shows provider-qualified variants after colon"
BUFFER="/model openai/gpt:h"
CURSOR=${#BUFFER}
completion_candidates=("${(@f)$(_mu_zsh_completion_candidates)}")
[[ " ${(j: :)completion_candidates} " == *" openai/gpt:high "* ]] || fail "offers model variants to zsh from the zle buffer"
[[ " ${(j: :)completion_candidates} " == *" local/solo:max "* ]] || fail "does not prefilter model variants in zsh"

attachment_one=$tmpdir/screenshot.png
attachment_two=$tmpdir/recording.wav
touch -- "$attachment_one" "$attachment_two"
MU_ZSH_PENDING_ATTACHMENTS=()
_mu_zsh_run_slash_command "/attach $attachment_one"
_mu_zsh_run_slash_command "/attach $attachment_two"
(( ${#MU_ZSH_PENDING_ATTACHMENTS[@]} == 2 )) || fail "attach slash command queues repeated files"
pending_prompt=$(_mu_zsh_build_mode_prompt)
[[ "$pending_prompt" == *'[2 attachments]'* ]] || fail "prompt shows pending attachment count"
_mu_zsh_run_slash_command "/model gpt"
(( ${#MU_ZSH_PENDING_ATTACHMENTS[@]} == 2 )) || fail "model command preserves pending attachments"
_mu_zsh_clear_model_state
rm -f "$MU_ZSH_FAKE_LOG"
_mu_zsh_submit_prompt "inspect these"
grep -Fq -- "-a $attachment_one -a $attachment_two" "$MU_ZSH_FAKE_LOG" || fail "prompt forwards every pending attachment"
(( ${#MU_ZSH_PENDING_ATTACHMENTS[@]} == 0 )) || fail "prompt consumes pending attachments"

_mu_zsh_run_slash_command "/attach $attachment_one"
rm -f "$MU_ZSH_FAKE_LOG"
_mu_zsh_run_slash_command "/review.md Inspect image"
grep -Fq -- "-a $attachment_one review.md" "$MU_ZSH_FAKE_LOG" || fail "custom command forwards pending attachments"
(( ${#MU_ZSH_PENDING_ATTACHMENTS[@]} == 0 )) || fail "custom command consumes pending attachments"

_mu_zsh_run_slash_command "/attach $attachment_one"
_mu_zsh_run_slash_command "/attach --clear"
(( ${#MU_ZSH_PENDING_ATTACHMENTS[@]} == 0 )) || fail "attach clear discards pending attachments"
if _mu_zsh_run_slash_command "/attach $tmpdir/missing.png"; then
  fail "attach should reject unreadable files"
fi

rm -f "$MU_ZSH_FAKE_LOG"
_mu_zsh_run_slash_command "/retry"
grep -q -- "retry -s tracked-session --output plain" "$MU_ZSH_FAKE_LOG" || fail "retry slash command targets tracked session"
rm -f "$MU_ZSH_FAKE_LOG"
_mu_zsh_run_slash_command "/compact"
grep -q -- "compact --session tracked-session" "$MU_ZSH_FAKE_LOG" || fail "compact slash command targets tracked session"
rm -f "$MU_ZSH_FAKE_LOG"
_mu_zsh_run_slash_command $'/compact Focus on authentication\nKeep concrete API shapes'
grep -q -- "compact --session tracked-session" "$MU_ZSH_FAKE_LOG" || fail "focused compact targets tracked session"
compact_prompt=$(cat "$MU_ZSH_FAKE_LOG")
[[ "$compact_prompt" == *$'prompt=Focus on authentication\nKeep concrete API shapes'* ]] || fail "focused compact pipes multiline instruction"
rm -f "$MU_ZSH_FAKE_LOG"
_mu_zsh_run_slash_command "/review.md"
grep -q -- "--output plain -s tracked-session review.md" "$MU_ZSH_FAKE_LOG" || fail "custom slash command targets tracked session"
rm -f "$MU_ZSH_FAKE_LOG"
_mu_zsh_run_slash_command "/review.md Focus on authentication"
grep -q -- "--output plain -s tracked-session review.md" "$MU_ZSH_FAKE_LOG" || fail "custom slash command keeps tracked session with instruction"
grep -Fxq -- "prompt=Focus on authentication" "$MU_ZSH_FAKE_LOG" || fail "custom slash command pipes instruction"
rm -f "$MU_ZSH_FAKE_LOG"
_mu_zsh_run_slash_command $'/review.md First line\nSecond line'
custom_prompt=$(cat "$MU_ZSH_FAKE_LOG")
[[ "$custom_prompt" == *$'prompt=First line\nSecond line'* ]] || fail "custom slash command preserves multiline instruction"
_mu_zsh_run_slash_command "/new"
[[ -z "$MU_ZSH_SESSION_ID" && -z "$MU_ZSH_SESSION_SCOPE" ]] || fail "new slash command lazily clears tracked session"
rm -f "$MU_ZSH_FAKE_LOG" "$MU_ZSH_SESSION_FILE"
_mu_zsh_run_slash_command "/review.md"
[[ "$MU_ZSH_SESSION_ID" == "created-session" ]] || fail "custom slash command captures new session id"
_mu_zsh_clear_session_state
rm -f "$MU_ZSH_FAKE_LOG" "$MU_ZSH_SESSION_FILE"
_mu_zsh_run_slash_command "/review.md Start a fresh session"
[[ "$MU_ZSH_SESSION_ID" == "created-session" ]] || fail "custom slash instruction captures new session id"
grep -Fxq -- "prompt=Start a fresh session" "$MU_ZSH_FAKE_LOG" || fail "fresh custom slash command pipes instruction"
_mu_zsh_clear_session_state
if _mu_zsh_run_slash_command "/retry"; then
  fail "retry without a valid tracked session should fail"
fi
if _mu_zsh_run_slash_command "/new extra"; then
  fail "new should reject arguments"
fi
if _mu_zsh_run_slash_command "/unknown"; then
  fail "unknown slash command should fail"
fi
_mu_zsh_run_slash_command "/model gpt"
[[ "$MU_ZSH_MODEL" == openai/gpt ]] || fail "model slash command records canonical model"
[[ "$MU_ZSH_MODEL_SCOPE" == "$(_mu_zsh_current_scope_key)" ]] || fail "model slash command records scope"
if _mu_zsh_run_slash_command "/model invalid/model"; then
  fail "model slash command should validate model refs"
fi
_mu_zsh_clear_model_state
_mu_zsh_clear_session_state
rm -f "$MU_ZSH_FAKE_LOG"

scope_root=$tmpdir/scope-projects
project_a=$scope_root/project-a
project_b=$scope_root/project-b
mkdir -p "$project_a/.mu" "$project_b/.mu" "$project_a/subdir" "$project_b/subdir"
scope_fake_bin=$tmpdir/scope-bin
mkdir -p -- "$scope_fake_bin"
cat > "$scope_fake_bin/mu" <<'EOF'
#!/usr/bin/env zsh
scope_root=$PWD
while [[ "$scope_root" != "/" && ! -d "$scope_root/.mu" && ! -e "$scope_root/.git" ]]; do
  scope_root=${scope_root:h}
done
scope_name=${scope_root:t}
if [[ "$1" == "status" ]]; then
  print -r -- "$*" >> "$MU_ZSH_SCOPE_LOG"
  print -r -- "{\"model\":{\"provider_id\":\"test\",\"model_id\":\"scope-model\",\"effort\":null,\"canonical\":\"scope-model\"},\"context_percent\":10.0,\"project_root\":\"$scope_root\"}"
  exit 0
fi
print -r -- "$PWD :: $*" >> "$MU_ZSH_SCOPE_LOG"
prompt=$(cat)
print -r -- "prompt=$prompt" >> "$MU_ZSH_SCOPE_LOG"
if [[ -n "$MU_SESSION_FILE" ]]; then
  print -r -- "session-$scope_name" > "$MU_SESSION_FILE"
fi
EOF
chmod +x "$scope_fake_bin/mu"
MU_ZSH_BIN=$scope_fake_bin/mu
MU_ZSH_OUTPUT=plain
MU_ZSH_SESSION_FILE=${TMPDIR:-/tmp}/mu-zsh-scope-submit-${$}.session
export MU_ZSH_SCOPE_LOG=${TMPDIR:-/tmp}/mu-zsh-scope-${$}.log
rm -f "$MU_ZSH_SCOPE_LOG" "$MU_ZSH_SESSION_FILE"
MU_ZSH_SESSION_ID=
MU_ZSH_SESSION_SCOPE=
MU_ZSH_EFFECTIVE_SESSION_ID=

saved_pwd=$PWD
builtin cd "$project_a/subdir"
_mu_zsh_submit_prompt "project a prompt"
[[ "$MU_ZSH_SESSION_ID" == "session-project-a" ]] || fail "creates a scoped session for the first project"

MU_ZSH_MODEL=model-for-a
MU_ZSH_MODEL_SCOPE=$(_mu_zsh_current_scope_key)

builtin cd "$project_b/subdir"
_mu_zsh_base_command_reply
assert_command_reply "does not reuse another project's session before submitting there" "$scope_fake_bin/mu" --output plain
: > "$MU_ZSH_SCOPE_LOG"
status_json=$(_mu_zsh_status_json)
[[ "$status_json" == *"\"project_root\":\"$project_b\""* ]] || fail "status follows the current project"
! grep -q -- "-s session-project-a" "$MU_ZSH_SCOPE_LOG" || fail "status should not attach the first project's session in a different project"

builtin cd "$project_a/subdir"
_mu_zsh_base_command_reply
assert_command_reply "returns to the original scoped session and model after cd-ing back" "$scope_fake_bin/mu" --output plain -s session-project-a --model model-for-a

builtin cd "$project_b/subdir"
_mu_zsh_submit_prompt "project b prompt"
[[ "$MU_ZSH_SESSION_ID" == "session-project-b" ]] || fail "creates a new scoped session after submitting in the second project"
[[ "$MU_ZSH_SESSION_SCOPE" == "project:$project_b" ]] || fail "moves the tracked session scope after starting in the second project"
[[ -z "$MU_ZSH_MODEL" && -z "$MU_ZSH_MODEL_SCOPE" ]] || fail "forgets pending model after submitting in another project"

builtin cd "$project_a/subdir"
_mu_zsh_base_command_reply
assert_command_reply "forgets the first project's session once a new one starts elsewhere" "$scope_fake_bin/mu" --output plain

builtin cd "$saved_pwd"
MU_ZSH_BIN=$prompt_fake_bin/mu
rm -f "$MU_ZSH_SCOPE_LOG" "$MU_ZSH_SESSION_FILE"

for dependency in script timeout perl col cmp jq; do
  command -v "$dependency" >/dev/null || fail "missing test dependency: $dependency"
done

interactive_fake_bin=$tmpdir/bin
interactive_capture_args=$tmpdir/args
interactive_capture_stdin=$tmpdir/stdin
interactive_capture_calls=$tmpdir/calls
mkdir -p -- "$interactive_fake_bin"

cat > "$interactive_fake_bin/mu" <<'EOF'
#!/bin/sh
if [ "$1" = "status" ]; then
  model=prompt-test-model
  include_commands=0
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --model)
        shift
        model=$1
        ;;
      --include-commands)
        include_commands=1
        ;;
    esac
    shift
  done
  [ "$model" = gpt ] && model=openai/gpt
  provider=${model%%/*}
  model_id=${model#*/}
  [ "$provider" = "$model" ] && provider=test
  model_json="\"model\":{\"provider_id\":\"$provider\",\"model_id\":\"$model_id\",\"effort\":null,\"canonical\":\"$model\"}"
  if [ "$include_commands" -eq 1 ] && [ -n "$TEST_EXTRA_COMMAND" ]; then
    printf '%s\n' "{$model_json,\"context_percent\":25.0,\"project_root\":\"$MU_ZSH_TEST_PROJECT_ROOT\",\"commands\":[{\"name\":\"$TEST_EXTRA_COMMAND\",\"path\":\"$MU_ZSH_TEST_PROJECT_ROOT/.mu/$TEST_EXTRA_COMMAND\",\"scope\":\"project\"}]}"
  else
    printf '%s\n' "{$model_json,\"context_percent\":25.0,\"project_root\":\"$MU_ZSH_TEST_PROJECT_ROOT\"}"
  fi
  exit 0
fi
printf x >> "$TEST_CAPTURE_CALLS"
printf '%s\n' "$@" > "$TEST_CAPTURE_ARGS"
cat > "$TEST_CAPTURE_STDIN"
if [ -n "$MU_SESSION_FILE" ]; then
  printf '%s\n' "created-session" > "$MU_SESSION_FILE"
fi
printf '%s\n\n' "Hello! I'm your terminal agent."
printf '%s\n\n' "[mu] tokens: 12 in / 5 out  context: 25%" >&2
EOF
chmod +x "$interactive_fake_bin/mu"

interactive_setup="PS1='> '; PATH=${(q)interactive_fake_bin}:\$PATH; export TEST_CAPTURE_ARGS=${(q)interactive_capture_args} TEST_CAPTURE_STDIN=${(q)interactive_capture_stdin} TEST_CAPTURE_CALLS=${(q)interactive_capture_calls}; autoload -Uz compinit; compinit -D; source ${(q)root}/mu.zsh; bindkey -M mumode '^G' _mu_zsh_interrupt"

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
submitted_display_before_response "$interactive_transcript"
expected_submitted_display="prompt-test-model 25% $root"$'\nmu> hello\n'
[[ "$REPLY" == *"$expected_submitted_display"* ]] || fail "submitted prompt should remain complete in terminal scrollback"
after_submitted_display=${REPLY#*"$expected_submitted_display"}
[[ "$after_submitted_display" != *"$expected_submitted_display"* ]] || fail "submitted prompt should be committed exactly once"
[[ "$normalized" == *"Hello! I'm your terminal agent."* ]] || fail "interactive response should be rendered"
after_submitted_prompt=${normalized##*$'mu> hello\n'}
[[ "$after_submitted_prompt" == $'\nHello! I'* ]] || fail "submitted prompt should have one empty line before terminal output"
[[ "$after_submitted_prompt" != $'\n\nHello! I'* ]] || fail "submitted prompt should not have two empty lines before terminal output"
[[ "$normalized" == *'mu> cancel-me'* ]] || fail "Ctrl-C should leave the cancelled mu line in scrollback"
[[ $(<"$interactive_capture_calls") == x ]] || fail "interactive fake mu should run exactly once"
after_response=${normalized#*"Hello! I'm your terminal agent."}
[[ "$after_response" == $'\n\n[mu] tokens: 12 in / 5 out  context: 25%\n\n'* ]] || fail "token summary should be a separate block after assistant output"
[[ "$after_response" == *$'[mu] tokens: 12 in / 5 out  context: 25%\n\n'* ]] || fail "token summary should own one trailing empty line"
[[ "$after_response" != *$'[mu] tokens: 12 in / 5 out  context: 25%\n\n\n'* ]] || fail "token summary should not leave two trailing empty lines"
post_turn_prompt_count=0
native_exit_count=0
for line in "${(@f)after_response}"; do
  [[ "$line" == 'mu>' || "$line" == 'mu> ' ]] && (( post_turn_prompt_count += 1 ))
  [[ "$line" == 'exit' ]] && (( native_exit_count += 1 ))
done
(( post_turn_prompt_count == 1 )) || fail "post-turn mu prompt should appear once, saw $post_turn_prompt_count copies"
(( native_exit_count == 0 )) || fail "Ctrl-D should not synthesize a visible exit command"

interactive_expected_stdin=$tmpdir/expected-stdin
print -rn -- 'hello'$'\n' > "$interactive_expected_stdin"
cmp -- "$interactive_expected_stdin" "$interactive_capture_stdin" || fail "interactive prompt should be passed on stdin"

interactive_args=("${(@f)$(<"$interactive_capture_args")}")
expected_interactive_args=(--output terminal)
[[ "${(j:\0:)interactive_args}" == "${(j:\0:)expected_interactive_args}" ]] || fail "unexpected interactive args: ${interactive_args[*]}"

shift_enter_transcript=$tmpdir/shift-enter-transcript
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  print -r -- "$interactive_setup"
  sleep 0.2
  print -rn -- $'\t'"first line"$'\e[13;2u'"second line"$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 5 script -qfec 'TERM=xterm-256color zsh -df' "$shift_enter_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "Shift+Enter transcript exited with status $interactive_status"
[[ $(<"$interactive_capture_calls") == x ]] || fail "Shift+Enter should not submit before Enter"
submitted_display_before_response "$shift_enter_transcript"
expected_submitted_display="prompt-test-model 25% $root"$'\nmu> first line\nsecond line\n'
[[ "$REPLY" == *"$expected_submitted_display"* ]] || fail "multiline submitted prompt should remain complete in terminal scrollback"
shift_enter_expected_stdin=$tmpdir/shift-enter-expected-stdin
print -rn -- 'first line'$'\n''second line'$'\n' > "$shift_enter_expected_stdin"
cmp -- "$shift_enter_expected_stdin" "$interactive_capture_stdin" || fail "Shift+Enter draft should be passed as one multiline prompt"

wrapped_transcript=$tmpdir/wrapped-transcript
wrapped_prompt=
wrapped_prompt=${(l:120::x:)wrapped_prompt}
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  print -r -- "$interactive_setup"
  sleep 0.2
  print -rn -- $'\t'"$wrapped_prompt"$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 5 script -qfec 'TERM=xterm-256color zsh -df' "$wrapped_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "wrapped prompt transcript exited with status $interactive_status"
submitted_display_before_response "$wrapped_transcript"
wrapped_expected_stdin=$tmpdir/wrapped-expected-stdin
print -rn -- "$wrapped_prompt"$'\n' > "$wrapped_expected_stdin"
cmp -- "$wrapped_expected_stdin" "$interactive_capture_stdin" || fail "wrapped prompt should be passed on stdin"

custom_slash_transcript=$tmpdir/custom-slash-transcript
custom_slash_setup="$interactive_setup; export TEST_EXTRA_COMMAND=review.md"
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  print -r -- "$custom_slash_setup"
  sleep 0.2
  print -rn -- $'\t'"/review.md First line"$'\e[13;2u'"Second line"$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 5 script -qfec 'TERM=xterm-256color zsh -df' "$custom_slash_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "custom slash transcript exited with status $interactive_status"
[[ $(<"$interactive_capture_calls") == x ]] || fail "custom slash command should run once"
custom_slash_expected_stdin=$tmpdir/custom-slash-expected-stdin
print -rn -- 'First line'$'\n''Second line' > "$custom_slash_expected_stdin"
cmp -- "$custom_slash_expected_stdin" "$interactive_capture_stdin" || fail "custom slash instruction should preserve multiline text"
interactive_args=("${(@f)$(<"$interactive_capture_args")}")
expected_custom_slash_args=(--output terminal review.md)
[[ "${(j:\0:)interactive_args}" == "${(j:\0:)expected_custom_slash_args}" ]] || fail "custom slash command should use the command path"

plain_transcript=$tmpdir/plain-transcript
plain_setup="$interactive_setup; MU_ZSH_OUTPUT=plain"
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  print -r -- "$plain_setup"
  sleep 0.2
  print -rn -- $'\t'"plain prompt"$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 5 script -qfec 'TERM=xterm-256color zsh -df' "$plain_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "plain transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$plain_transcript" | col -b)
after_submitted_prompt=${normalized##*$'mu> plain prompt\n'}
[[ "$after_submitted_prompt" == $'\nHello! I'* ]] || fail "submitted prompt should have one empty line before plain output"
[[ "$after_submitted_prompt" != $'\n\nHello! I'* ]] || fail "submitted prompt should not have two empty lines before plain output"
after_response=${normalized#*"Hello! I'm your terminal agent."}
[[ "$after_response" == $'\n\n[mu] tokens: 12 in / 5 out  context: 25%\n\n'* ]] || fail "plain token summary should be a separate block after assistant output"
[[ "$after_response" == *$'[mu] tokens: 12 in / 5 out  context: 25%\n\n'* ]] || fail "plain token summary should own one trailing empty line"
[[ "$after_response" != *$'[mu] tokens: 12 in / 5 out  context: 25%\n\n\n'* ]] || fail "plain token summary should not leave two trailing empty lines"
interactive_args=("${(@f)$(<"$interactive_capture_args")}")
expected_plain_args=(--output plain)
[[ "${(j:\0:)interactive_args}" == "${(j:\0:)expected_plain_args}" ]] || fail "unexpected plain interactive args: ${interactive_args[*]}"

model_switch_transcript=$tmpdir/model-switch-transcript
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  print -r -- "$interactive_setup"
  sleep 0.2
  print -rn -- $'\t'"/model gpt"$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 5 script -qfec 'TERM=xterm-256color zsh -df' "$model_switch_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "model switch transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$model_switch_transcript" | col -b)
[[ "$normalized" == *$'[mu] next turns in this scope will use openai/gpt\n'* ]] || fail "model slash command should confirm the canonical model"
after_model_switch=${normalized#*$'[mu] next turns in this scope will use openai/gpt\n'}
[[ "$after_model_switch" == $'\n'* ]] || fail "model slash command should leave an empty line before the next prompt"
[[ "$after_model_switch" != $'\n\n'* ]] || fail "model slash command should not leave two empty lines before the next prompt"
[[ "$after_model_switch" == *$'openai/gpt 25%'* ]] || fail "model slash command should redraw prompt with selected model"
[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "model slash command should not submit a prompt"

new_session_transcript=$tmpdir/new-session-transcript
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
new_session_setup="$interactive_setup; MU_ZSH_SESSION_ID=tracked-session; MU_ZSH_SESSION_SCOPE=\$(_mu_zsh_current_scope_key); _mu_zsh_sync_state"
{
  print -r -- "$new_session_setup"
  sleep 0.2
  print -rn -- $'\t'"/new"$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 5 script -qfec 'TERM=xterm-256color zsh -df' "$new_session_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "new session transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$new_session_transcript" | col -b)
[[ "$normalized" == *$'[mu] next turn will start a new session\n'* ]] || fail "new slash command should confirm the next turn starts fresh"
after_new_session=${normalized#*$'[mu] next turn will start a new session\n'}
[[ "$after_new_session" == $'\n'* ]] || fail "new slash command should leave an empty line before the next prompt"
[[ "$after_new_session" != $'\n\n'* ]] || fail "new slash command should not leave two empty lines before the next prompt"
[[ "$after_new_session" == *$'prompt-test-model 25%'* ]] || fail "new slash command should redraw prompt"
[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "new slash command should not submit a prompt"

slash_listing_transcript=$tmpdir/slash-listing-transcript
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  print -r -- "$interactive_setup"
  sleep 0.2
  print -rn -- $'\t/\x07'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 5 script -qfec 'TERM=xterm-256color zsh -df' "$slash_listing_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "slash listing transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$slash_listing_transcript" | col -b)
[[ "$normalized" == *'/model'* ]] || fail "typing slash should proactively list completion candidates"

slash_completion_transcript=$tmpdir/slash-completion-transcript
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  print -r -- "$interactive_setup; zstyle ':completion:*' matcher-list 'm:{a-zA-Z}={A-Za-z}'"
  sleep 0.2
  print -rn -- $'\t'"/MO"$'\t\t'"gpt"$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 5 script -qfec 'TERM=xterm-256color zsh -df' "$slash_completion_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "slash completion transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$slash_completion_transcript" | col -b)
[[ "$normalized" == *$'[mu] next turns in this scope will use openai/gpt\n'* ]] || fail "Tab should use zsh matcher rules to complete /MO to /model"
[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "completed model slash command should not submit a prompt"

common_prefix_transcript=$tmpdir/common-prefix-transcript
common_prefix_setup="$interactive_setup; export TEST_EXTRA_COMMAND=model-helper.md; _mu_test_common_prefix_completion() { BUFFER='/mod'; CURSOR=\${#BUFFER}; _mu_zsh_complete_slash; zle -I; print -r -- \"[completion-buffer=\$BUFFER cursor=\$CURSOR]\"; _mu_zsh_clear_prompt; _mu_zsh_reset_mode_prompt; }; zle -N _mu_test_common_prefix_completion; bindkey -M mumode '^T' _mu_test_common_prefix_completion"
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  print -r -- "$common_prefix_setup"
  sleep 0.2
  print -rn -- $'\t\x14'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 5 script -qfec 'TERM=xterm-256color zsh -df' "$common_prefix_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "common-prefix completion transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$common_prefix_transcript" | col -b)
[[ "$normalized" == *'[completion-buffer=/model cursor=6]'* ]] || fail "common-prefix completion should not add a suffix space"
[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "common-prefix completion should not submit a prompt"

delete_slash_transcript=$tmpdir/delete-slash-transcript
delete_slash_setup="$interactive_setup; _mu_test_delete_slash_completion() { BUFFER='/'; CURSOR=1; _mu_zsh_list_slash_choices; zle backward-delete-char; if _mu_zsh_slash_completion_context; then back_state=active; else back_state=inactive; fi; back_buffer=\$BUFFER; back_cursor=\$CURSOR; BUFFER='/'; CURSOR=0; _mu_zsh_list_slash_choices; zle delete-char; if _mu_zsh_slash_completion_context; then forward_state=active; else forward_state=inactive; fi; zle -I; print -r -- \"[back-buffer=\$back_buffer back-cursor=\$back_cursor back-context=\$back_state forward-buffer=\$BUFFER forward-cursor=\$CURSOR forward-context=\$forward_state]\"; _mu_zsh_clear_prompt; _mu_zsh_reset_mode_prompt; }; zle -N _mu_test_delete_slash_completion; bindkey -M mumode '^Y' _mu_test_delete_slash_completion"
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  print -r -- "$delete_slash_setup"
  sleep 0.2
  print -rn -- $'\t\x19'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 5 script -qfec 'TERM=xterm-256color zsh -df' "$delete_slash_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "delete slash completion transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$delete_slash_transcript" | col -b)
[[ "$normalized" == *'[back-buffer= back-cursor=0 back-context=inactive forward-buffer= forward-cursor=0 forward-context=inactive]'* ]] || fail "deleting slash should leave slash-completion context"
[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "delete slash completion should not submit a prompt"

unknown_slash_transcript=$tmpdir/unknown-slash-transcript
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  print -r -- "$interactive_setup"
  sleep 0.2
  print -rn -- $'\t'"/not-a-command custom"$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 5 script -qfec 'TERM=xterm-256color zsh -df' "$unknown_slash_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "unknown slash transcript exited with status $interactive_status"

[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "unknown slash input should not submit a prompt"
normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$unknown_slash_transcript" | col -b)
[[ "$normalized" == *"[mu] unknown slash command: /not-a-command"* ]] || fail "unknown slash input should report a command error"

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

history_disabled_replay=$tmpdir/history-disabled-replay
history_disabled_file=$tmpdir/history-disabled
history_disabled_transcript=$tmpdir/history-disabled-transcript
history_disabled_prompt='agent ignores arrows'
print -r -- "print -rn -- shell-history > ${(q)history_disabled_replay}" > "$history_disabled_file"
rm -f -- "$history_disabled_replay" "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"

history_disabled_setup=" setopt HIST_IGNORE_SPACE; PS1='> '; PATH=${(q)interactive_fake_bin}:\$PATH; export TEST_CAPTURE_ARGS=${(q)interactive_capture_args} TEST_CAPTURE_STDIN=${(q)interactive_capture_stdin} TEST_CAPTURE_CALLS=${(q)interactive_capture_calls}; HISTFILE=${(q)history_disabled_file}; HISTSIZE=100; SAVEHIST=100; fc -R ${(q)history_disabled_file}; source ${(q)root}/mu.zsh"
interactive_status=0
{
  print -r -- "$history_disabled_setup"
  sleep 0.2
  print -rn -- $'\t'"$history_disabled_prompt"$'\e[A\e[B\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 5 script -qfec 'TERM=xterm-256color zsh -df' "$history_disabled_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "history-disabled transcript exited with status $interactive_status"
[[ ! -e "$history_disabled_replay" ]] || fail "mu-mode arrows should not execute the recalled shell history entry"
[[ $(<"$interactive_capture_calls") == x ]] || fail "mu-mode arrows should still submit exactly one mu prompt"

interactive_args=("${(@f)$(<"$interactive_capture_args")}")
[[ "${(j:\0:)interactive_args}" == "${(j:\0:)expected_interactive_args}" ]] || fail "unexpected history-disabled args: ${interactive_args[*]}"
print -rn -- "$history_disabled_prompt"$'\n' > "$interactive_expected_stdin"
cmp -- "$interactive_expected_stdin" "$interactive_capture_stdin" || fail "mu-mode arrows should leave the draft unchanged before submit"

print -- "ok"
