#!/usr/bin/env zsh
set -eu

root=${0:A:h}
source "$root/mu.zsh"

fail() {
  print -u2 -- "FAIL: $*"
  exit 1
}

current_scope_key() {
  _mu_zsh_set_scope_key_for_dir "$PWD"
  print -r -- "$REPLY"
}

submitted_display_before_response() {
  local transcript=$1 stream
  stream=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$transcript" | col -b)
  REPLY=${stream%%"Hello! I'm your terminal agent."*}
}

raw_newline_count_between() {
  local transcript=$1 start=$2 end=$3
  REPLY=$(START="$start" END="$end" perl -0777 -ne '
    if (/.*\Q$ENV{START}\E(.*?)\Q$ENV{END}\E/s) {
      my $between = $1;
      print $between =~ tr/\n//;
    }
  ' "$transcript")
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
      print -u2 -- "test files: $tmpdir"
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
    print -r -- "{$model_json,\"context_tokens\":25,\"context_window\":100,\"project_root\":\"$MU_ZSH_TEST_PROJECT_ROOT\",\"available_models\":{\"providers\":[{\"id\":\"local\",\"models\":[{\"id\":\"local/solo\",\"model_id\":\"solo\",\"supported_efforts\":[\"max\"]},{\"id\":\"local/shared\",\"model_id\":\"shared\",\"supported_efforts\":[\"low\"]}]},{\"id\":\"openai\",\"models\":[{\"id\":\"openai/gpt\",\"model_id\":\"gpt\",\"supported_efforts\":[\"provider-custom\",\"high\",\"minimum\",\"low\",\"max\",\"medium\",\"xhigh\"]},{\"id\":\"openai/gpt-5.6-luna\",\"model_id\":\"gpt-5.6-luna\",\"supported_efforts\":[\"none\",\"max\"]},{\"id\":\"openai/shared\",\"model_id\":\"shared\",\"supported_efforts\":[\"medium\"]}]}]}}"
  elif (( include_commands )); then
    print -r -- "{$model_json,\"context_tokens\":25,\"context_window\":100,\"project_root\":\"$MU_ZSH_TEST_PROJECT_ROOT\",\"commands\":[{\"name\":\"review.md\",\"path\":\"$MU_ZSH_TEST_PROJECT_ROOT/.mu/review.md\",\"scope\":\"project\"}]}"
  else
    print -r -- "{$model_json,\"context_tokens\":25,\"context_window\":100,\"project_root\":\"$MU_ZSH_TEST_PROJECT_ROOT\"}"
  fi
  exit 0
fi
if [[ "$1" == "session" && "$2" == "new" ]]; then
  print -r -- "$*" >> "$MU_ZSH_FAKE_LOG"
  print -r -- "ses_01234567"
  exit 0
fi
if [[ "$1" == "--output" && "$3" == "review.md" ]]; then
  print -r -- "$*" >> "$MU_ZSH_FAKE_LOG"
  if [[ ! -t 0 ]]; then
    prompt=$(cat)
    [[ -n "$prompt" ]] && print -r -- "prompt=$prompt" >> "$MU_ZSH_FAKE_LOG"
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
EOF
chmod +x "$prompt_fake_bin/mu"
MU_ZSH_BIN=$prompt_fake_bin/mu

[[ "$MU_ZSH_MODE" == shell ]] || fail "starts in shell mode"

history_input=$'first line\nsecond $HOME `tick` "quoted"'
history_entry="true mu-history-v1 ${(qqq)history_input}; print replay"
_mu_zsh_decode_history "$history_entry" || fail "decodes tagged zsh history"
[[ "$REPLY" == "$history_input" ]] || fail "tagged zsh history preserves multiline shell-special input"
_mu_zsh_decode_history "mu status" && fail "ordinary shell history must not decode as Mu input"

BUFFER="echo hello"
CURSOR=0
PROMPT="%# "
RPROMPT="right"
_mu_zsh_enter_mode
[[ "$MU_ZSH_MODE" == mu ]] || fail "enters mu mode"
[[ "$BUFFER" == "echo hello" ]] || fail "preserves buffer in mu mode"
[[ "$CURSOR" -eq 0 ]] || fail "preserves cursor in mu mode"
escaped_pwd=${PWD//\%/%%}
expected_prompt="%F{12}prompt-test-model%f %F{6}${escaped_pwd}%f
mu> "
[[ "$PROMPT" == "$MU_ZSH_PROMPT" ]] || fail "sets mu prompt"
[[ "$PROMPT" == "$expected_prompt" ]] || fail "renders two-line mu prompt"

short_fake_bin=$tmpdir/short-bin
mkdir -p -- "$short_fake_bin"
cat > "$short_fake_bin/mu" <<'EOF'
#!/usr/bin/env zsh
if [[ "$1" == "status" ]]; then
  print -r -- '{"model":{"provider_id":"test","model_id":"m","effort":null,"canonical":"m"},"context_tokens":0,"context_window":100,"context_usage_source":"estimated","project_root":null}'
  exit 0
fi
exit 1
EOF
chmod +x "$short_fake_bin/mu"
MU_ZSH_SESSION_ID=short-session
MU_ZSH_TRACKED_SCOPE=$(current_scope_key)
MU_ZSH_BIN=$short_fake_bin/mu
short_prompt=$(_mu_zsh_build_mode_prompt)
MU_ZSH_BIN=$prompt_fake_bin/mu
_mu_zsh_clear_session_state
[[ "$short_prompt" == *"%F{$MU_ZSH_PROMPT_CONTEXT_COLOR}~0%%%f"* ]] || fail "marks estimated context for an attached short session"

compact_fake_bin=$tmpdir/compact-bin
mkdir -p -- "$compact_fake_bin"
cat > "$compact_fake_bin/mu" <<'EOF'
#!/usr/bin/env zsh
if [[ "$1" == "status" ]]; then
  print -r -- '{"model":{"provider_id":"test","model_id":"m","effort":null,"canonical":"m"},"context_tokens":140001,"context_window":200000,"compaction_soft_threshold_tokens":140000,"project_root":null}'
  exit 0
fi
exit 1
EOF
chmod +x "$compact_fake_bin/mu"
MU_ZSH_SESSION_ID=compact-session
MU_ZSH_TRACKED_SCOPE=$(current_scope_key)
MU_ZSH_BIN=$compact_fake_bin/mu
compact_prompt=$(_mu_zsh_build_mode_prompt)
MU_ZSH_BIN=$prompt_fake_bin/mu
_mu_zsh_clear_session_state
[[ "$compact_prompt" == *"%F{$MU_ZSH_PROMPT_CONTEXT_COLOR}[to compact]%f"* ]] || fail "marks sessions above the soft compaction threshold"

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

primary_root=$tmpdir/primary-project
worktree_root=$tmpdir/feature-worktree
mkdir -p -- "$worktree_root/src"
saved_project_root=$MU_ZSH_TEST_PROJECT_ROOT
MU_ZSH_TEST_PROJECT_ROOT=$primary_root
saved_pwd=$PWD
builtin cd "$worktree_root/src"
worktree_prompt=$(_mu_zsh_build_mode_prompt)
builtin cd "$saved_pwd"
MU_ZSH_TEST_PROJECT_ROOT=$saved_project_root
escaped_primary_root=${primary_root//\%/%%}
nested_pwd=$worktree_root/src
escaped_nested_pwd=${nested_pwd//\%/%%}
[[ "$worktree_prompt" == *"%F{6}${escaped_nested_pwd}%f %F{8}(${escaped_primary_root})%f"* ]] || fail "shows primary project root from a linked worktree"

global_fake_bin=$tmpdir/global-bin
mkdir -p -- "$global_fake_bin"
cat > "$global_fake_bin/mu" <<'EOF'
#!/usr/bin/env zsh
if [[ "$1" == "status" ]]; then
  print -r -- '{"model":{"provider_id":"test","model_id":"global-model","effort":null,"canonical":"global-model"},"context_tokens":5,"context_window":100,"project_root":null}'
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
[[ "$global_prompt" == *"%F{6}${escaped_global_pwd}%f %F{8}(global)%f"* ]] || fail "shows global marker outside project scope"

unclean_fake_bin=$tmpdir/unclean-bin
mkdir -p -- "$unclean_fake_bin"
cat > "$unclean_fake_bin/mu" <<'EOF'
#!/usr/bin/env zsh
if [[ "$1" == "status" ]]; then
  print -r -- '{"model":{"provider_id":"test","model_id":"m","effort":null,"canonical":"m"},"context_tokens":25,"context_window":100,"project_root":null,"clean":false}'
  exit 0
fi
exit 1
EOF
chmod +x "$unclean_fake_bin/mu"
MU_ZSH_BIN=$unclean_fake_bin/mu
unclean_prompt=$(_mu_zsh_build_mode_prompt)
MU_ZSH_BIN=$prompt_fake_bin/mu
escaped_unclean=${MU_ZSH_PROMPT_UNCLEAN_TEXT//\%/%%}
[[ "$unclean_prompt" == *"%F{$MU_ZSH_PROMPT_UNCLEAN_COLOR}[${escaped_unclean}]%f"* ]] || fail "shows unclean marker when last turn was interrupted"

clean_fake_bin=$tmpdir/clean-bin
mkdir -p -- "$clean_fake_bin"
cat > "$clean_fake_bin/mu" <<'EOF'
#!/usr/bin/env zsh
if [[ "$1" == "status" ]]; then
  print -r -- '{"model":{"provider_id":"test","model_id":"m","effort":null,"canonical":"m"},"context_tokens":25,"context_window":100,"project_root":null,"clean":true}'
  exit 0
fi
exit 1
EOF
chmod +x "$clean_fake_bin/mu"
MU_ZSH_BIN=$clean_fake_bin/mu
clean_prompt=$(_mu_zsh_build_mode_prompt)
MU_ZSH_BIN=$prompt_fake_bin/mu
[[ "$clean_prompt" != *"[${escaped_unclean}]"* ]] || fail "omits unclean marker when last turn was clean"

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
[[ "$(current_scope_key)" == "global" ]] || fail "starts uncached global"
mkdir -p -- .mu
[[ "$(current_scope_key)" == "project:$scope_discovery_dir" ]] || fail "scope detection refreshes project markers"
primary_scope_dir=$tmpdir/scope-repo
worktree_scope_dir=$tmpdir/scope-worktree
mkdir -p -- "$primary_scope_dir/.mu" "$primary_scope_dir/.git/worktrees/feature"
mkdir -p -- "$worktree_scope_dir/src"
print -r -- "gitdir: $primary_scope_dir/.git/worktrees/feature" > "$worktree_scope_dir/.git"
print -r -- '../..' > "$primary_scope_dir/.git/worktrees/feature/commondir"
builtin cd "$primary_scope_dir"
primary_scope_key=$(current_scope_key)
MU_ZSH_SESSION_ID=session-primary
MU_ZSH_MODEL=model-primary
MU_ZSH_TRACKED_SCOPE=$primary_scope_key
builtin cd "$worktree_scope_dir/src"
[[ "$(current_scope_key)" == "$primary_scope_key" ]] || fail "linked worktree shares the primary project scope"
_mu_zsh_sync_state
[[ "$MU_ZSH_EFFECTIVE_SESSION_ID" == session-primary ]] || fail "linked worktree reuses the primary project session"
[[ "$MU_ZSH_EFFECTIVE_MODEL" == model-primary ]] || fail "linked worktree reuses the primary project model"
_mu_zsh_clear_session_state
_mu_zsh_clear_model_state
MU_ZSH_TRACKED_SCOPE=
mkdir -p -- "$worktree_scope_dir/.mu"
[[ "$(current_scope_key)" == "project:$worktree_scope_dir" ]] || fail "worktree-local .mu creates an independent scope"
builtin cd "$saved_pwd"
HOME=$saved_home

MU_ZSH_BIN=mu
MU_ZSH_OUTPUT=
MU_ZSH_SESSION_ID=
MU_ZSH_TRACKED_SCOPE=
_mu_zsh_base_command_reply
assert_command_reply "inherits configured output by default" mu

MU_ZSH_OUTPUT=detail
MU_ZSH_SESSION_ID=abc123
MU_ZSH_TRACKED_SCOPE=$(current_scope_key)
_mu_zsh_base_command_reply
assert_command_reply "builds attached command" mu --output detail -s abc123

MU_ZSH_SESSION_ID=
MU_ZSH_TRACKED_SCOPE=
_mu_zsh_base_command_reply
assert_command_reply "builds new-session command" mu --output detail
MU_ZSH_BIN=$prompt_fake_bin/mu

MU_ZSH_MODEL=openai/gpt
MU_ZSH_TRACKED_SCOPE=$(current_scope_key)
_mu_zsh_base_command_reply
assert_command_reply "builds pending-model command" "$prompt_fake_bin/mu" --output detail --model openai/gpt
status_json=$(_mu_zsh_status_json)
[[ "$status_json" == *"\"canonical\":\"openai/gpt\""* ]] || fail "status uses pending model"
MU_ZSH_SESSION_ID=abc123
_mu_zsh_base_command_reply
assert_command_reply "builds attached pending-model command" "$prompt_fake_bin/mu" --output detail -s abc123 --model openai/gpt
_mu_zsh_clear_model_state
_mu_zsh_clear_session_state

export MU_ZSH_FAKE_LOG=${TMPDIR:-/tmp}/mu-zsh-test-${$}.log
rm -f "$MU_ZSH_FAKE_LOG"
MU_ZSH_OUTPUT=detail
MU_ZSH_SESSION_ID=
_mu_zsh_submit_prompt "first prompt"
[[ "$MU_ZSH_SESSION_ID" == "ses_01234567" ]] || fail "captures session id after explicit session creation"

_mu_zsh_submit_prompt "second prompt"
grep -q -- "--output detail" "$MU_ZSH_FAKE_LOG" || fail "passes output mode"
grep -q -- "-s ses_01234567" "$MU_ZSH_FAKE_LOG" || fail "passes session id on later submit"
grep -q -- "prompt=first prompt" "$MU_ZSH_FAKE_LOG" || fail "sends first prompt on stdin"
grep -q -- "prompt=second prompt" "$MU_ZSH_FAKE_LOG" || fail "sends second prompt on stdin"

rm -f "$MU_ZSH_FAKE_LOG"

MU_ZSH_BIN=$prompt_fake_bin/mu
MU_ZSH_OUTPUT=detail
MU_ZSH_SESSION_ID=
MU_ZSH_TRACKED_SCOPE=
command_candidates=("${(@f)$(_mu_zsh_slash_command_candidates)}")
[[ "${(j:,:)command_candidates}" == "/attach,/model,/review.md" ]] || fail "hides session commands without a valid session"
MU_ZSH_SESSION_ID=tracked-session
MU_ZSH_TRACKED_SCOPE=$(current_scope_key)
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
model_candidates=("${(@f)$(_mu_zsh_model_completion_candidates "")}")
[[ " ${(j: :)model_candidates} " == *" openai/gpt "* ]] || fail "offers provider-qualified model"
[[ " ${(j: :)model_candidates} " == *" gpt "* ]] || fail "offers unique unqualified model"
[[ " ${(j: :)model_candidates} " == *" local/solo "* ]] || fail "offers second provider-qualified model"
[[ " ${(j: :)model_candidates} " == *" solo "* ]] || fail "offers second unique unqualified model"
[[ " ${(j: :)model_candidates} " == *" openai/shared "* ]] || fail "offers ambiguous model qualified"
[[ " ${(j: :)model_candidates} " == *" local/shared "* ]] || fail "offers other ambiguous model qualified"
[[ " ${(j: :)model_candidates} " == *" shared "* ]] || fail "offers shared model as floating choice"
[[ " ${(j: :)model_candidates} " != *":low "* ]] || fail "does not show variants before colon"
model_candidates=("${(@f)$(_mu_zsh_model_completion_candidates "gpt")}")
[[ " ${(j: :)model_candidates} " == *" gpt "* ]] || fail "keeps all base models available for zsh matching"
[[ " ${(j: :)model_candidates} " != *":high "* ]] || fail "does not show variants until colon"
effort_suffixes=("${(@f)$(_mu_zsh_model_completion_candidates "gpt" 1)}")
[[ "${(j:,:)effort_suffixes}" == ":minimum,:low,:medium,:high,:xhigh,:max,:provider-custom" ]] ||
  fail "sorts recognized exact-model efforts by strength and leaves custom efforts last: ${(j:,:)effort_suffixes}"
qualified_effort_suffixes=("${(@f)$(_mu_zsh_model_completion_candidates "openai/gpt" 1)}")
[[ "${(j:,:)qualified_effort_suffixes}" == "${(j:,:)effort_suffixes}" ]] ||
  fail "provider-qualified exact models use the same sorted effort menu"
prefix_effort_suffixes=("${(@f)$(_mu_zsh_model_completion_candidates "gp" 1)}")
prefix_effort_suffixes=("${(@)prefix_effort_suffixes:#}")
(( ${#prefix_effort_suffixes[@]} == 0 )) ||
  fail "model prefixes do not switch to the effort menu"
if _mu_zsh_model_completion_transition "gpt"; then
  fail "exact models shadowed by longer model names do not transition to efforts"
fi
_mu_zsh_model_completion_transition "gpt-5.6-luna" ||
  fail "unshadowed exact models transition to efforts"
[[ "${(j:,:)MU_ZSH_MODEL_COMPLETION_EFFORTS}" == ":max,:none" ]] ||
  fail "transition exposes the completed model's efforts"
_mu_zsh_model_completion_transition "shared" ||
  fail "floating exact models transition to efforts"
[[ "${(j:,:)MU_ZSH_MODEL_COMPLETION_EFFORTS}" == ":low,:medium" ]] ||
  fail "floating transition merges provider efforts"
captured_compadd_calls=()
compadd() { captured_compadd_calls+=("${(j:,:)@}") }
BUFFER="/model gpt:"
CURSOR=${#BUFFER}
_mu_zsh_fallback_completion
unfunction compadd
for effort in minimum low medium high xhigh max provider-custom; do
  [[ ",$captured_compadd_calls[1]," == *",$effort,"* ]] ||
    fail "exact-model effort menu includes $effort"
done
model_candidates=("${(@f)$(_mu_zsh_model_completion_candidates "gpt:")}")
[[ " ${(j: :)model_candidates} " == *" gpt:low "* ]] || fail "shows unqualified variants after colon"
[[ " ${(j: :)model_candidates} " == *" openai/gpt:high "* ]] || fail "shows provider-qualified variants after colon"
[[ " ${(j: :)model_candidates} " == *" gpt:provider-custom "* ]] || fail "shows provider-defined effort strings"
[[ " ${(j: :)model_candidates} " == *" shared:low "* ]] || fail "merges first floating-model provider efforts"
[[ " ${(j: :)model_candidates} " == *" shared:medium "* ]] || fail "merges floating-model effort suggestions"
MU_ZSH_MODEL=invalid/removed
_mu_zsh_sync_state
model_candidates=("${(@f)$(_mu_zsh_model_completion_candidates "")}")
[[ " ${(j: :)model_candidates} " == *" openai/gpt "* ]] || fail "stale model override does not block model discovery"
_mu_zsh_clear_model_state
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
grep -q -- "retry -s tracked-session --output detail" "$MU_ZSH_FAKE_LOG" || fail "retry slash command targets tracked session"
_mu_zsh_run_slash_command "/model gpt"
rm -f "$MU_ZSH_FAKE_LOG"
_mu_zsh_run_slash_command "/retry"
grep -q -- "retry -s tracked-session --model openai/gpt --output detail" "$MU_ZSH_FAKE_LOG" || fail "retry slash command forwards pending model"
_mu_zsh_clear_model_state
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
grep -q -- "--output detail -s tracked-session review.md" "$MU_ZSH_FAKE_LOG" || fail "custom slash command targets tracked session"
rm -f "$MU_ZSH_FAKE_LOG"
_mu_zsh_run_slash_command "/review.md Focus on authentication"
grep -q -- "--output detail -s tracked-session review.md" "$MU_ZSH_FAKE_LOG" || fail "custom slash command keeps tracked session with instruction"
grep -Fxq -- "prompt=Focus on authentication" "$MU_ZSH_FAKE_LOG" || fail "custom slash command pipes instruction"
rm -f "$MU_ZSH_FAKE_LOG"
_mu_zsh_run_slash_command $'/review.md First line\nSecond line'
custom_prompt=$(cat "$MU_ZSH_FAKE_LOG")
[[ "$custom_prompt" == *$'prompt=First line\nSecond line'* ]] || fail "custom slash command preserves multiline instruction"
_mu_zsh_run_slash_command "/model gpt"
_mu_zsh_run_slash_command "/attach $attachment_one"
_mu_zsh_run_slash_command "/new"
[[ -z "$MU_ZSH_SESSION_ID" && -n "$MU_ZSH_TRACKED_SCOPE" ]] || fail "new slash command lazily clears only the tracked session"
[[ "$MU_ZSH_MODEL" == openai/gpt ]] || fail "new slash command preserves the model override"
(( ${#MU_ZSH_PENDING_ATTACHMENTS[@]} == 1 )) || fail "new slash command preserves pending attachments"
_mu_zsh_clear_model_state
MU_ZSH_PENDING_ATTACHMENTS=()
MU_ZSH_EFFECTIVE_ATTACHMENT_COUNT=0
rm -f "$MU_ZSH_FAKE_LOG"
_mu_zsh_run_slash_command "/review.md"
[[ "$MU_ZSH_SESSION_ID" == "ses_01234567" ]] || fail "custom slash command captures new session id"
_mu_zsh_clear_session_state
rm -f "$MU_ZSH_FAKE_LOG"
_mu_zsh_run_slash_command "/review.md Start a fresh session"
[[ "$MU_ZSH_SESSION_ID" == "ses_01234567" ]] || fail "custom slash instruction captures new session id"
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
[[ "$MU_ZSH_TRACKED_SCOPE" == "$(current_scope_key)" ]] || fail "model slash command records scope"
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
  model=scope-model
  while (( $# )); do
    if [[ "$1" == "--model" ]]; then
      shift
      model=$1
    fi
    shift
  done
  [[ "$model" == invalid/* ]] && exit 1
  print -r -- "{\"model\":{\"provider_id\":\"test\",\"model_id\":\"$model\",\"effort\":null,\"canonical\":\"$model\"},\"context_tokens\":10,\"context_window\":100,\"project_root\":\"$scope_root\"}"
  exit 0
fi
if [[ "$1" == "session" && "$2" == "new" ]]; then
  print -r -- "$PWD :: $*" >> "$MU_ZSH_SCOPE_LOG"
  case "$scope_name" in
    project-a) print -r -- "ses_0000000a" ;;
    project-b) print -r -- "ses_0000000b" ;;
    *) print -r -- "ses_0000000c" ;;
  esac
  exit 0
fi
print -r -- "$PWD :: $*" >> "$MU_ZSH_SCOPE_LOG"
prompt=$(cat)
print -r -- "prompt=$prompt" >> "$MU_ZSH_SCOPE_LOG"
EOF
chmod +x "$scope_fake_bin/mu"
MU_ZSH_BIN=$scope_fake_bin/mu
MU_ZSH_OUTPUT=detail
export MU_ZSH_SCOPE_LOG=${TMPDIR:-/tmp}/mu-zsh-scope-${$}.log
rm -f "$MU_ZSH_SCOPE_LOG"
MU_ZSH_SESSION_ID=
MU_ZSH_TRACKED_SCOPE=
MU_ZSH_EFFECTIVE_SESSION_ID=

saved_pwd=$PWD
builtin cd "$project_a/subdir"
_mu_zsh_submit_prompt "project a prompt"
[[ "$MU_ZSH_SESSION_ID" == "ses_0000000a" ]] || fail "creates a scoped session for the first project"

MU_ZSH_MODEL=model-for-a
MU_ZSH_PENDING_ATTACHMENTS=("$attachment_one")
_mu_zsh_sync_state

builtin cd "$project_b/subdir"
_mu_zsh_base_command_reply
assert_command_reply "does not reuse another project's session before submitting there" "$scope_fake_bin/mu" --output detail
parked_prompt=$(_mu_zsh_build_mode_prompt)
[[ "$parked_prompt" != *'[1 attachments]'* ]] || fail "prompt hides another scope's attachments"
: > "$MU_ZSH_SCOPE_LOG"
status_json=$(_mu_zsh_status_json)
[[ "$status_json" == *"\"project_root\":\"$project_b\""* ]] || fail "status follows the current project"
! grep -q -- "-s ses_0000000a" "$MU_ZSH_SCOPE_LOG" || fail "status should not attach the first project's session in a different project"

builtin cd "$project_a/subdir"
_mu_zsh_base_command_reply
assert_command_reply "returns to the original scoped session and model after cd-ing back" "$scope_fake_bin/mu" --output detail -s ses_0000000a --model model-for-a
(( ${#MU_ZSH_PENDING_ATTACHMENTS[@]} == 1 )) || fail "passive scope observation preserves parked attachments"
restored_prompt=$(_mu_zsh_build_mode_prompt)
[[ "$restored_prompt" == *'[1 attachments]'* ]] || fail "prompt restores parked attachments in their scope"

builtin cd "$project_b/subdir"
if _mu_zsh_run_slash_command "/model invalid/model"; then
  fail "invalid model in another scope should fail"
fi
if _mu_zsh_run_slash_command "/attach $tmpdir/missing-scope-file"; then
  fail "invalid attachment in another scope should fail"
fi
builtin cd "$project_a/subdir"
_mu_zsh_sync_state
[[ "$MU_ZSH_SESSION_ID" == "ses_0000000a" && "$MU_ZSH_MODEL" == model-for-a ]] || fail "invalid actions elsewhere preserve parked session and model"
(( ${#MU_ZSH_PENDING_ATTACHMENTS[@]} == 1 )) || fail "invalid actions elsewhere preserve parked attachments"

builtin cd "$project_b/subdir"
_mu_zsh_run_slash_command "/model model-for-b"
[[ -z "$MU_ZSH_SESSION_ID" ]] || fail "valid model action elsewhere invalidates the parked session"
[[ "$MU_ZSH_MODEL" == model-for-b ]] || fail "valid model action elsewhere replaces the parked model"
(( ${#MU_ZSH_PENDING_ATTACHMENTS[@]} == 0 )) || fail "valid model action elsewhere invalidates parked attachments"
[[ "$MU_ZSH_TRACKED_SCOPE" == "project:$project_b" ]] || fail "valid model action moves the tracked scope"

: > "$MU_ZSH_SCOPE_LOG"
_mu_zsh_submit_prompt "project b prompt"
[[ "$MU_ZSH_SESSION_ID" == "ses_0000000b" ]] || fail "creates a new scoped session after submitting in the second project"
[[ "$MU_ZSH_TRACKED_SCOPE" == "project:$project_b" ]] || fail "keeps the tracked scope after starting in the second project"
grep -Fxq -- "$project_b/subdir :: session new" "$MU_ZSH_SCOPE_LOG" || fail "creates an empty session without forwarding the model override"
grep -Fq -- "$project_b/subdir :: --output detail -s ses_0000000b --model model-for-b" "$MU_ZSH_SCOPE_LOG" || fail "forwards the model override on the first real turn"

builtin cd "$project_a/subdir"
_mu_zsh_base_command_reply
assert_command_reply "forgets the first project's session once a new one starts elsewhere" "$scope_fake_bin/mu" --output detail

builtin cd "$saved_pwd"
MU_ZSH_BIN=$prompt_fake_bin/mu
rm -f "$MU_ZSH_SCOPE_LOG"

if [[ ${MU_ZSH_SKIP_PTY:-0} == 1 ]]; then
  print -- ok
  exit 0
fi

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
  include_models=0
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --model)
        shift
        model=$1
        ;;
      --include-commands)
        include_commands=1
        ;;
      --include-models)
        include_models=1
        ;;
    esac
    shift
  done
  [ "$model" = gpt ] && model=openai/gpt
  provider=${model%%/*}
  model_id=${model#*/}
  [ "$provider" = "$model" ] && provider=test
  model_json="\"model\":{\"provider_id\":\"$provider\",\"model_id\":\"$model_id\",\"effort\":null,\"canonical\":\"$model\"}"
  if [ "$include_models" -eq 1 ]; then
    printf '%s\n' "{$model_json,\"context_tokens\":25,\"context_window\":100,\"project_root\":\"$MU_ZSH_TEST_PROJECT_ROOT\",\"available_models\":{\"providers\":[{\"id\":\"local\",\"models\":[{\"id\":\"local/solo\",\"model_id\":\"solo\",\"supported_efforts\":[\"max\"]},{\"id\":\"local/shared\",\"model_id\":\"shared\",\"supported_efforts\":[\"low\"]}]},{\"id\":\"openai\",\"models\":[{\"id\":\"openai/gpt\",\"model_id\":\"gpt\",\"supported_efforts\":[\"low\",\"high\"]},{\"id\":\"openai/gpt-5.6-luna\",\"model_id\":\"gpt-5.6-luna\",\"supported_efforts\":[\"none\",\"max\"]},{\"id\":\"openai/shared\",\"model_id\":\"shared\",\"supported_efforts\":[\"medium\"]}]}]}}"
  elif [ "$include_commands" -eq 1 ] && [ -n "$TEST_EXTRA_COMMAND" ]; then
    printf '%s\n' "{$model_json,\"context_tokens\":25,\"context_window\":100,\"project_root\":\"$MU_ZSH_TEST_PROJECT_ROOT\",\"commands\":[{\"name\":\"$TEST_EXTRA_COMMAND\",\"path\":\"$MU_ZSH_TEST_PROJECT_ROOT/.mu/$TEST_EXTRA_COMMAND\",\"scope\":\"project\"}]}"
  else
    printf '%s\n' "{$model_json,\"context_tokens\":25,\"context_window\":100,\"project_root\":\"$MU_ZSH_TEST_PROJECT_ROOT\"}"
  fi
  exit 0
fi
if [ "$1" = "--model" ]; then
  shift 2
fi
if [ "$1" = "session" ] && [ "$2" = "new" ]; then
  printf '%s\n' "ses_01234567"
  exit 0
fi
printf x >> "$TEST_CAPTURE_CALLS"
printf '%s\n' "$@" > "$TEST_CAPTURE_ARGS"
cat > "$TEST_CAPTURE_STDIN"
if [ "$2" = detail ]; then
  printf '%s\n\n' "[thought 100ms, 2 tokens]"
fi
printf '%s\n\n' "Hello! I'm your terminal agent."
if [ "$2" = detail ] || [ "$2" = full ]; then
  printf '%s\n\n' "[mu] tokens: 12 in / 5 out  context: 25%" >&2
fi
EOF
chmod +x "$interactive_fake_bin/mu"

interactive_setup="PS1='> '; PATH=${(q)interactive_fake_bin}:\$PATH; export TEST_CAPTURE_ARGS=${(q)interactive_capture_args} TEST_CAPTURE_STDIN=${(q)interactive_capture_stdin} TEST_CAPTURE_CALLS=${(q)interactive_capture_calls}; autoload -Uz compinit; compinit -D; source ${(q)root}/mu.zsh; MU_ZSH_OUTPUT=detail"
interactive_ready=$tmpdir/interactive-ready

send_interactive_setup() {
  rm -f -- "$interactive_ready"
  print -rn -- "$1; : > ${(q)interactive_ready}"$'\r'
  local attempt
  for attempt in {1..100}; do
    [[ -e "$interactive_ready" ]] && return 0
    sleep 0.05
  done
  fail "interactive shell did not finish setup"
}

empty_enter_transcript=$tmpdir/empty-enter-transcript
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  send_interactive_setup "$interactive_setup"
  print -rn -- $'\t\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$empty_enter_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "empty Enter transcript exited with status $interactive_status"
normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$empty_enter_transcript" | col -b)
empty_prompt_pair="prompt-test-model $root"$'\nmu>\nprompt-test-model '"$root"$'\nmu>'
[[ "$normalized" == *"$empty_prompt_pair"* ]] || fail "empty Enter should commit one complete mu prompt before drawing the next"
empty_enter_raw=$(<"$empty_enter_transcript")
[[ "$empty_enter_raw" != *$'\e[A'* ]] || fail "empty Enter should not move up and overwrite the previous prompt"
[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "empty Enter should not call fake mu"

interactive_transcript=$tmpdir/transcript
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  send_interactive_setup "$interactive_setup"
  print -rn -- $'\t\r'
  sleep 0.2
  print -rn -- '   '$'\r'
  sleep 0.2
  print -rn -- 'cancel-me'
  sleep 0.3
  print -rn -- $'\x03'
  sleep 0.4
  print -rn -- 'hello'$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$interactive_transcript" >/dev/null || interactive_status=$?
# The real Ctrl-C above is delivered as SIGINT by the tty, so the interactive
# shell's final status is 130 (128 + SIGINT); the draft cancel is verified from
# the transcript below rather than from a bespoke interrupt widget.
(( interactive_status == 0 || interactive_status == 130 )) || fail "interactive transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$interactive_transcript" | col -b)
submitted_display_before_response "$interactive_transcript"
expected_submitted_display="prompt-test-model $root"$'\nmu> hello\n'
[[ "$REPLY" == *"$expected_submitted_display"* ]] || fail "submitted prompt should remain complete in terminal scrollback"
after_submitted_display=${REPLY#*"$expected_submitted_display"}
[[ "$after_submitted_display" != *"$expected_submitted_display"* ]] || fail "submitted prompt should be committed exactly once"
[[ "$normalized" == *"Hello! I'm your terminal agent."* ]] || fail "interactive response should be rendered"
after_submitted_prompt=${normalized##*$'mu> hello\n'}
[[ "$after_submitted_prompt" == *'[thought '* ]] || fail "thinking indicator should follow the submitted prompt"
raw_newline_count_between "$interactive_transcript" hello '[thought '
[[ "$REPLY" == 1 ]] || fail "submitted prompt handoff should advance one raw line before thinking, saw $REPLY"
[[ "$normalized" == *'mu> cancel-me'* ]] || fail "Ctrl-C should leave the cancelled mu line in scrollback"
[[ $(<"$interactive_capture_calls") == x ]] || fail "interactive fake mu should run exactly once"
after_response=${normalized#*"Hello! I'm your terminal agent."}
[[ "$after_response" == $'\n\n[mu] tokens: 12 in / 5 out  context: 25%\n\n'* ]] || fail "token summary should be a separate block after assistant output"
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
expected_interactive_args=(--output detail -s ses_01234567)
[[ "${(j:\0:)interactive_args}" == "${(j:\0:)expected_interactive_args}" ]] || fail "unexpected interactive args: ${interactive_args[*]}"

no_prompt_sp_transcript=$tmpdir/no-prompt-sp-transcript
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  send_interactive_setup "$interactive_setup; unsetopt PROMPT_SP"
  print -rn -- $'\t'"spacing probe"$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$no_prompt_sp_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "PROMPT_SP-disabled transcript exited with status $interactive_status"
raw_newline_count_between "$no_prompt_sp_transcript" 'spacing probe' '[thought '
[[ "$REPLY" == 1 ]] || fail "PROMPT_SP-disabled handoff should advance one raw line before thinking, saw $REPLY"

shift_enter_transcript=$tmpdir/shift-enter-transcript
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  send_interactive_setup "$interactive_setup"
  print -rn -- $'\t'"first line"$'\e[13;2u'"second line"$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$shift_enter_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "Shift+Enter transcript exited with status $interactive_status"
[[ $(<"$interactive_capture_calls") == x ]] || fail "Shift+Enter should not submit before Enter"
submitted_display_before_response "$shift_enter_transcript"
expected_submitted_display="prompt-test-model $root"$'\nmu> first line\nsecond line\n'
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
  send_interactive_setup "$interactive_setup"
  print -rn -- $'\t'"$wrapped_prompt"$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$wrapped_transcript" >/dev/null || interactive_status=$?
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
  send_interactive_setup "$custom_slash_setup"
  print -rn -- $'\t'"/review.md First line"$'\e[13;2u'"Second line"$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$custom_slash_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "custom slash transcript exited with status $interactive_status"
[[ $(<"$interactive_capture_calls") == x ]] || fail "custom slash command should run once"
custom_slash_expected_stdin=$tmpdir/custom-slash-expected-stdin
print -rn -- 'First line'$'\n''Second line' > "$custom_slash_expected_stdin"
cmp -- "$custom_slash_expected_stdin" "$interactive_capture_stdin" || fail "custom slash instruction should preserve multiline text"
interactive_args=("${(@f)$(<"$interactive_capture_args")}")
expected_custom_slash_args=(--output detail -s ses_01234567 review.md)
[[ "${(j:\0:)interactive_args}" == "${(j:\0:)expected_custom_slash_args}" ]] || fail "custom slash command should use the command path"

concise_transcript=$tmpdir/concise-transcript
concise_setup="$interactive_setup; MU_ZSH_OUTPUT=concise"
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  send_interactive_setup "$concise_setup"
  print -rn -- $'\t'"concise prompt"$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$concise_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "concise transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$concise_transcript" | col -b)
after_submitted_prompt=${normalized##*$'mu> concise prompt\n'}
[[ "$after_submitted_prompt" == *"Hello! I'm your terminal agent."* ]] || fail "concise output should follow the submitted prompt"
raw_newline_count_between "$concise_transcript" 'concise prompt' "Hello! I'm your terminal agent."
[[ "$REPLY" == 1 ]] || fail "concise prompt handoff should advance one raw line before output, saw $REPLY"
after_response=${normalized#*"Hello! I'm your terminal agent."}
[[ "$after_response" != *'[mu] tokens:'* ]] || fail "concise output should omit the token summary"
interactive_args=("${(@f)$(<"$interactive_capture_args")}")
expected_concise_args=(--output concise -s ses_01234567)
[[ "${(j:\0:)interactive_args}" == "${(j:\0:)expected_concise_args}" ]] || fail "unexpected concise interactive args: ${interactive_args[*]}"

model_switch_transcript=$tmpdir/model-switch-transcript
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  send_interactive_setup "$interactive_setup"
  print -rn -- $'\t'"/model gpt"$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$model_switch_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "model switch transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$model_switch_transcript" | col -b)
[[ "$normalized" == *$'[mu] next turns in this scope will use openai/gpt\n'* ]] || fail "model slash command should confirm the canonical model"
after_model_switch=${normalized#*$'[mu] next turns in this scope will use openai/gpt\n'}
[[ "$after_model_switch" == $'\n'* ]] || fail "model slash command should leave an empty line before the next prompt"
[[ "$after_model_switch" != $'\n\n'* ]] || fail "model slash command should not leave two empty lines before the next prompt"
[[ "$after_model_switch" == *$'openai/gpt '"$root"* ]] || fail "model slash command should redraw a fresh-session prompt without context usage"
[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "model slash command should not submit a prompt"

new_session_transcript=$tmpdir/new-session-transcript
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
new_session_setup="$interactive_setup; MU_ZSH_SESSION_ID=tracked-session; _mu_zsh_set_scope_key_for_dir \"\$PWD\"; MU_ZSH_TRACKED_SCOPE=\$REPLY; _mu_zsh_sync_state"
{
  send_interactive_setup "$new_session_setup"
  print -rn -- $'\t'"/new"$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$new_session_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "new session transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$new_session_transcript" | col -b)
[[ "$normalized" == *$'[mu] next turn will start a new session\n'* ]] || fail "new slash command should confirm the next turn starts fresh"
raw_newline_count_between "$new_session_transcript" /new '[mu] next turn will start a new session'
[[ "$REPLY" == 1 ]] || fail "new slash command handoff should advance one raw line before output, saw $REPLY"
after_new_session=${normalized#*$'[mu] next turn will start a new session\n'}
[[ "$after_new_session" == $'\n'* ]] || fail "new slash command should leave an empty line before the next prompt"
[[ "$after_new_session" != $'\n\n'* ]] || fail "new slash command should not leave two empty lines before the next prompt"
[[ "$after_new_session" == *$'prompt-test-model '"$root"* ]] || fail "new slash command should redraw a fresh-session prompt without context usage"
[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "new slash command should not submit a prompt"

slash_listing_transcript=$tmpdir/slash-listing-transcript
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  send_interactive_setup "$interactive_setup"
  print -rn -- $'\t/'
  sleep 0.4
  # Real Ctrl-C (SIGINT) cancels the draft and returns to an empty mu> prompt,
  # so the following Ctrl-D can EOF the shell cleanly.
  print -rn -- $'\x03'
  sleep 0.3
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$slash_listing_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 || interactive_status == 130 )) || fail "slash listing transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$slash_listing_transcript" | col -b)
[[ "$normalized" == *'/model'* ]] || fail "typing slash should proactively list completion candidates"

slash_completion_transcript=$tmpdir/slash-completion-transcript
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  send_interactive_setup "$interactive_setup; zstyle ':completion:*' matcher-list 'm:{a-zA-Z}={A-Za-z}'"
  print -rn -- $'\t'"/MO"$'\t\t'"gpt"$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$slash_completion_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "slash completion transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$slash_completion_transcript" | col -b)
[[ "$normalized" == *$'[mu] next turns in this scope will use openai/gpt\n'* ]] || fail "Tab should use zsh matcher rules to complete /MO to /model"
[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "completed model slash command should not submit a prompt"

common_prefix_transcript=$tmpdir/common-prefix-transcript
common_prefix_setup="$interactive_setup; _mu_test_common_prefix_completion() { BUFFER='/mod'; CURSOR=\${#BUFFER}; _mu_zsh_complete_slash; zle -I; print -r -- \"[completion-buffer=\$BUFFER cursor=\$CURSOR]\"; BUFFER='/model o'; CURSOR=\${#BUFFER}; _mu_zsh_complete_slash; zle -I; print -r -- \"[model-prefix-buffer=\$BUFFER cursor=\$CURSOR]\"; BUFFER=; CURSOR=0; _mu_zsh_reset_mode_prompt; }; zle -N _mu_test_common_prefix_completion; bindkey -M mumode '^T' _mu_test_common_prefix_completion"
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  send_interactive_setup "$common_prefix_setup"
  print -rn -- $'\t\x14'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$common_prefix_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "common-prefix completion transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$common_prefix_transcript" | col -b)
[[ "$normalized" == *'[completion-buffer=/model  cursor=7]'* ]] || fail "completing /model should enter its argument without filling a model prefix"
[[ "$normalized" == *'[model-prefix-buffer=/model openai/ cursor=14]'* ]] || fail "later /model completion should use the normal common prefix"
[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "common-prefix completion should not submit a prompt"

model_effort_transcript=$tmpdir/model-effort-transcript
model_effort_setup="$interactive_setup; zstyle ':completion:*' matcher-list 'm:{a-zA-Z-_}={A-Za-z_-}' 'r:|=*' 'l:|=* r:|=*'; _mu_test_model_effort_completion() { BUFFER='/model luna'; CURSOR=\${#BUFFER}; _mu_zsh_complete_slash; zle -I; print -r -- \"[first-buffer=\$BUFFER first-cursor=\$CURSOR]\"; BUFFER=; CURSOR=0; _mu_zsh_reset_mode_prompt; }; zle -N _mu_test_model_effort_completion; bindkey -M mumode '^T' _mu_test_model_effort_completion"
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  send_interactive_setup "$model_effort_setup"
  print -rn -- $'\t\x14'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$model_effort_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "model effort completion transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$model_effort_transcript" | col -b)
raw_transcript=$(<"$model_effort_transcript")
[[ "$normalized" == *'[first-buffer=/model gpt-5.6-luna: first-cursor=20]'* ]] || fail "infix model completion should append a speculative colon"
[[ "$raw_transcript" == *'none'* && "$raw_transcript" == *'max'* ]] || fail "one model completion should immediately list supported efforts"
[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "model effort completion should not submit a prompt"

effort_menu_transcript=$tmpdir/effort-menu-transcript
effort_menu_setup="$interactive_setup; zstyle ':completion:*' menu select; MU_ZSH_TEST_MENU_CAPTURE=0; _mu_test_capture_effort_menu() { (( MU_ZSH_TEST_MENU_CAPTURE += 1 )); zle -I; print -r -- \"[menu-\$MU_ZSH_TEST_MENU_CAPTURE-buffer=\$BUFFER cursor=\$CURSOR]\"; BUFFER=; CURSOR=0; _mu_zsh_reset_mode_prompt; }; zle -N _mu_test_capture_effort_menu; bindkey -M mumode '^T' _mu_test_capture_effort_menu"
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  send_interactive_setup "$effort_menu_setup"
  print -rn -- $'\t'"/model shared"$'\t\t\r\x14'"/model shared:"$'\t\r\x14'"/model shared"$'\tl\x7f\x14'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$effort_menu_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "effort menu transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$effort_menu_transcript" | col -b)
[[ "$normalized" == *'[menu-1-buffer=/model shared:low cursor=17]'* ]] ||
  fail "Tab after a speculative colon should immediately select the first ordered effort"
[[ "$normalized" == *'[menu-2-buffer=/model shared:low cursor=17]'* ]] ||
  fail "Tab after an explicit empty colon should immediately select the first ordered effort"
[[ "$normalized" == *'[menu-3-buffer=/model shared: cursor=14]'* ]] ||
  fail "typing after a speculative colon should commit it before Backspace"
[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "effort menu completion should not submit a prompt"

floating_effort_transcript=$tmpdir/floating-effort-transcript
floating_effort_setup="$interactive_setup; zstyle ':completion:*' menu select; _mu_test_floating_effort_completion() { BUFFER='/model shared'; CURSOR=\${#BUFFER}; _mu_zsh_complete_slash; zle -I; print -r -- \"[floating-buffer=\$BUFFER cursor=\$CURSOR]\"; BUFFER=; CURSOR=0; _mu_zsh_reset_mode_prompt; }; zle -N _mu_test_floating_effort_completion; bindkey -M mumode '^T' _mu_test_floating_effort_completion"
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  send_interactive_setup "$floating_effort_setup"
  print -rn -- $'\t\x14'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$floating_effort_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "floating effort completion transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$floating_effort_transcript" | col -b)
raw_transcript=$(<"$floating_effort_transcript")
[[ "$normalized" == *'[floating-buffer=/model shared: cursor=14]'* ]] || fail "floating model effort listing should append a speculative colon"
[[ "$raw_transcript" == *'low'* && "$raw_transcript" == *'medium'* ]] || fail "floating model completion should list merged provider efforts"
[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "floating model effort completion should not submit a prompt"

speculative_colon_transcript=$tmpdir/speculative-colon-transcript
speculative_colon_setup="$interactive_setup; _mu_test_speculative_colon_state() { BUFFER='/model shared'; CURSOR=\${#BUFFER}; _mu_zsh_append_speculative_model_colon; _mu_zsh_model_colon; explicit=\"\$BUFFER,\$CURSOR,\$MU_ZSH_SPECULATIVE_MODEL_COLON\"; BUFFER='/model shared'; CURSOR=\${#BUFFER}; _mu_zsh_append_speculative_model_colon; _mu_zsh_speculative_backspace; back=\"\$BUFFER,\$CURSOR,\$MU_ZSH_SPECULATIVE_MODEL_COLON\"; BUFFER='/model shared'; CURSOR=\${#BUFFER}; _mu_zsh_append_speculative_model_colon; _mu_zsh_speculative_delete || true; delete=\"\$BUFFER,\$CURSOR,\$MU_ZSH_SPECULATIVE_MODEL_COLON\"; BUFFER='/model shared'; CURSOR=\${#BUFFER}; _mu_zsh_append_speculative_model_colon; (( CURSOR -= 1 )); _mu_zsh_resolve_speculative_model_colon; moved=\"\$BUFFER,\$CURSOR,\$MU_ZSH_SPECULATIVE_MODEL_COLON\"; BUFFER='/model shared'; CURSOR=\${#BUFFER}; _mu_zsh_append_speculative_model_colon; _mu_zsh_resolve_speculative_model_colon discard; entered=\"\$BUFFER,\$CURSOR,\$MU_ZSH_SPECULATIVE_MODEL_COLON\"; zle -I; print -r -- \"[explicit=\$explicit back=\$back delete=\$delete moved=\$moved entered=\$entered]\"; BUFFER=; CURSOR=0; _mu_zsh_reset_mode_prompt; }; zle -N _mu_test_speculative_colon_state; bindkey -M mumode '^Y' _mu_test_speculative_colon_state"
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  send_interactive_setup "$speculative_colon_setup"
  print -rn -- $'\t\x19'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$speculative_colon_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "speculative colon transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$speculative_colon_transcript" | col -b)
[[ "$normalized" == *'[explicit=/model shared:,14,0 back=/model shared,13,0 delete=/model shared:,14,0 moved=/model shared:,13,0 entered=/model shared,13,0]'* ]] ||
  fail "speculative colon editing actions should commit or remove the delimiter as specified"

delete_slash_transcript=$tmpdir/delete-slash-transcript
delete_slash_setup="$interactive_setup; _mu_test_delete_slash_completion() { BUFFER='/'; CURSOR=1; _mu_zsh_list_slash_choices; zle backward-delete-char; if _mu_zsh_slash_completion_context; then back_state=active; else back_state=inactive; fi; back_buffer=\$BUFFER; back_cursor=\$CURSOR; BUFFER='/'; CURSOR=0; _mu_zsh_list_slash_choices; zle delete-char; if _mu_zsh_slash_completion_context; then forward_state=active; else forward_state=inactive; fi; zle -I; print -r -- \"[back-buffer=\$back_buffer back-cursor=\$back_cursor back-context=\$back_state forward-buffer=\$BUFFER forward-cursor=\$CURSOR forward-context=\$forward_state]\"; BUFFER=; CURSOR=0; _mu_zsh_reset_mode_prompt; }; zle -N _mu_test_delete_slash_completion; bindkey -M mumode '^Y' _mu_test_delete_slash_completion"
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  send_interactive_setup "$delete_slash_setup"
  print -rn -- $'\t\x19'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$delete_slash_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "delete slash completion transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$delete_slash_transcript" | col -b)
[[ "$normalized" == *'[back-buffer= back-cursor=0 back-context=inactive forward-buffer= forward-cursor=0 forward-context=inactive]'* ]] || fail "deleting slash should leave slash-completion context"
[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "delete slash completion should not submit a prompt"

unknown_slash_transcript=$tmpdir/unknown-slash-transcript
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  send_interactive_setup "$interactive_setup"
  print -rn -- $'\t'"/not-a-command custom"$'\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$unknown_slash_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "unknown slash transcript exited with status $interactive_status"

[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "unknown slash input should not submit a prompt"
normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$unknown_slash_transcript" | col -b)
[[ "$normalized" == *"[mu] unknown slash command: /not-a-command"* ]] || fail "unknown slash input should report a command error"

toggle_transcript=$tmpdir/toggle-transcript
toggle_setup="$interactive_setup; _mu_test_tab_roundtrip() { BUFFER='echo toggled'; CURSOR=0; _mu_zsh_tab; _mu_zsh_tab; }; zle -N _mu_test_tab_roundtrip; bindkey '^T' _mu_test_tab_roundtrip"
rm -f -- "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  send_interactive_setup "$toggle_setup"
  print -rn -- $'\x14\r'
  sleep 0.2
  print -rn -- 'exit'
  sleep 0.2
  print -rn -- $'\r'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$toggle_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "toggle transcript exited with status $interactive_status"

normalized=$(perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$toggle_transcript" | col -b)
[[ "$normalized" == *$'\ntoggled\n'* ]] || fail "Tab at cursor start should preserve the buffer when returning to shell mode"
[[ ! -e "$interactive_capture_calls" || ! -s "$interactive_capture_calls" ]] || fail "Tab toggle transcript should not call fake mu"

history_replay=$tmpdir/history-replay
history_file=$tmpdir/history
history_recall_transcript=$tmpdir/history-recall-transcript
history_recalled_prompt='recalled Mu prompt'
history_tagged_entry="true mu-history-v1 ${(qqq)history_recalled_prompt}; print -rn -- recalled > ${(q)history_replay}"
print -r -- "$history_tagged_entry" > "$history_file"
print -r -- "print -rn -- shell-history > ${(q)history_replay}" >> "$history_file"
rm -f -- "$history_replay" "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"

history_setup=" setopt HIST_IGNORE_SPACE; PS1='> '; PATH=${(q)interactive_fake_bin}:\$PATH; export TEST_CAPTURE_ARGS=${(q)interactive_capture_args} TEST_CAPTURE_STDIN=${(q)interactive_capture_stdin} TEST_CAPTURE_CALLS=${(q)interactive_capture_calls}; HISTFILE=${(q)history_file}; HISTSIZE=100; SAVEHIST=100; fc -R ${(q)history_file}; source ${(q)root}/mu.zsh"
interactive_status=0
{
  send_interactive_setup "$history_setup"
  print -rn -- $'\t\e[A\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$history_recall_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "history recall transcript exited with status $interactive_status"
[[ ! -e "$history_replay" ]] || fail "Mu history recall should skip and not execute shell history"
[[ $(<"$interactive_capture_calls") == x ]] || fail "recalled Mu history should submit exactly one prompt"
print -rn -- "$history_recalled_prompt"$'\n' > "$interactive_expected_stdin"
cmp -- "$interactive_expected_stdin" "$interactive_capture_stdin" || fail "Up should recall the prior Mu prompt"

history_restore_transcript=$tmpdir/history-restore-transcript
history_draft='keep this draft'
rm -f -- "$history_replay" "$interactive_capture_args" "$interactive_capture_stdin" "$interactive_capture_calls"
interactive_status=0
{
  send_interactive_setup "$history_setup"
  print -rn -- $'\t'"$history_draft"$'\e[A\e[B\r'
  sleep 0.4
  print -rn -- $'\x04'
} | timeout 10 script -qfec 'TERM=xterm-256color zsh -df' "$history_restore_transcript" >/dev/null || interactive_status=$?
(( interactive_status == 0 )) || fail "history restore transcript exited with status $interactive_status"
[[ ! -e "$history_replay" ]] || fail "Mu history navigation should not execute shell history"
print -rn -- "$history_draft"$'\n' > "$interactive_expected_stdin"
cmp -- "$interactive_expected_stdin" "$interactive_capture_stdin" || fail "Down should restore the pre-history Mu draft"

print -- "ok"
