#!/usr/bin/env fish

if not set -q MU_FISH_TEST_ISOLATED; or not set -q TEST_TMPDIR[1]
    set -l test_root (path resolve (path dirname (status filename)))
    set -l test_tmpdir (mktemp -d /tmp/mu-fish-test.XXXXXX); or exit 1
    mkdir -p "$test_tmpdir/home/.config" "$test_tmpdir/home/.local/share" "$test_tmpdir/home/.cache"; or exit 1

    env \
        MU_FISH_TEST_ISOLATED=1 \
        TEST_TMPDIR="$test_tmpdir" \
        HOME="$test_tmpdir/home" \
        XDG_CONFIG_HOME="$test_tmpdir/home/.config" \
        XDG_DATA_HOME="$test_tmpdir/home/.local/share" \
        XDG_CACHE_HOME="$test_tmpdir/home/.cache" \
        (status fish-path) "$test_root/test.fish" $argv
    set -l test_status $status

    if set -q MU_FISH_KEEP_TEST_TMP
        printf 'kept test files in %s\n' "$test_tmpdir" >&2
    else
        rm -rf -- "$test_tmpdir"
    end
    exit $test_status
end

set -g TEST_ROOT (path resolve (path dirname (status filename)))

function fail --argument-names message
    printf 'FAIL: %s\n' "$message" >&2
    exit 1
end

function assert_equal --argument-names actual expected message
    test "$actual" = "$expected"; or fail "$message: expected <$expected>, got <$actual>"
end

function assert_contains --argument-names actual expected message
    string match -q "*$expected*" -- "$actual"; or fail "$message: missing <$expected>"
end

set -g fish_history mu_fish_test_(random)

source "$TEST_ROOT/mu.fish"

assert_equal "$MU_FISH_MODE" shell 'starts in shell mode'
assert_equal (_mu_fish_current_scope) "project:$TEST_ROOT" 'discovers repository scope'
set history_input (printf 'first line\nsecond $HOME `tick` "quoted"' | string collect)
_mu_fish_history_entry "$history_input"
_mu_fish_decode_history "$_MU_FISH_HISTORY_ENTRY; printf replay"; or fail 'decodes tagged Fish history'
assert_equal "$_MU_FISH_DECODED_HISTORY" "$history_input" 'tagged Fish history preserves multiline shell-special input'
_mu_fish_decode_history 'mu status'; and fail 'ordinary shell history must not decode as Mu input'

set project_a "$TEST_TMPDIR/project-a"
set project_b "$TEST_TMPDIR/project-b"
mkdir -p "$project_a/subdir" "$project_b/subdir" "$project_b/.mu"
mkdir "$project_a/.git"
assert_equal (_mu_fish_scope_for_dir "$project_a/subdir") "project:$project_a" 'discovers Git project'
assert_equal (_mu_fish_scope_for_dir "$project_b/subdir") "project:$project_b" 'discovers .mu project'
assert_equal (_mu_fish_scope_for_dir "$HOME") global 'stops project discovery at HOME'

set main_worktree "$TEST_TMPDIR/main-worktree"
set linked_worktree "$TEST_TMPDIR/linked-worktree"
set linked_git_dir "$main_worktree/.git/worktrees/linked-worktree"
mkdir -p "$linked_git_dir" "$linked_worktree/subdir"
printf 'gitdir: %s\n' "$linked_git_dir" >"$linked_worktree/.git"
printf '../..\n' >"$linked_git_dir/commondir"
assert_equal (_mu_fish_scope_for_dir "$linked_worktree/subdir") "project:$main_worktree" 'maps linked worktree to main scope'
mkdir "$linked_worktree/.mu"
assert_equal (_mu_fish_scope_for_dir "$linked_worktree/subdir") "project:$linked_worktree" 'worktree-local .mu overrides shared scope'

set fake_bin "$TEST_TMPDIR/bin"
set capture_args "$TEST_TMPDIR/args"
set capture_stdin "$TEST_TMPDIR/stdin"
set capture_calls "$TEST_TMPDIR/calls"
set capture_session_args "$TEST_TMPDIR/session-args"
mkdir "$fake_bin"

begin
    printf '%s\n' '#!/bin/sh'
    printf '%s\n' 'if [ "$1" = status ]; then'
    printf '%s\n' '  model=local/test'
    printf '%s\n' '  session='
    printf '%s\n' '  include_commands=0'
    printf '%s\n' '  include_models=0'
    printf '%s\n' '  while [ "$#" -gt 0 ]; do'
    printf '%s\n' '    case "$1" in'
    printf '%s\n' '      --model) shift; model=$1 ;;'
    printf '%s\n' '      -s|--session) shift; session=$1 ;;'
    printf '%s\n' '      -c|--continue) [ "${MU_FISH_TEST_NO_CURRENT:-0}" = 1 ] || session=ses_0000000d ;;'
    printf '%s\n' '      --include-commands) include_commands=1 ;;'
    printf '%s\n' '      --include-models) include_models=1 ;;'
    printf '%s\n' '    esac'
    printf '%s\n' '    shift'
    printf '%s\n' '  done'
    printf '%s\n' '  [ "$session" = ses_missing ] && exit 1'
    printf '%s\n' '  [ "$model" = unknown ] && exit 1'
    printf '%s\n' '  [ "$model" = gpt ] && model=openai/gpt'
    printf '%s\n' '  session_json=null'
    printf '%s\n' '  [ -n "$session" ] && session_json="\"$session\""'
    printf '%s\n' '  if [ "$include_models" -eq 1 ]; then'
    printf '%s\n' '    printf "%s\n" "{\"model\":{\"canonical\":\"$model\"},\"output\":\"concise\",\"session_id\":$session_json,\"context_tokens\":25,\"context_window\":100,\"project_root\":\"$TEST_PROJECT_ROOT\",\"clean\":true,\"available_models\":{\"providers\":[{\"id\":\"local\",\"models\":[{\"id\":\"local/solo\",\"model_id\":\"solo\",\"supported_efforts\":[\"max\"]},{\"id\":\"local/shared\",\"model_id\":\"shared\",\"supported_efforts\":[]}]},{\"id\":\"openai\",\"models\":[{\"id\":\"openai/gpt\",\"model_id\":\"gpt\",\"supported_efforts\":[\"low\",\"high\"]},{\"id\":\"openai/shared\",\"model_id\":\"shared\",\"supported_efforts\":[\"medium\"]}]}]}}"'
    printf '%s\n' '  elif [ "$include_commands" -eq 1 ]; then'
    printf '%s\n' '    printf "%s\n" "{\"model\":{\"canonical\":\"$model\"},\"output\":\"concise\",\"session_id\":$session_json,\"context_tokens\":25,\"context_window\":100,\"project_root\":\"$TEST_PROJECT_ROOT\",\"clean\":true,\"commands\":[{\"name\":\"review.md\"}]}"'
    printf '%s\n' '  else'
    printf '%s\n' '    printf "%s\n" "{\"model\":{\"canonical\":\"$model\"},\"output\":\"concise\",\"session_id\":$session_json,\"context_tokens\":75,\"context_window\":100,\"compaction_soft_threshold_tokens\":70,\"context_usage_source\":\"estimated\",\"project_root\":\"$TEST_PROJECT_ROOT\",\"clean\":true}"'
    printf '%s\n' '  fi'
    printf '%s\n' '  exit 0'
    printf '%s\n' fi
    printf '%s\n' 'if [ "$1" = transcript ]; then'
    printf '%s\n' '  printf x >>"$TEST_CAPTURE_CALLS"'
    printf '%s\n' '  printf "%s\n" "$@" >"$TEST_CAPTURE_ARGS"'
    printf '%s\n' '  case "$*" in *ses_missing*) exit 1 ;; esac'
    printf '%s\n' '  printf "%s\n" "Loaded transcript."'
    printf '%s\n' '  exit 0'
    printf '%s\n' fi
    printf '%s\n' 'if [ "$1" = new ]; then'
    printf '%s\n' '  printf "%s\n" "$@" >"$TEST_CAPTURE_SESSION_ARGS"'
    printf '%s\n' '  printf "%s\n" ses_01234567'
    printf '%s\n' '  exit 0'
    printf '%s\n' fi
    printf '%s\n' 'printf x >>"$TEST_CAPTURE_CALLS"'
    printf '%s\n' 'printf "%s\n" "$@" >"$TEST_CAPTURE_ARGS"'
    printf '%s\n' 'cat >"$TEST_CAPTURE_STDIN"'
    printf '%s\n' 'printf "%s\n\n" "Hello from Fish."'
end >"$fake_bin/mu"
chmod +x "$fake_bin/mu"

set -gx TEST_PROJECT_ROOT "$TEST_ROOT"
set -gx TEST_CAPTURE_ARGS "$capture_args"
set -gx TEST_CAPTURE_STDIN "$capture_stdin"
set -gx TEST_CAPTURE_CALLS "$capture_calls"
set -gx TEST_CAPTURE_SESSION_ARGS "$capture_session_args"
set -g MU_FISH_BIN "$fake_bin/mu"
set -g MU_FISH_OUTPUT detail

set prompt (_mu_fish_build_mode_prompt | string collect)
assert_contains "$prompt" local/test 'prompt shows model'
assert_contains "$prompt" "$TEST_ROOT" 'prompt shows cwd'
assert_contains "$prompt" 'mu> ' 'prompt shows input marker'
string match -q '*75%*' -- "$prompt"; and fail 'fresh prompt should omit context'
string replace -q '[to compact]' '' -- "$prompt"; and fail 'fresh prompt should omit compaction marker'

set -g MU_FISH_SESSION_ID ses_0000000a
set -g MU_FISH_TRACKED_SCOPE (_mu_fish_current_scope)
_mu_fish_sync_state
set prompt (_mu_fish_build_mode_prompt | string collect)
assert_contains "$prompt" '~75%' 'session prompt marks estimated context'
string replace -q '[to compact]' '' -- "$prompt"; or fail 'session prompt marks pending compaction'

set command (_mu_fish_base_command)
assert_equal (string join \x1e -- $command) (string join \x1e -- "$fake_bin/mu" --output detail -s ses_0000000a) 'builds active turn command'

set attachment "$TEST_TMPDIR/file with spaces.png"
printf image >"$attachment"
set -g MU_FISH_MODEL openai/gpt
set -g MU_FISH_EFFECTIVE_MODEL openai/gpt
set -g MU_FISH_PENDING_ATTACHMENTS "$attachment"
set -g MU_FISH_EFFECTIVE_ATTACHMENT_COUNT 1
set -g MU_FISH_OUTPUT
rm -f "$capture_args" "$capture_calls"
set load_output (_mu_fish_run_slash_command '/load ses_0000000b' | string collect)
assert_equal "$MU_FISH_SESSION_ID" ses_0000000b '/load attaches the selected session'
assert_equal "$MU_FISH_MODEL" openai/gpt '/load preserves the model override'
set -q MU_FISH_PENDING_ATTACHMENTS[1]; or fail '/load preserves pending attachments'
assert_contains "$load_output" 'Loaded transcript.' '/load renders the selected transcript'
assert_contains "$load_output" '[mu] loaded session ses_0000000b' '/load confirms the selected session'
set load_args (cat "$capture_args")
assert_equal (string join \x1e -- $load_args) (string join \x1e -- transcript --session ses_0000000b --output concise) '/load uses the configured output density'
set -g MU_FISH_OUTPUT full
rm -f "$capture_args" "$capture_calls"
_mu_fish_run_slash_command '/load ses_0000000c' >/dev/null
set load_args (cat "$capture_args")
assert_equal (string join \x1e -- $load_args) (string join \x1e -- transcript --session ses_0000000c --output full) '/load uses the shell output override'
rm -f "$capture_args" "$capture_calls"
set load_output (_mu_fish_run_slash_command '/load' | string collect)
assert_equal "$MU_FISH_SESSION_ID" ses_0000000d 'argument-free /load attaches current-session'
set load_args (cat "$capture_args")
assert_equal (string join \x1e -- $load_args) (string join \x1e -- transcript --session ses_0000000d --output full) 'argument-free /load replays current-session explicitly'
assert_contains "$load_output" '[mu] loaded session ses_0000000d' 'argument-free /load confirms the resolved session'
set -gx MU_FISH_TEST_NO_CURRENT 1
_mu_fish_run_slash_command '/load' >/dev/null; and fail 'argument-free /load should reject a missing current-session'
set -e MU_FISH_TEST_NO_CURRENT
assert_equal "$MU_FISH_SESSION_ID" ses_0000000d 'failed current load preserves the attached session'
_mu_fish_run_slash_command '/load ses_missing' >/dev/null 2>&1; and fail '/load should reject a missing session'
assert_equal "$MU_FISH_SESSION_ID" ses_0000000d 'failed load preserves the attached session'
_mu_fish_run_slash_command '/load ses_0000000b extra' >/dev/null; and fail '/load should accept exactly one session id'
set -g MU_FISH_SESSION_ID ses_0000000a
set -g MU_FISH_EFFECTIVE_SESSION_ID ses_0000000a
set -g MU_FISH_OUTPUT detail
_mu_fish_clear_model_state
set -g MU_FISH_PENDING_ATTACHMENTS
set -g MU_FISH_PENDING_ATTACHMENTS "$attachment"
rm -f "$capture_args" "$capture_stdin" "$capture_calls"
_mu_fish_submit_prompt 'inspect this'
assert_equal (cat "$capture_calls") x 'submits one turn'
assert_equal (cat "$capture_stdin") 'inspect this' 'passes prompt on stdin'
set submitted_args (cat "$capture_args")
assert_equal (string join \x1e -- $submitted_args) (string join \x1e -- --output detail -s ses_0000000a -a "$attachment") 'forwards session and attachment'
not set -q MU_FISH_PENDING_ATTACHMENTS[1]; or fail 'submission clears attachments'

set replay (builtin history search --max 1 | string collect)
assert_contains "$replay" 'true mu-history-v1 ' 'tags Fish Mu history'
assert_contains "$replay" "printf '%s\\n'" 'records replayable Fish history'
assert_contains "$replay" 'inspect this' 'history preserves submitted prompt'
_mu_fish_decode_history "$replay"; or fail 'decodes recorded Fish turn history'
assert_equal "$_MU_FISH_DECODED_HISTORY" 'inspect this' 'recorded Fish turn decodes to its original prompt'
fish -n -c "$replay"; or fail 'recorded Fish history is not valid Fish syntax'
builtin history save
set expected_history_file "$XDG_DATA_HOME/fish/$fish_history"_history
test -f "$expected_history_file"; or fail "test history escaped isolated data directory: $expected_history_file"

_mu_fish_run_slash_command "/attach $attachment"
assert_equal "$MU_FISH_PENDING_ATTACHMENTS[1]" "$attachment" '/attach stages resolved file'
_mu_fish_run_slash_command /attach
_mu_fish_run_slash_command '/attach --clear'
not set -q MU_FISH_PENDING_ATTACHMENTS[1]; or fail '/attach --clear clears queue'

_mu_fish_run_slash_command '/model gpt'
assert_equal "$MU_FISH_EFFECTIVE_MODEL" openai/gpt '/model stores canonical model'
_mu_fish_run_slash_command '/model unknown'; and fail '/model should reject unknown model'

set models (_mu_fish_model_candidates gp)
contains gpt $models; or fail 'model candidates include unique shorthand'
contains openai/gpt $models; or fail 'model candidates include canonical id'
contains shared $models; or fail 'model candidates include shared floating choice'
set efforts (_mu_fish_model_effort_suffixes gpt)
contains :low $efforts; or fail 'effort candidates include low'
contains :high $efforts; or fail 'effort candidates include high'
set shared_efforts (_mu_fish_model_effort_suffixes shared)
contains :medium $shared_efforts; or fail 'floating effort candidates merge provider suggestions'
set model_records (_mu_fish_model_records)
_mu_fish_model_completion_transition gpt $model_records; or fail 'unshadowed exact models transition to efforts'
assert_equal (string join , -- $_MU_FISH_MODEL_COMPLETION_EFFORTS) :low,:high 'model transition exposes configured efforts'
_mu_fish_model_completion_transition shared $model_records; or fail 'floating exact models transition to efforts'
assert_equal (string join , -- $_MU_FISH_MODEL_COMPLETION_EFFORTS) :medium 'floating transition merges provider efforts'
set shadow_records \
    (printf 'openai/gpt\tgpt\tlow\n') \
    (printf 'openai/gpt-plus\tgpt-plus\thigh\n')
_mu_fish_model_completion_transition gpt $shadow_records; and fail 'prefix-shadowed exact models must not transition to efforts'
set -g MU_FISH_MODEL unknown
_mu_fish_sync_state
set models (_mu_fish_model_candidates gp)
contains openai/gpt $models; or fail 'stale model override does not block model discovery'
set -g MU_FISH_MODEL openai/gpt
_mu_fish_sync_state
assert_equal (_mu_fish_common_prefix 'file*one' 'file*two') 'file*' 'common prefix treats wildcard characters literally'
set literal_matches (_mu_fish_matching_candidates 'file*' 'file*star' file-other)
assert_equal (string join \x1e -- $literal_matches) 'file*star' 'candidate matching treats wildcard characters literally'

set -g MU_FISH_SESSION_ID ses_0000000a
set -g MU_FISH_TRACKED_SCOPE (_mu_fish_current_scope)
_mu_fish_sync_state
_mu_fish_run_slash_command "/attach $attachment"
_mu_fish_run_slash_command /new
not set -q MU_FISH_SESSION_ID[1]; or fail '/new clears session'
assert_equal "$MU_FISH_MODEL" openai/gpt '/new preserves model override'
set -q MU_FISH_PENDING_ATTACHMENTS[1]; or fail '/new preserves pending attachments'

rm -f "$capture_args" "$capture_stdin" "$capture_calls"
_mu_fish_run_slash_command '/review.md First line
Second line'
assert_equal (cat "$capture_calls") x 'custom slash command runs one turn'
set custom_stdin (cat "$capture_stdin" | string collect)
set expected_custom_stdin 'First line
Second line'
assert_equal "$custom_stdin" "$expected_custom_stdin" 'custom slash command preserves multiline instruction'
set custom_args (cat "$capture_args")
assert_equal "$custom_args[-1]" review.md 'custom slash command selects prompt file'

set saved_pwd "$PWD"
_mu_fish_clear_tracked_state
cd "$project_a/subdir"
set -g MU_FISH_TRACKED_SCOPE (_mu_fish_current_scope)
set -g MU_FISH_SESSION_ID ses_0000000a
set -g MU_FISH_MODEL local/solo
set -g MU_FISH_PENDING_ATTACHMENTS "$attachment"
_mu_fish_sync_state

cd "$project_b/subdir"
set observed_command (_mu_fish_base_command)
assert_equal (string join \x1e -- $observed_command) (string join \x1e -- "$fake_bin/mu" --output detail) 'passive observation hides another scope bundle'
set observed_prompt (_mu_fish_build_mode_prompt | string collect)
not string match -q '*attachments*' -- "$observed_prompt"; or fail 'prompt hides another scope attachments'

cd "$project_a/subdir"
_mu_fish_sync_state
assert_equal "$MU_FISH_EFFECTIVE_SESSION_ID" ses_0000000a 'returning without an action restores parked session'
assert_equal "$MU_FISH_EFFECTIVE_MODEL" local/solo 'returning without an action restores parked model'
assert_equal "$MU_FISH_EFFECTIVE_ATTACHMENT_COUNT" 1 'returning without an action restores parked attachments'

cd "$project_b/subdir"
_mu_fish_run_slash_command '/model unknown'; and fail 'invalid model elsewhere should fail'
_mu_fish_run_slash_command "/attach $TEST_TMPDIR/missing-scope-file"; and fail 'invalid attachment elsewhere should fail'
_mu_fish_run_slash_command '/load ses_missing'; and fail 'invalid load elsewhere should fail'
cd "$project_a/subdir"
_mu_fish_sync_state
assert_equal "$MU_FISH_SESSION_ID" ses_0000000a 'invalid actions elsewhere preserve parked session'
assert_equal "$MU_FISH_MODEL" local/solo 'invalid actions elsewhere preserve parked model'
set -q MU_FISH_PENDING_ATTACHMENTS[1]; or fail 'invalid actions elsewhere preserve parked attachments'

cd "$project_b/subdir"
_mu_fish_run_slash_command '/model gpt'
not set -q MU_FISH_SESSION_ID[1]; or fail 'valid model action elsewhere invalidates parked session'
assert_equal "$MU_FISH_MODEL" openai/gpt 'valid model action elsewhere replaces parked model'
not set -q MU_FISH_PENDING_ATTACHMENTS[1]; or fail 'valid model action elsewhere invalidates parked attachments'
assert_equal "$MU_FISH_TRACKED_SCOPE" "project:$project_b" 'valid model action moves tracked scope'

rm -f "$capture_args" "$capture_stdin" "$capture_calls" "$capture_session_args"
_mu_fish_submit_prompt 'project b prompt'
set session_args (cat "$capture_session_args")
assert_equal (string join \x1e -- $session_args) new 'creates an empty session without forwarding model override'
set submitted_args (cat "$capture_args")
assert_equal (string join \x1e -- $submitted_args) (string join \x1e -- --output detail -s ses_01234567 --model openai/gpt) 'forwards model override on first real turn'

cd "$project_a/subdir"
set observed_command (_mu_fish_base_command)
assert_equal (string join \x1e -- $observed_command) (string join \x1e -- "$fake_bin/mu" --output detail) 'old scope bundle stays invalidated'
cd "$saved_pwd"
_mu_fish_clear_tracked_state

function _test_enter_hook
    set -g TEST_ENTER_HOOK_RAN yes
end
function _test_exit_hook
    set -g TEST_EXIT_HOOK_RAN yes
end
set -g MU_FISH_ENTER_HOOKS _test_enter_hook
set -g MU_FISH_EXIT_HOOKS _test_exit_hook
_mu_fish_enter_mode
assert_equal "$MU_FISH_MODE" mu 'enters Mu mode'
assert_equal "$TEST_ENTER_HOOK_RAN" yes 'runs enter hooks'
_mu_fish_exit_mode
assert_equal "$MU_FISH_MODE" shell 'exits Mu mode'
assert_equal "$TEST_EXIT_HOOK_RAN" yes 'runs exit hooks'

function fish_prompt
    printf 'custom:%s:%s> ' $status (string join , $pipestatus)
end
function fish_right_prompt
    printf 'right:%s:%s> ' $status (string join , $pipestatus)
end
function fish_mode_prompt
    printf 'mode:%s:%s> ' $status (string join , $pipestatus)
end
source "$TEST_ROOT/mu.fish"
set status_prompt "$TEST_TMPDIR/status-prompt"
false | true
fish_prompt >"$status_prompt"
assert_equal (cat "$status_prompt") 'custom:0:1,0> ' 're-sourced wrapper preserves prompt status'
false | true
fish_right_prompt >"$status_prompt"
assert_equal (cat "$status_prompt") 'right:0:1,0> ' 'right prompt wrapper preserves prompt status'
false | true
fish_mode_prompt >"$status_prompt"
assert_equal (cat "$status_prompt") 'mode:0:1,0> ' 'mode prompt wrapper preserves prompt status'

function _test_default_tab
end
function _test_insert_tab
end
bind -M default tab _test_default_tab
bind -M insert tab _test_insert_tab
_mu_fish_configure_keymap
assert_equal (string join \x1e -- $_MU_FISH_DEFAULT_TAB_BINDING) _test_default_tab 'captures default-mode Tab binding'
assert_equal (string join \x1e -- $_MU_FISH_INSERT_TAB_BINDING) _test_insert_tab 'captures insert-mode Tab binding'
_mu_fish_configure_keymap
assert_equal (string join \x1e -- $_MU_FISH_DEFAULT_TAB_BINDING) _test_default_tab 'reconfiguration preserves default-mode Tab binding'
assert_equal (string join \x1e -- $_MU_FISH_INSERT_TAB_BINDING) _test_insert_tab 'reconfiguration preserves insert-mode Tab binding'

if test "$MU_FISH_SKIP_PTY" = 1
    printf 'ok\n'
    exit 0
end

for dependency in script timeout perl col jq
    command -q "$dependency"; or fail "missing test dependency: $dependency"
end

set interactive_home "$TEST_TMPDIR/interactive-home"
set interactive_ready "$TEST_TMPDIR/interactive-ready"
mkdir -p "$interactive_home"
rm -f "$capture_args" "$capture_stdin" "$capture_calls"

set setup_parts \
    "set -gx HOME "(string escape "$interactive_home") \
    "set -gx XDG_CONFIG_HOME "(string escape "$interactive_home/.config") \
    "set -gx XDG_DATA_HOME "(string escape "$interactive_home/.local/share") \
    "set -gx XDG_CACHE_HOME "(string escape "$interactive_home/.cache") \
    "set -gx TEST_PROJECT_ROOT "(string escape "$TEST_ROOT") \
    "set -gx TEST_CAPTURE_ARGS "(string escape "$capture_args") \
    "set -gx TEST_CAPTURE_STDIN "(string escape "$capture_stdin") \
    "set -gx TEST_CAPTURE_CALLS "(string escape "$capture_calls") \
    "set -gx TEST_CAPTURE_SESSION_ARGS "(string escape "$capture_session_args") \
    "set -g MU_FISH_BIN "(string escape "$fake_bin/mu") \
    'set -g MU_FISH_OUTPUT detail' \
    "source "(string escape "$TEST_ROOT/mu.fish")
set interactive_setup (string join '; ' $setup_parts)

function send_interactive_setup --argument-names setup ready
    rm -f -- "$ready"
    printf '%s; touch %s\r' "$setup" (string escape "$ready")
    for attempt in (seq 1 100)
        test -e "$ready"; and return 0
        sleep 0.05
    end
    fail 'interactive Fish did not finish setup'
end

set transcript "$TEST_TMPDIR/transcript"
begin
    sleep 0.2
    send_interactive_setup "$interactive_setup" "$interactive_ready"
    printf '\t'
    sleep 0.2
    printf '\r'
    sleep 0.2
    printf '   \r'
    sleep 0.2
    printf cancel-me
    sleep 0.2
    printf '\x03'
    sleep 0.3
    printf /mo
    sleep 0.2
    printf '\t'
    sleep 0.2
    printf 'gpt\r'
    sleep 0.3
    printf hello
    sleep 0.2
    printf '\r'
    sleep 0.5
    printf '\x04'
end | timeout 10 script -qfec \
    'env fish_features=no-query-term,no-keyboard-protocols,no-mark-prompt TERM=xterm-256color fish --no-config' \
    "$transcript" >/dev/null
set interactive_status $pipestatus[2]
test $interactive_status -eq 0; or fail "interactive transcript exited with status $interactive_status"

test (cat "$capture_calls") = x; or fail 'interactive fake Mu should run exactly once'
assert_equal (cat "$capture_stdin") hello 'interactive prompt reaches stdin'
set interactive_args (cat "$capture_args")
assert_equal (string join \x1e -- $interactive_args) (string join \x1e -- --output detail -s ses_01234567 --model openai/gpt) 'interactive turn uses created session and completed model'

set normalized (perl -pe 's/\e\[[0-?]*[ -\/]*[@-~]//g' "$transcript" | col -b | string collect)
set raw_transcript (string collect <"$transcript")
assert_contains "$raw_transcript" cancel-me 'Ctrl-C leaves cancelled draft in scrollback'
assert_contains "$normalized" 'hello

Hello from Fish.' 'submitted prompt remains visible before streamed output'
assert_contains "$normalized" 'next turns in this scope will use openai/gpt' 'Tab completes slash command before model selection'
assert_contains "$normalized" 'Hello from Fish.' 'interactive output streams'
assert_contains "$normalized" 'mu>' 'Mu prompt redraws after the turn'

set history_recalled_prompt 'recalled Mu prompt'
_mu_fish_history_entry "$history_recalled_prompt"
set history_marker "$_MU_FISH_HISTORY_ENTRY"
set history_setup (string join '; ' -- \
    "$interactive_setup" \
    "builtin history append "(string escape "$history_marker") \
    "builtin history append "(string escape 'echo shell-only'))

set history_recall_transcript "$TEST_TMPDIR/history-recall-transcript"
rm -f "$capture_args" "$capture_stdin" "$capture_calls" "$interactive_ready"
begin
    sleep 0.2
    send_interactive_setup "$history_setup" "$interactive_ready"
    printf '\t\e[A\r'
    sleep 0.5
    printf '\x04'
end | timeout 10 script -qfec \
    'env fish_features=no-query-term,no-keyboard-protocols,no-mark-prompt TERM=xterm-256color fish --no-config' \
    "$history_recall_transcript" >/dev/null
set interactive_status $pipestatus[2]
test $interactive_status -eq 0; or fail "history recall transcript exited with status $interactive_status"
assert_equal (cat "$capture_calls") x 'recalled Fish history submits exactly one Mu prompt'
assert_equal (cat "$capture_stdin") "$history_recalled_prompt" 'Fish Up skips shell entries and recalls Mu input'

set history_restore_transcript "$TEST_TMPDIR/history-restore-transcript"
set history_draft 'keep this draft'
rm -f "$capture_args" "$capture_stdin" "$capture_calls" "$interactive_ready"
begin
    sleep 0.2
    send_interactive_setup "$history_setup" "$interactive_ready"
    printf '\t%s\e[A\e[B\r' "$history_draft"
    sleep 0.5
    printf '\x04'
end | timeout 10 script -qfec \
    'env fish_features=no-query-term,no-keyboard-protocols,no-mark-prompt TERM=xterm-256color fish --no-config' \
    "$history_restore_transcript" >/dev/null
set interactive_status $pipestatus[2]
test $interactive_status -eq 0; or fail "history restore transcript exited with status $interactive_status"
assert_equal (cat "$capture_stdin") "$history_draft" 'Fish Down restores the pre-history Mu draft'

set common_prefix_transcript "$TEST_TMPDIR/common-prefix-transcript"
set common_prefix_setup (string join '; ' -- \
    "$interactive_setup" \
    'function _mu_test_common_prefix_completion; commandline -r "/mod"; commandline -C 4; _mu_fish_complete_slash; set first (string join , -- (commandline | string collect) (commandline -C)); commandline -r "/model o"; commandline -C 8; _mu_fish_complete_slash; printf "\n[first=%s later=%s,%s]\n" $first (commandline | string collect) (commandline -C); commandline -r ""; commandline -f repaint; end' \
    'bind -M mumode ctrl-y _mu_test_common_prefix_completion')
rm -f "$capture_args" "$capture_stdin" "$capture_calls" "$interactive_ready"
begin
    sleep 0.2
    send_interactive_setup "$common_prefix_setup" "$interactive_ready"
    printf '\t\x19'
    sleep 0.4
    printf '\x04'
end | timeout 10 script -qfec \
    'env fish_features=no-query-term,no-keyboard-protocols,no-mark-prompt TERM=xterm-256color fish --no-config' \
    "$common_prefix_transcript" >/dev/null
set interactive_status $pipestatus[2]
test $interactive_status -eq 0; or fail "common-prefix completion transcript exited with status $interactive_status"

set raw_transcript (string collect <"$common_prefix_transcript")
assert_contains "$raw_transcript" '[first=/model ,7 later=/model openai/,14]' 'Fish defers model completion until the next Tab and then fills the native common prefix'
not test -s "$capture_calls"; or fail 'common-prefix completion should not submit a prompt'

set model_effort_transcript "$TEST_TMPDIR/model-effort-transcript"
rm -f "$capture_args" "$capture_stdin" "$capture_calls" "$interactive_ready"
begin
    sleep 0.2
    send_interactive_setup "$interactive_setup" "$interactive_ready"
    printf '\t/model gp\t'
    sleep 0.5
    printf '\r'
    sleep 0.3
    printf '\x04'
end | timeout 10 script -qfec \
    'env fish_features=no-query-term,no-keyboard-protocols,no-mark-prompt TERM=xterm-256color fish --no-config' \
    "$model_effort_transcript" >/dev/null
set interactive_status $pipestatus[2]
test $interactive_status -eq 0; or fail "model effort completion transcript exited with status $interactive_status"

set raw_transcript (string collect <"$model_effort_transcript")
assert_contains "$raw_transcript" '/model gpt' 'one Tab completes the Fish model'
assert_contains "$raw_transcript" '/model gpt:' 'one Tab appends the speculative colon'
assert_contains "$raw_transcript" 'gpt:low' 'one Fish model completion immediately lists low effort'
assert_contains "$raw_transcript" 'gpt:high' 'one Fish model completion immediately lists high effort'
not test -s "$capture_calls"; or fail 'model effort completion should not submit a prompt'

set speculative_colon_transcript "$TEST_TMPDIR/speculative-colon-transcript"
set speculative_colon_setup (string join '; ' -- \
    "$interactive_setup" \
    'function _mu_test_speculative_colon_state; commandline -r "/model gpt"; commandline -C 10; _mu_fish_append_speculative_model_colon; _mu_fish_model_colon; set explicit (string join , -- (commandline | string collect) (commandline -C) $MU_FISH_SPECULATIVE_MODEL_COLON); commandline -r "/model gpt"; commandline -C 10; _mu_fish_append_speculative_model_colon; _mu_fish_speculative_backspace; set back (string join , -- (commandline | string collect) (commandline -C) $MU_FISH_SPECULATIVE_MODEL_COLON); commandline -r "/model gpt"; commandline -C 10; _mu_fish_append_speculative_model_colon; _mu_fish_speculative_delete; set delete (string join , -- (commandline | string collect) (commandline -C) $MU_FISH_SPECULATIVE_MODEL_COLON); commandline -r "/model gpt"; commandline -C 10; _mu_fish_append_speculative_model_colon; commandline -C 10; _mu_fish_commit_speculative_model_colon_if_changed; set moved (string join , -- (commandline | string collect) (commandline -C) $MU_FISH_SPECULATIVE_MODEL_COLON); commandline -r "/model gpt"; commandline -C 10; _mu_fish_append_speculative_model_colon; _mu_fish_strip_speculative_model_colon; set entered (string join , -- (commandline | string collect) (commandline -C) $MU_FISH_SPECULATIVE_MODEL_COLON); printf "\n[explicit=%s back=%s delete=%s moved=%s entered=%s]\n" $explicit $back $delete $moved $entered; commandline -r ""; commandline -f repaint; end' \
    'bind -M mumode ctrl-y _mu_test_speculative_colon_state')
rm -f "$interactive_ready"
begin
    sleep 0.2
    send_interactive_setup "$speculative_colon_setup" "$interactive_ready"
    printf '\t\x19'
    sleep 0.4
    printf '\x04'
end | timeout 10 script -qfec \
    'env fish_features=no-query-term,no-keyboard-protocols,no-mark-prompt TERM=xterm-256color fish --no-config' \
    "$speculative_colon_transcript" >/dev/null
set interactive_status $pipestatus[2]
test $interactive_status -eq 0; or fail "speculative colon transcript exited with status $interactive_status"
set raw_transcript (string collect <"$speculative_colon_transcript")
assert_contains "$raw_transcript" '[explicit=/model gpt:,11,0 back=/model gpt,10,0 delete=/model gpt:,11,0 moved=/model gpt:,10,0 entered=/model gpt,10,0]' 'Fish speculative colon actions commit or remove the delimiter as specified'

set shift_transcript "$TEST_TMPDIR/shift-transcript"
rm -f "$capture_args" "$capture_stdin" "$capture_calls" "$interactive_ready"
begin
    sleep 0.2
    send_interactive_setup "$interactive_setup" "$interactive_ready"
    printf '\tfirst line\e[13;2usecond line\r'
    sleep 0.5
    printf '\x04'
end | timeout 10 script -qfec \
    'env fish_features=no-query-term,no-keyboard-protocols,no-mark-prompt TERM=xterm-256color fish --no-config' \
    "$shift_transcript" >/dev/null
set interactive_status $pipestatus[2]
test $interactive_status -eq 0; or fail "Shift+Enter transcript exited with status $interactive_status"
assert_equal (cat "$capture_stdin" | string collect) 'first line
second line' 'Shift+Enter submits one multiline prompt'

set saved_tab_transcript "$TEST_TMPDIR/saved-tab-transcript"
set saved_tab_capture "$TEST_TMPDIR/saved-tab-capture"
set saved_tab_buffer "$TEST_TMPDIR/saved-tab-buffer"
set saved_tab_roundtrip_capture "$TEST_TMPDIR/saved-tab-roundtrip-capture"
rm -f "$saved_tab_capture" "$saved_tab_buffer" "$saved_tab_roundtrip_capture" "$interactive_ready"
set saved_tab_setup_parts \
    "$interactive_setup" \
    "set -gx TEST_SAVED_TAB_CAPTURE "(string escape "$saved_tab_capture") \
    "set -gx TEST_SAVED_TAB_BUFFER "(string escape "$saved_tab_buffer") \
    "set -gx TEST_TAB_ROUNDTRIP_CAPTURE "(string escape "$saved_tab_roundtrip_capture") \
    'function _test_saved_tab; printf x >>$TEST_SAVED_TAB_CAPTURE; commandline -i -- -preserved; commandline >$TEST_SAVED_TAB_BUFFER; end' \
    'function _test_tab_roundtrip; commandline -r -- "printf x >\$TEST_TAB_ROUNDTRIP_CAPTURE"; commandline -C 0; _mu_fish_tab; _mu_fish_tab; end' \
    'bind -M default tab _test_saved_tab' \
    'bind -M default ctrl-t _test_tab_roundtrip' \
    "source "(string escape "$TEST_ROOT/mu.fish")
set saved_tab_setup (string join '; ' $saved_tab_setup_parts)

begin
    sleep 0.2
    send_interactive_setup "$saved_tab_setup" "$interactive_ready"
    printf 'x\t'
    sleep 0.2
    printf '\x03'
    sleep 0.2
    printf '\x14\r'
    sleep 0.3
    printf '\x04'
end | timeout 10 script -qfec \
    'env fish_features=no-query-term,no-keyboard-protocols,no-mark-prompt TERM=xterm-256color fish --no-config' \
    "$saved_tab_transcript" >/dev/null
set interactive_status $pipestatus[2]
test $interactive_status -eq 0; or fail "saved Tab transcript exited with status $interactive_status"
test -f "$saved_tab_capture"; or fail 'shell Tab binding was not delegated'
assert_equal (cat "$saved_tab_capture") x 'shell Tab binding runs exactly once'
assert_equal (cat "$saved_tab_buffer") x-preserved 'shell Tab binding keeps its buffer behavior'
test -f "$saved_tab_roundtrip_capture"; or fail 'Tab mode roundtrip did not execute the preserved shell buffer'
assert_equal (cat "$saved_tab_roundtrip_capture") x 'Tab mode roundtrip executes the preserved shell buffer once'

printf 'ok\n'
