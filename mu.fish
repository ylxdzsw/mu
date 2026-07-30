# Fish integration for Mu.
#
# Source this file near the end of config.fish. Press Tab at cursor position 0
# to toggle Mu prompt mode while preserving the current buffer. Enter submits a
# non-blank turn, Ctrl-C cancels the draft, Ctrl-D keeps Fish's normal EOF
# behavior, and Up/Down move within the current multiline buffer.

set -l _mu_fish_version_major (string split -m1 . "$version")[1]
if not string match -qr '^[0-9]+$' -- "$_mu_fish_version_major"; or test "$_mu_fish_version_major" -lt 4
    printf '%s\n' 'mu: mu.fish requires Fish 4 or newer' >&2
    return 1
end

set -q MU_FISH_MODE; or set -g MU_FISH_MODE shell
set -q MU_FISH_TRACKED_SCOPE; or set -g MU_FISH_TRACKED_SCOPE
set -q MU_FISH_SESSION_ID; or set -g MU_FISH_SESSION_ID
set -q MU_FISH_EFFECTIVE_SESSION_ID; or set -g MU_FISH_EFFECTIVE_SESSION_ID
set -q MU_FISH_MODEL; or set -g MU_FISH_MODEL
set -q MU_FISH_EFFECTIVE_MODEL; or set -g MU_FISH_EFFECTIVE_MODEL
set -q MU_FISH_EFFECTIVE_ATTACHMENT_COUNT; or set -g MU_FISH_EFFECTIVE_ATTACHMENT_COUNT 0
set -q MU_FISH_BIN; or set -g MU_FISH_BIN mu
set -q MU_FISH_OUTPUT; or set -g MU_FISH_OUTPUT
set -q MU_FISH_PROMPT_INPUT; or set -g MU_FISH_PROMPT_INPUT 'mu> '
set -q MU_FISH_PENDING_ATTACHMENTS; or set -g MU_FISH_PENDING_ATTACHMENTS
set -q MU_FISH_SAVED_BIND_MODE; or set -g MU_FISH_SAVED_BIND_MODE default
set -q MU_FISH_ENTER_HOOKS; or set -g MU_FISH_ENTER_HOOKS
set -q MU_FISH_EXIT_HOOKS; or set -g MU_FISH_EXIT_HOOKS
set -q _MU_FISH_DEFAULT_TAB_BINDING; or set -g _MU_FISH_DEFAULT_TAB_BINDING
set -q _MU_FISH_INSERT_TAB_BINDING; or set -g _MU_FISH_INSERT_TAB_BINDING
set -q _MU_FISH_INPUT_FUNCTIONS; or set -g _MU_FISH_INPUT_FUNCTIONS

set -q MU_FISH_PROMPT_MODEL_COLOR; or set -g MU_FISH_PROMPT_MODEL_COLOR cyan
set -q MU_FISH_PROMPT_CONTEXT_COLOR; or set -g MU_FISH_PROMPT_CONTEXT_COLOR magenta
set -q MU_FISH_PROMPT_PWD_COLOR; or set -g MU_FISH_PROMPT_PWD_COLOR brblue
set -q MU_FISH_PROMPT_PROJECT_COLOR; or set -g MU_FISH_PROMPT_PROJECT_COLOR brblack
set -q MU_FISH_PROMPT_UNCLEAN_COLOR; or set -g MU_FISH_PROMPT_UNCLEAN_COLOR brred
set -q MU_FISH_PROMPT_UNCLEAN_TEXT; or set -g MU_FISH_PROMPT_UNCLEAN_TEXT 'interrupted · /retry'

function _mu_fish_linked_project_root --argument-names checkout_root
    test -f "$checkout_root/.git"; or return 1

    set -l pointer
    read -l pointer <"$checkout_root/.git"; or return 1
    string match -qr '^gitdir:[[:space:]]*' -- "$pointer"; or return 1

    set -l git_dir (string replace -r '^gitdir:[[:space:]]*' '' -- "$pointer")
    test -n "$git_dir"; or return 1
    if not string match -q '/*' -- "$git_dir"
        set git_dir "$checkout_root/$git_dir"
    end
    set git_dir (path resolve -- "$git_dir" 2>/dev/null); or return 1

    test -r "$git_dir/commondir"; or return 1
    set -l common_dir
    read -l common_dir <"$git_dir/commondir"; or return 1
    test -n "$common_dir"; or return 1
    if not string match -q '/*' -- "$common_dir"
        set common_dir "$git_dir/$common_dir"
    end
    set common_dir (path resolve -- "$common_dir" 2>/dev/null); or return 1

    test (path basename "$common_dir") = .git; or return 1
    test (path dirname (path dirname "$git_dir")) = "$common_dir"; or return 1
    path dirname "$common_dir"
end

function _mu_fish_scope_for_dir --argument-names initial_dir
    set -l dir $initial_dir
    while test -n "$dir"
        if set -q HOME; and test "$dir" = "$HOME"
            break
        end
        test "$dir" = /; and break

        if test -d "$dir/.mu"
            printf 'project:%s\n' "$dir"
            return 0
        end
        if test -e "$dir/.git"
            set -l project_root $dir
            set -l linked_root (_mu_fish_linked_project_root "$dir")
            and set project_root $linked_root
            printf 'project:%s\n' "$project_root"
            return 0
        end

        set -l parent (path dirname "$dir")
        test -z "$parent"; and break
        test "$parent" = "$dir"; and break
        set dir $parent
    end

    printf 'global\n'
end

function _mu_fish_current_scope
    _mu_fish_scope_for_dir "$PWD"
end

function _mu_fish_sync_state --argument-names requested_scope
    set -l scope $requested_scope
    test -n "$scope"; or set scope (_mu_fish_current_scope)

    set -l has_state 0
    if set -q MU_FISH_SESSION_ID[1]; and test -n "$MU_FISH_SESSION_ID"
        set has_state 1
    else if set -q MU_FISH_MODEL[1]; and test -n "$MU_FISH_MODEL"
        set has_state 1
    else if set -q MU_FISH_PENDING_ATTACHMENTS[1]
        set has_state 1
    end

    if test $has_state -eq 1
        and begin
            not set -q MU_FISH_TRACKED_SCOPE[1]; or test -z "$MU_FISH_TRACKED_SCOPE"
        end
        set -g MU_FISH_TRACKED_SCOPE $scope
    end

    if set -q MU_FISH_TRACKED_SCOPE[1]
        and test -n "$MU_FISH_TRACKED_SCOPE"
        and test "$MU_FISH_TRACKED_SCOPE" = "$scope"
        set -g MU_FISH_EFFECTIVE_SESSION_ID $MU_FISH_SESSION_ID
        set -g MU_FISH_EFFECTIVE_MODEL $MU_FISH_MODEL
        set -g MU_FISH_EFFECTIVE_ATTACHMENT_COUNT (count $MU_FISH_PENDING_ATTACHMENTS)
    else
        set -g MU_FISH_EFFECTIVE_SESSION_ID
        set -g MU_FISH_EFFECTIVE_MODEL
        set -g MU_FISH_EFFECTIVE_ATTACHMENT_COUNT 0
    end
    return 0
end

function _mu_fish_clear_session_state
    set -g MU_FISH_SESSION_ID
    set -g MU_FISH_EFFECTIVE_SESSION_ID
end

function _mu_fish_clear_model_state
    set -g MU_FISH_MODEL
    set -g MU_FISH_EFFECTIVE_MODEL
end

function _mu_fish_clear_tracked_state
    _mu_fish_clear_session_state
    _mu_fish_clear_model_state
    set -g MU_FISH_PENDING_ATTACHMENTS
    set -g MU_FISH_TRACKED_SCOPE
    set -g MU_FISH_EFFECTIVE_ATTACHMENT_COUNT 0
end

function _mu_fish_activate_scope --argument-names scope
    if set -q MU_FISH_TRACKED_SCOPE[1]
        and test -n "$MU_FISH_TRACKED_SCOPE"
        and test "$MU_FISH_TRACKED_SCOPE" != "$scope"
        _mu_fish_clear_tracked_state
    end
    set -g MU_FISH_TRACKED_SCOPE $scope
    _mu_fish_sync_state "$scope"
end

function _mu_fish_remember_session_for_scope --argument-names id requested_scope
    test -n "$id"; or return 0
    set -l scope $requested_scope
    test -n "$scope"; or set scope (_mu_fish_current_scope)
    _mu_fish_activate_scope "$scope"
    set -g MU_FISH_SESSION_ID $id
    set -g MU_FISH_EFFECTIVE_SESSION_ID $id
end

function _mu_fish_base_command --argument-names requested_scope
    set -l scope $requested_scope
    test -n "$scope"; or set scope (_mu_fish_current_scope)
    _mu_fish_sync_state "$scope"

    printf '%s\n' "$MU_FISH_BIN"
    set -q MU_FISH_OUTPUT[1]; and test -n "$MU_FISH_OUTPUT"; and printf '%s\n' --output "$MU_FISH_OUTPUT"
    set -q MU_FISH_EFFECTIVE_SESSION_ID[1]; and test -n "$MU_FISH_EFFECTIVE_SESSION_ID"; and printf '%s\n' -s "$MU_FISH_EFFECTIVE_SESSION_ID"
    set -q MU_FISH_EFFECTIVE_MODEL[1]; and test -n "$MU_FISH_EFFECTIVE_MODEL"; and printf '%s\n' --model "$MU_FISH_EFFECTIVE_MODEL"
    return 0
end

function _mu_fish_status_json
    _mu_fish_sync_state
    set -l command "$MU_FISH_BIN" status --json $argv
    set -q MU_FISH_EFFECTIVE_SESSION_ID[1]; and test -n "$MU_FISH_EFFECTIVE_SESSION_ID"; and set -a command -s "$MU_FISH_EFFECTIVE_SESSION_ID"
    set -q MU_FISH_EFFECTIVE_MODEL[1]; and test -n "$MU_FISH_EFFECTIVE_MODEL"; and set -a command --model "$MU_FISH_EFFECTIVE_MODEL"

    set -l json ($command 2>/dev/null | string collect)
    set -l command_status $pipestatus[1]
    test $command_status -eq 0; or return $command_status
    printf '%s' "$json"
end

function _mu_fish_print_block_message --argument-names message
    printf '%s\n\n' "$message"
end

function _mu_fish_create_session_for_scope --argument-names scope
    set -l command "$MU_FISH_BIN"
    set -l id ($command session new | string collect)
    set -l command_status $pipestatus[1]
    test $command_status -eq 0; or return $command_status
    set id (string replace -a \n '' -- "$id")
    if not string match -qr '^ses_[0-9a-hjkmnpqrstvwxyz]{8}$' -- "$id"
        printf '%s\n' 'mu: session new returned an invalid session id' >&2
        return 1
    end
    _mu_fish_remember_session_for_scope "$id" "$scope"
end

function _mu_fish_append_history --argument-names entry
    builtin history append "$entry"
end

function _mu_fish_record_turn_history --argument-names input
    set -e argv[1]
    set -l command $argv
    set -l escaped_command
    for argument in $command
        set -a escaped_command (string escape -- "$argument")
    end
    set -l escaped_input (string escape -- "$input")
    set -l replay "printf '%s\\n' $escaped_input | "(string join ' ' -- $escaped_command)
    _mu_fish_append_history "$replay"
end

function _mu_fish_build_mode_prompt
    _mu_fish_sync_state
    set -l status_json (_mu_fish_status_json | string collect)
    set -l fields
    if test -n "$status_json"; and command -q jq
        set fields (printf '%s' "$status_json" | jq -r '[
            (.model.canonical // ""),
            (.context_percent // ""),
            (.project_root // ""),
            (if has("clean") then (.clean|tostring) else "" end),
            (.context_usage_source // "")
        ] | @tsv' 2>/dev/null | string split \t)
    end

    set -l model mu
    set -q fields[1]; and test -n "$fields[1]"; and set model "$fields[1]"
    set -l context_raw
    set -q fields[2]; and set context_raw "$fields[2]"
    set -l project_root
    set -q fields[3]; and set project_root "$fields[3]"
    set -l clean
    set -q fields[4]; and set clean "$fields[4]"
    set -l context_source
    set -q fields[5]; and set context_source "$fields[5]"

    set_color "$MU_FISH_PROMPT_MODEL_COLOR"
    printf '%s' "$model"
    set_color normal

    if set -q MU_FISH_EFFECTIVE_SESSION_ID[1]; and test -n "$MU_FISH_EFFECTIVE_SESSION_ID"
        set -l context 0%
        if string match -qr '^-?[0-9]+([.][0-9]+)?$' -- "$context_raw"
            set context (printf '%.0f%%' "$context_raw")
        end
        test "$context_source" = estimated; and set context "~$context"
        printf ' '
        set_color "$MU_FISH_PROMPT_CONTEXT_COLOR"
        printf '%s' "$context"
        set_color normal
    end

    printf ' '
    set_color "$MU_FISH_PROMPT_PWD_COLOR"
    printf '%s' "$PWD"
    set_color normal

    if test -z "$project_root"
        printf ' '
        set_color "$MU_FISH_PROMPT_PROJECT_COLOR"
        printf '(global)'
        set_color normal
    else if test "$project_root" != "$PWD"
        printf ' '
        set_color "$MU_FISH_PROMPT_PROJECT_COLOR"
        printf '(%s)' "$project_root"
        set_color normal
    end

    if test "$clean" = false
        printf ' '
        set_color "$MU_FISH_PROMPT_UNCLEAN_COLOR"
        printf '[%s]' "$MU_FISH_PROMPT_UNCLEAN_TEXT"
        set_color normal
    end

    if test "$MU_FISH_EFFECTIVE_ATTACHMENT_COUNT" -gt 0
        printf ' '
        set_color "$MU_FISH_PROMPT_CONTEXT_COLOR"
        printf '[%d attachments]' "$MU_FISH_EFFECTIVE_ATTACHMENT_COUNT"
        set_color normal
    end

    printf '\n%s' "$MU_FISH_PROMPT_INPUT"
end

function _mu_fish_run_hooks
    for hook in $argv
        test -n "$hook"; or continue
        if functions -q "$hook"
            $hook
        else
            printf 'mu mu.fish: hook function not found: %s\n' "$hook" >&2
        end
    end
end

function _mu_fish_capture_tab_binding --argument-names mode variable_name
    set -l binding (bind --user -M "$mode" tab 2>/dev/null | string collect)
    if test -z "$binding"
        set binding (bind --preset -M "$mode" tab 2>/dev/null | string collect)
    end

    set -l commands
    if test -n "$binding"
        set binding (string replace -r '^bind( --preset)?( -M [^[:space:]]+)? tab[[:space:]]+' '' -- "$binding")
        printf '%s\n' "$binding" | read --tokenize -a commands
    end
    test (count $commands) -gt 0; or set commands complete

    # Re-sourcing our own wrapper must not replace the saved user binding.
    contains -- _mu_fish_tab $commands; and return 0
    set -g $variable_name $commands
end

function _mu_fish_call_saved_tab --argument-names mode
    set -l _mu_fish_saved_commands
    switch "$mode"
        case insert
            set _mu_fish_saved_commands $_MU_FISH_INSERT_TAB_BINDING
        case '*'
            set _mu_fish_saved_commands $_MU_FISH_DEFAULT_TAB_BINDING
    end
    test (count $_mu_fish_saved_commands) -gt 0; or set _mu_fish_saved_commands complete

    # Fish bindings may contain editor functions or arbitrary Fish commands.
    for _mu_fish_saved_action in $_mu_fish_saved_commands
        if contains -- "$_mu_fish_saved_action" $_MU_FISH_INPUT_FUNCTIONS
            commandline -f "$_mu_fish_saved_action"
        else
            eval "$_mu_fish_saved_action"
        end
    end
end

function _mu_fish_has_effective_session
    _mu_fish_sync_state
    set -q MU_FISH_EFFECTIVE_SESSION_ID[1]; and test -n "$MU_FISH_EFFECTIVE_SESSION_ID"
end

function _mu_fish_custom_slash_commands
    set -l json (_mu_fish_status_json --include-commands | string collect); or return 1
    command -q jq; or return 1
    printf '%s' "$json" | jq -r '.commands[]?.name | "/" + .' 2>/dev/null
end

function _mu_fish_has_custom_slash_command --argument-names requested_command
    for candidate in (_mu_fish_custom_slash_commands 2>/dev/null)
        test "$candidate" = "$requested_command"; and return 0
    end
    return 1
end

function _mu_fish_slash_command_candidates
    printf '%s\n' /attach /model
    _mu_fish_has_effective_session; and printf '%s\n' /new /retry /compact
    _mu_fish_custom_slash_commands 2>/dev/null
end

function _mu_fish_model_records
    set -l json (_mu_fish_status_json --include-models | string collect); or return 1
    command -q jq; or return 1
    printf '%s' "$json" | jq -r '
        .available_models.providers[]? as $provider
        | $provider.models[]?
        | [(.id // ""), (.model_id // ""), ((.supported_efforts // []) | join(","))]
        | @tsv
    ' 2>/dev/null
end

function _mu_fish_model_candidates --argument-names fragment
    set -e argv[1]
    set -l records $argv
    test (count $records) -gt 0; or set records (_mu_fish_model_records 2>/dev/null)
    test (count $records) -gt 0; or return 0

    set -l model_ids
    for record in $records
        set -l fields (string split \t -- "$record")
        set -q fields[2]; and test -n "$fields[2]"; and set -a model_ids "$fields[2]"
    end

    set -l matches
    for record in $records
        set -l fields (string split \t -- "$record")
        set -l canonical "$fields[1]"
        set -l model_id "$fields[2]"
        set -l efforts
        set -q fields[3]; and set efforts (string split , -- "$fields[3]")
        set -l id_count 0
        test -n "$model_id"; and set id_count (count (string match -- "$model_id" $model_ids))

        if string match -q '*:*' -- "$fragment"
            for effort in $efforts
                test -n "$effort"; or continue
                set -a matches "$canonical:$effort"
                test -n "$model_id"; and test $id_count -eq 1; and set -a matches "$model_id:$effort"
            end
        else
            test -n "$canonical"; and set -a matches "$canonical"
            test -n "$model_id"; and test $id_count -eq 1; and set -a matches "$model_id"
        end
    end

    printf '%s\n' $matches | sort -u
end

function _mu_fish_model_effort_suffixes --argument-names fragment
    set -e argv[1]
    test -n "$fragment"; or return 0
    string match -q '*:*' -- "$fragment"; and return 0

    set -l records $argv
    test (count $records) -gt 0; or set records (_mu_fish_model_records 2>/dev/null)
    set -l model_ids
    for record in $records
        set -l fields (string split \t -- "$record")
        set -q fields[2]; and test -n "$fields[2]"; and set -a model_ids "$fields[2]"
    end

    for record in $records
        set -l fields (string split \t -- "$record")
        set -l canonical "$fields[1]"
        set -l model_id "$fields[2]"
        set -l id_count 0
        test -n "$model_id"; and set id_count (count (string match -- "$model_id" $model_ids))
        if test "$fragment" != "$canonical"
            and begin
                test "$fragment" != "$model_id"; or test $id_count -ne 1
            end
            continue
        end
        set -q fields[3]; or return 0
        for effort in (string split , -- "$fields[3]")
            test -n "$effort"; and printf ':%s\n' "$effort"
        end
        return 0
    end
end

function _mu_fish_native_model_candidates
    set -l records (_mu_fish_model_records 2>/dev/null)
    _mu_fish_model_candidates '' $records
    _mu_fish_model_candidates : $records
end

function _mu_fish_install_model_completion
    complete -e -p /model
    complete -k -f -p /model -a '(_mu_fish_native_model_candidates)'
end

function _mu_fish_remove_model_completion
    complete -e -p /model
end

function _mu_fish_require_effective_session --argument-names slash_command
    _mu_fish_sync_state
    if not set -q MU_FISH_EFFECTIVE_SESSION_ID[1]; or test -z "$MU_FISH_EFFECTIVE_SESSION_ID"
        _mu_fish_print_block_message "[mu] $slash_command requires an active session in this scope"
        return 1
    end
end

function _mu_fish_validate_model_ref --argument-names model
    _mu_fish_sync_state
    set -l command "$MU_FISH_BIN" status --json --model "$model"
    set -q MU_FISH_EFFECTIVE_SESSION_ID[1]; and test -n "$MU_FISH_EFFECTIVE_SESSION_ID"; and set -a command -s "$MU_FISH_EFFECTIVE_SESSION_ID"
    set -l json ($command 2>/dev/null | string collect)
    set -l command_status $pipestatus[1]
    test $command_status -eq 0; or return $command_status
    set -g _MU_FISH_VALIDATED_MODEL (printf '%s' "$json" | jq -r '.model.canonical // empty' 2>/dev/null)
    test -n "$_MU_FISH_VALIDATED_MODEL"; or set -g _MU_FISH_VALIDATED_MODEL "$model"
end

function _mu_fish_run_custom_slash_command --argument-names slash_command instruction
    set -l scope (_mu_fish_current_scope)
    _mu_fish_activate_scope "$scope"
    if not set -q MU_FISH_EFFECTIVE_SESSION_ID[1]; or test -z "$MU_FISH_EFFECTIVE_SESSION_ID"
        _mu_fish_create_session_for_scope "$scope"; or return $status
    end

    set -l command (_mu_fish_base_command "$scope")
    for attachment in $MU_FISH_PENDING_ATTACHMENTS
        set -a command -a "$attachment"
    end
    set -a command (string replace -r '^/' '' -- "$slash_command")
    set -g MU_FISH_PENDING_ATTACHMENTS
    set -g MU_FISH_EFFECTIVE_ATTACHMENT_COUNT 0

    if test -n "$instruction"
        printf '%s' "$instruction" | $command
        return $pipestatus[2]
    end
    $command
end

function _mu_fish_run_slash_command --argument-names line
    set -l slash_command (string match -r -g '^([^[:space:]]+)' -- "$line")
    set -l command_length (string length -- "$slash_command")
    set -l instruction
    if test (string length -- "$line") -gt $command_length
        set instruction (string sub --start (math $command_length + 2) -- "$line" | string collect)
    end
    set -l rest (string trim -- "$instruction")
    set -l exit_status 0

    _mu_fish_append_history "$line"
    set -l scope (_mu_fish_current_scope)

    switch "$slash_command"
        case /attach
            if test -z "$rest"
                _mu_fish_activate_scope "$scope"
                if set -q MU_FISH_PENDING_ATTACHMENTS[1]
                    _mu_fish_print_block_message "[mu] pending attachments: "(string join ', ' -- $MU_FISH_PENDING_ATTACHMENTS)
                else
                    _mu_fish_print_block_message '[mu] no pending attachments'
                end
                return 0
            end
            if test "$rest" = --clear
                _mu_fish_activate_scope "$scope"
                set -g MU_FISH_PENDING_ATTACHMENTS
                set -g MU_FISH_EFFECTIVE_ATTACHMENT_COUNT 0
                _mu_fish_print_block_message '[mu] cleared pending attachments'
                return 0
            end
            if string match -q "*\n*" -- "$rest"
                _mu_fish_print_block_message '[mu] /attach accepts exactly one file'
                return 1
            end
            set -l attachment_path "$rest"
            if string match -q '~/*' -- "$attachment_path"
                set attachment_path "$HOME/"(string replace -r '^~/' '' -- "$attachment_path")
            end
            if not test -f "$attachment_path"; or not test -r "$attachment_path"
                _mu_fish_print_block_message "[mu] attachment is not a readable file: $rest"
                return 1
            end
            set attachment_path (path resolve -- "$attachment_path")
            _mu_fish_activate_scope "$scope"
            set -ga MU_FISH_PENDING_ATTACHMENTS "$attachment_path"
            set -l count (count $MU_FISH_PENDING_ATTACHMENTS)
            set -g MU_FISH_EFFECTIVE_ATTACHMENT_COUNT $count
            set -l label files
            test $count -eq 1; and set label file
            _mu_fish_print_block_message "[mu] attached "(path basename "$attachment_path")" for the next message ($count $label)"

        case /model
            if test -z "$rest"
                _mu_fish_print_block_message '[mu] usage: /model <model>'
                return 1
            end
            if string match -qr '[[:space:]]' -- "$rest"
                _mu_fish_print_block_message '[mu] /model accepts exactly one model reference'
                return 1
            end
            if not _mu_fish_validate_model_ref "$rest"
                _mu_fish_print_block_message "[mu] unknown or unsupported model: $rest"
                return 1
            end
            _mu_fish_activate_scope "$scope"
            set -g MU_FISH_MODEL "$_MU_FISH_VALIDATED_MODEL"
            set -g MU_FISH_EFFECTIVE_MODEL "$_MU_FISH_VALIDATED_MODEL"
            _mu_fish_print_block_message "[mu] next turns in this scope will use $_MU_FISH_VALIDATED_MODEL"

        case /new
            if test -n "$rest"
                _mu_fish_print_block_message '[mu] /new does not accept arguments'
                return 1
            end
            _mu_fish_activate_scope "$scope"
            _mu_fish_require_effective_session /new; or return 1
            _mu_fish_clear_session_state
            _mu_fish_print_block_message '[mu] next turn will start a new session'

        case /retry
            if test -n "$rest"
                _mu_fish_print_block_message '[mu] /retry does not accept arguments'
                return 1
            end
            _mu_fish_activate_scope "$scope"
            _mu_fish_require_effective_session /retry; or return 1
            set -l retry_command "$MU_FISH_BIN" retry -s "$MU_FISH_EFFECTIVE_SESSION_ID"
            set -q MU_FISH_EFFECTIVE_MODEL[1]; and test -n "$MU_FISH_EFFECTIVE_MODEL"; and set -a retry_command --model "$MU_FISH_EFFECTIVE_MODEL"
            set -q MU_FISH_OUTPUT[1]; and test -n "$MU_FISH_OUTPUT"; and set -a retry_command --output "$MU_FISH_OUTPUT"
            $retry_command
            set exit_status $status

        case /compact
            _mu_fish_activate_scope "$scope"
            _mu_fish_require_effective_session /compact; or return 1
            if test -n "$instruction"
                printf '%s' "$instruction" | "$MU_FISH_BIN" compact --session "$MU_FISH_EFFECTIVE_SESSION_ID"
                set exit_status $pipestatus[2]
            else
                "$MU_FISH_BIN" compact --session "$MU_FISH_EFFECTIVE_SESSION_ID"
                set exit_status $status
            end
            printf '\n'

        case '*'
            if _mu_fish_has_custom_slash_command "$slash_command"
                _mu_fish_run_custom_slash_command "$slash_command" "$instruction"
                set exit_status $status
            else
                _mu_fish_print_block_message "[mu] unknown slash command: $slash_command"
                return 1
            end
    end

    _mu_fish_sync_state "$scope"
    return $exit_status
end

function _mu_fish_submit_prompt --argument-names input
    set -l scope (_mu_fish_current_scope)
    _mu_fish_activate_scope "$scope"
    if not set -q MU_FISH_EFFECTIVE_SESSION_ID[1]; or test -z "$MU_FISH_EFFECTIVE_SESSION_ID"
        _mu_fish_create_session_for_scope "$scope"; or return $status
    end

    set -l command (_mu_fish_base_command "$scope")
    for attachment in $MU_FISH_PENDING_ATTACHMENTS
        set -a command -a "$attachment"
    end
    _mu_fish_record_turn_history "$input" $command
    set -g MU_FISH_PENDING_ATTACHMENTS
    set -g MU_FISH_EFFECTIVE_ATTACHMENT_COUNT 0

    printf '%s\n' "$input" | $command
    return $pipestatus[2]
end

function _mu_fish_common_prefix
    test (count $argv) -gt 0; or return 0
    set -l prefix "$argv[1]"
    for candidate in $argv[2..-1]
        while test -n "$prefix"
            set -l candidate_prefix (string sub --length (string length -- "$prefix") -- "$candidate")
            test "$candidate_prefix" = "$prefix"; and break
            set prefix (string sub --end -1 -- "$prefix")
        end
        test -n "$prefix"; or break
    end
    printf '%s' "$prefix"
end

function _mu_fish_matching_candidates --argument-names fragment
    set -e argv[1]
    set -l fragment_length (string length -- "$fragment")
    set -l folded_fragment (string lower -- "$fragment")
    for candidate in $argv
        set -l candidate_prefix (string sub --length $fragment_length -- "$candidate")
        if test (string lower -- "$candidate_prefix") = "$folded_fragment"
            printf '%s\n' "$candidate"
        end
    end
end

function _mu_fish_list_candidates
    test (count $argv) -gt 0; or return 1
    printf '\n%s\n' (string join '  ' -- $argv)
    commandline -f repaint
end

function _mu_fish_complete_values --argument-names prefix fragment suffix
    set -e argv[1..3]
    set -l candidates (_mu_fish_matching_candidates "$fragment" $argv)
    test (count $candidates) -gt 0; or return 1

    set -l replacement
    if test (count $candidates) -eq 1
        set replacement "$candidates[1]$suffix"
    else
        set replacement (_mu_fish_common_prefix $candidates)
    end

    if test (string length -- "$replacement") -gt (string length -- "$fragment")
        set -l buffer (commandline)
        set -l cursor (commandline -C)
        set -l right (string sub --start (math $cursor + 1) -- "$buffer")
        set -l left "$prefix$replacement"
        commandline -r -- "$left$right"
        commandline -C (string length -- "$left")
        return 0
    end

    _mu_fish_list_candidates $candidates
end

function _mu_fish_complete_slash
    set -l buffer (commandline)
    set -l cursor (commandline -C)
    set -l left (string sub --length $cursor -- "$buffer")

    if string match -q '/attach *' -- "$left"
        set -l fragment (string replace -r '^/attach ' '' -- "$left")
        set -l escaped_fragment (string escape -- "$fragment")
        set -l raw_candidates (complete -C "cat $escaped_fragment")
        set -l candidates
        for candidate in $raw_candidates
            set -a candidates (string split -m1 \t -- "$candidate")[1]
        end
        _mu_fish_complete_values '/attach ' "$fragment" '' $candidates
        return
    end

    if string match -q '/model *' -- "$left"
        commandline -f complete
        return
    end

    if string match -q '/*' -- "$left"; and not string match -qr '[[:space:]]' -- "$left"
        set -l candidates (_mu_fish_slash_command_candidates)
        _mu_fish_complete_values '' "$left" ' ' $candidates
        return
    end

    commandline -i \t
end

function _mu_fish_enter_mode
    test "$MU_FISH_MODE" = mu; and return 0
    set -g MU_FISH_MODE mu
    set -g MU_FISH_SAVED_BIND_MODE $fish_bind_mode
    test -n "$MU_FISH_SAVED_BIND_MODE"; or set -g MU_FISH_SAVED_BIND_MODE default
    set fish_bind_mode mumode
    _mu_fish_install_model_completion
    _mu_fish_run_hooks $MU_FISH_ENTER_HOOKS
end

function _mu_fish_exit_mode
    test "$MU_FISH_MODE" = shell; and return 0
    set -g MU_FISH_MODE shell
    set fish_bind_mode "$MU_FISH_SAVED_BIND_MODE"
    _mu_fish_remove_model_completion
    _mu_fish_run_hooks $MU_FISH_EXIT_HOOKS
end

function _mu_fish_tab
    set -l cursor (commandline -C)
    if test "$MU_FISH_MODE" = mu
        set -l buffer (commandline)
        set -l left (string sub --length $cursor -- "$buffer")
        if string match -q '/*' -- "$left"
            _mu_fish_complete_slash
            return
        end
        if test $cursor -eq 0
            _mu_fish_exit_mode
            commandline -f repaint
            return
        end
        commandline -i \t
        return
    end

    if test $cursor -eq 0
        _mu_fish_enter_mode
        commandline -f repaint
        return
    end
    _mu_fish_call_saved_tab "$fish_bind_mode"
end

function _mu_fish_slash
    set -l should_list 0
    test "$MU_FISH_MODE" = mu; and test (commandline -C) -eq 0; and set should_list 1
    commandline -i /
    if test $should_list -eq 1
        set -l candidates (_mu_fish_slash_command_candidates)
        _mu_fish_list_candidates $candidates
    end
end

function _mu_fish_insert_newline
    commandline -i \n
end

function _mu_fish_accept
    set -l input (commandline | string collect)
    if string match -qr '^[[:space:]]*$' -- "$input"
        commandline -f execute
        return
    end

    # Advance while the draft is still rendered so it remains in scrollback,
    # then clear Fish's executable buffer before natural language can reach its
    # parser. The second newline supplies the one empty line before Mu output.
    printf '\n'
    commandline -r ''
    printf '\n'
    if string match -q '/*' -- "$input"
        _mu_fish_run_slash_command "$input"
    else
        _mu_fish_submit_prompt "$input"
    end
    commandline -f repaint
end

function mu-fish-mode
    _mu_fish_enter_mode
    commandline -f repaint
end

function mu-fish-exit-mode
    _mu_fish_exit_mode
    commandline -f repaint
end

function _mu_fish_configure_keymap
    set -g _MU_FISH_INPUT_FUNCTIONS (bind --function-names)
    _mu_fish_capture_tab_binding default _MU_FISH_DEFAULT_TAB_BINDING
    _mu_fish_capture_tab_binding insert _MU_FISH_INSERT_TAB_BINDING

    # Use Fish's complete Emacs-style editing set inside the dedicated mode,
    # then narrow only the keys whose meaning Mu owns.
    fish_default_key_bindings -M mumode
    bind -M mumode '' self-insert
    bind -M mumode enter _mu_fish_accept
    bind -M mumode tab _mu_fish_tab
    bind -M mumode / _mu_fish_slash
    bind -M mumode ctrl-c cancel-commandline
    bind -M mumode ctrl-d delete-or-exit
    bind -M mumode up up-line
    bind -M mumode down down-line
    bind -M mumode \e\[13\;2u _mu_fish_insert_newline

    # Fish uses default for Emacs and vi command mode, and insert for vi insert
    # mode. The handler delegates ordinary completion away from cursor zero.
    bind -M default tab _mu_fish_tab
    bind -M insert tab _mu_fish_tab
end

set -l _mu_fish_current_prompt (functions fish_prompt | string collect)
if not string match -q '*_mu_fish_build_mode_prompt*' -- "$_mu_fish_current_prompt"
    functions -e _mu_fish_original_prompt 2>/dev/null
    functions --copy fish_prompt _mu_fish_original_prompt
end
if functions -q fish_right_prompt
    set -l _mu_fish_current_right_prompt (functions fish_right_prompt | string collect)
    if not string match -q '*_mu_fish_original_right_prompt*' -- "$_mu_fish_current_right_prompt"
        functions -e _mu_fish_original_right_prompt 2>/dev/null
        functions --copy fish_right_prompt _mu_fish_original_right_prompt
    end
else
    functions -e _mu_fish_original_right_prompt 2>/dev/null
    function _mu_fish_original_right_prompt
    end
end
if functions -q fish_mode_prompt
    set -l _mu_fish_current_mode_prompt (functions fish_mode_prompt | string collect)
    if not string match -q '*_mu_fish_original_mode_prompt*' -- "$_mu_fish_current_mode_prompt"
        functions -e _mu_fish_original_mode_prompt 2>/dev/null
        functions --copy fish_mode_prompt _mu_fish_original_mode_prompt
    end
else
    functions -e _mu_fish_original_mode_prompt 2>/dev/null
    function _mu_fish_original_mode_prompt
    end
end

function fish_prompt
    # `switch` does not overwrite $status or $pipestatus. The original prompt
    # therefore sees the command's real result rather than the result of a
    # wrapper-side `test`.
    switch "$MU_FISH_MODE"
        case mu
            _mu_fish_build_mode_prompt
        case '*'
            _mu_fish_original_prompt
    end
end

function fish_right_prompt
    switch "$MU_FISH_MODE"
        case mu
            return 0
        case '*'
            _mu_fish_original_right_prompt
    end
end

function fish_mode_prompt
    switch "$MU_FISH_MODE"
        case mu
            return 0
        case '*'
            _mu_fish_original_mode_prompt
    end
end

_mu_fish_sync_state
if test "$MU_FISH_MODE" = mu
    _mu_fish_install_model_completion
else
    _mu_fish_remove_model_completion
end
status is-interactive; and _mu_fish_configure_keymap
