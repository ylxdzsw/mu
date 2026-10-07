# zsh integration for mu.
#
# Source this file from .zshrc to add a shell-native mu prompt mode:
# press Tab at cursor position 0 to toggle "mu>" mode while preserving the
# current buffer, Enter to submit one non-blank mu turn, Ctrl+C to cancel the
# current mu prompt while leaving the typed line in scrollback, Ctrl+D to keep
# normal shell EOF behavior even from "mu>" mode, and Up/Down to move through
# multiline input before browsing earlier Mu submissions.
# MU_ZSH_SESSION_ID is the only supported variable seed. Underscored variables
# below are private implementation state, not configuration inputs.

typeset -g _MU_ZSH_MODE=shell
typeset -g _MU_ZSH_TRACKED_SCOPE=
typeset -g MU_ZSH_SESSION_ID=${MU_ZSH_SESSION_ID:-}
typeset -g _MU_ZSH_MODEL=
typeset -g _MU_ZSH_TRAP=
typeset -g _MU_ZSH_PENDING_INPUT=
typeset -g _MU_ZSH_PENDING_PROMPT=
typeset -g _MU_ZSH_SPECULATIVE_MODEL_BUFFER=
typeset -g _MU_ZSH_ORIGINAL_PROMPT=
typeset -g _MU_ZSH_ORIGINAL_RPROMPT=
typeset -g _MU_ZSH_SAVED_KEYMAP=main
typeset -g _MU_ZSH_ORIGINAL_TAB_WIDGET
typeset -g _MU_ZSH_ORIGINAL_SLASH_WIDGET
typeset -gi _MU_ZSH_HISTORY_EVENT=0
typeset -gi _MU_ZSH_HISTORY_LATEST_EVENT=0
typeset -g _MU_ZSH_HISTORY_DRAFT=
typeset -gi _MU_ZSH_HISTORY_DRAFT_CURSOR=0
typeset -gi _MU_ZSH_HAD_HIGHLIGHTERS=0
typeset -gi _MU_ZSH_DISABLED_AUTOSUGGESTIONS=0
typeset -ga _MU_ZSH_SAVED_HIGHLIGHTERS=()

_mu_zsh_save_widget_bindings() {
  local binding
  if [[ -z "$_MU_ZSH_ORIGINAL_TAB_WIDGET" ]]; then
    binding=${${(z)$(bindkey '^I' 2>/dev/null)}[2]}
    [[ "$binding" != _mu_zsh_tab ]] && _MU_ZSH_ORIGINAL_TAB_WIDGET=$binding
  fi
  [[ -z "$_MU_ZSH_ORIGINAL_TAB_WIDGET" ]] && _MU_ZSH_ORIGINAL_TAB_WIDGET=expand-or-complete

  if [[ -z "$_MU_ZSH_ORIGINAL_SLASH_WIDGET" ]]; then
    binding=${${(z)$(bindkey '/' 2>/dev/null)}[2]}
    [[ "$binding" != _mu_zsh_slash ]] && _MU_ZSH_ORIGINAL_SLASH_WIDGET=$binding
  fi
  [[ -z "$_MU_ZSH_ORIGINAL_SLASH_WIDGET" ]] && _MU_ZSH_ORIGINAL_SLASH_WIDGET=.self-insert
  return 0
}

_mu_zsh_linked_project_root() {
  local checkout_root=$1
  local pointer git_dir common_dir

  [[ -f "$checkout_root/.git" ]] || return 1
  IFS= read -r pointer < "$checkout_root/.git" || return 1
  [[ "$pointer" == gitdir:* ]] || return 1
  git_dir=${pointer#gitdir:}
  git_dir=${git_dir# }
  [[ -n "$git_dir" ]] || return 1
  [[ "$git_dir" == /* ]] || git_dir="$checkout_root/$git_dir"
  git_dir=${git_dir:A}

  [[ -r "$git_dir/commondir" ]] || return 1
  IFS= read -r common_dir < "$git_dir/commondir" || return 1
  [[ -n "$common_dir" ]] || return 1
  [[ "$common_dir" == /* ]] || common_dir="$git_dir/$common_dir"
  common_dir=${common_dir:A}

  [[ "${common_dir:t}" == .git ]] || return 1
  [[ "${git_dir:h:h}" == "$common_dir" ]] || return 1
  REPLY=${common_dir:h}
}

_mu_zsh_set_scope_key_for_dir() {
  local dir=$1
  local home=${HOME:-}
  local parent project_root

  while [[ -n "$dir" ]]; do
    if [[ -n "$home" && "$dir" == "$home" ]]; then
      break
    fi
    if [[ "$dir" == "/" ]]; then
      break
    fi
    if [[ -d "$dir/.mu" ]]; then
      REPLY="project:$dir"
      return 0
    fi
    if [[ -e "$dir/.git" ]]; then
      project_root=$dir
      if _mu_zsh_linked_project_root "$dir"; then
        project_root=$REPLY
      fi
      REPLY="project:$project_root"
      return 0
    fi
    parent=${dir:h}
    [[ -z "$parent" || "$parent" == "$dir" ]] && break
    dir=$parent
  done

  REPLY=global
}

_mu_zsh_bundle_active() {
  local scope=${1:-}
  [[ -n "$scope" ]] || {
    _mu_zsh_set_scope_key_for_dir "$PWD"
    scope=$REPLY
  }
  [[ -n "$_MU_ZSH_TRACKED_SCOPE" && "$_MU_ZSH_TRACKED_SCOPE" == "$scope" ]]
}

_mu_zsh_adopt_seeded_bundle() {
  [[ -z "$_MU_ZSH_TRACKED_SCOPE" ]] || return 0
  [[ -n "$MU_ZSH_SESSION_ID" ]] || return 0
  _mu_zsh_set_scope_key_for_dir "$PWD"
  _MU_ZSH_TRACKED_SCOPE=$REPLY
}

_mu_zsh_clear_session_state() {
  MU_ZSH_SESSION_ID=
}

_mu_zsh_clear_model_state() {
  _MU_ZSH_MODEL=
}

_mu_zsh_clear_trap_state() {
  _MU_ZSH_TRAP=
}

_mu_zsh_clear_tracked_state() {
  _mu_zsh_clear_session_state
  _mu_zsh_clear_model_state
  _mu_zsh_clear_trap_state
  _MU_ZSH_TRACKED_SCOPE=
}

_mu_zsh_resolve_speculative_model_colon() {
  local action=${1:-commit}
  local current=0
  local restored_cursor

  if [[ -n "$_MU_ZSH_SPECULATIVE_MODEL_BUFFER" &&
    "$BUFFER" == "$_MU_ZSH_SPECULATIVE_MODEL_BUFFER" &&
    $CURSOR -eq ${#_MU_ZSH_SPECULATIVE_MODEL_BUFFER} ]]; then
    current=1
    if [[ "$action" == discard ]]; then
      restored_cursor=$(( CURSOR - 1 ))
      BUFFER="${BUFFER[1,CURSOR-1]}${BUFFER[CURSOR+1,-1]}"
      CURSOR=$restored_cursor
    fi
  fi

  _MU_ZSH_SPECULATIVE_MODEL_BUFFER=
  REPLY=$current
}

_mu_zsh_append_speculative_model_colon() {
  BUFFER="${BUFFER[1,CURSOR]}:${BUFFER[CURSOR+1,-1]}"
  (( CURSOR += 1 ))
  _MU_ZSH_SPECULATIVE_MODEL_BUFFER=$BUFFER
}

_mu_zsh_activate_scope() {
  local scope=$1

  if [[ -n "$_MU_ZSH_TRACKED_SCOPE" && "$_MU_ZSH_TRACKED_SCOPE" != "$scope" ]]; then
    _mu_zsh_clear_tracked_state
  fi
  _MU_ZSH_TRACKED_SCOPE=$scope
}

_mu_zsh_append_history() {
  local input=$1
  local replay=${2:-}
  local entry="true mu-history ${(qqq)input}"
  [[ -n "$replay" ]] && entry+="; $replay"
  print -sr -- "$entry"
}

_mu_zsh_decode_history() {
  local entry=$1
  local -a words
  words=("${(z)entry}")
  (( ${#words[@]} >= 3 )) || return 1
  [[ "${words[1]}" == true && "${words[2]}" == mu-history ]] || return 1
  REPLY=${(Q)words[3]}
}

_mu_zsh_reset_history_navigation() {
  _MU_ZSH_HISTORY_EVENT=0
  _MU_ZSH_HISTORY_LATEST_EVENT=0
  _MU_ZSH_HISTORY_DRAFT=
  _MU_ZSH_HISTORY_DRAFT_CURSOR=0
}

_mu_zsh_record_history() {
  local input=$1
  shift
  local replay="${(j: :)${(q)@}} <<< ${(qqq)input}"
  _mu_zsh_append_history "$input" "$replay"
}

_mu_zsh_print_block_message() {
  print -r -- "$1"
  print
}

_mu_zsh_create_session_for_scope() {
  local scope=$1
  local id
  local -a command
  command=(mu)
  id=$("${command[@]}" new) || return $?
  id=${id//$'\n'/}
  [[ "$id" =~ '^ses_[0-9a-hjkmnpqrstvwxyz]{8}$' ]] || {
    print -u2 -- "mu: new returned an invalid session id"
    return 1
  }
  _mu_zsh_activate_scope "$scope"
  MU_ZSH_SESSION_ID=$id
}

_mu_zsh_base_command() {
  local target=$1
  local scope=${2:-}
  local -a built
  [[ -n "$scope" ]] || {
    _mu_zsh_set_scope_key_for_dir "$PWD"
    scope=$REPLY
  }

  built=(mu)
  if _mu_zsh_bundle_active "$scope"; then
    [[ -n "$MU_ZSH_SESSION_ID" ]] && built+=(-s "$MU_ZSH_SESSION_ID")
    [[ -n "$_MU_ZSH_MODEL" ]] && built+=(--model "$_MU_ZSH_MODEL")
    [[ -n "$_MU_ZSH_TRAP" ]] && built+=(--trap "$_MU_ZSH_TRAP")
  fi
  set -A "$target" "${built[@]}"
  return 0
}

_mu_zsh_status_json() {
  local -a command
  command=(mu status --json "$@")
  if _mu_zsh_bundle_active; then
    [[ -n "$MU_ZSH_SESSION_ID" ]] && command+=(-s "$MU_ZSH_SESSION_ID")
    [[ -n "$_MU_ZSH_MODEL" ]] && command+=(--model "$_MU_ZSH_MODEL")
  fi
  "${command[@]}" 2>/dev/null
}

_mu_zsh_build_mode_prompt() {
  local status_json model context_raw context context_source context_segment to_compact compaction_segment cwd project_root project_segment trap_segment
  local clean unclean_segment bundle_active=0
  local escaped_model escaped_context escaped_project_root escaped_unclean_text

  # One jq pass extracts every prompt field as TSV; forking jq per field
  # dominates prompt-draw latency, so keep this to a single invocation.
  local tsv
  local -a fields
  if _mu_zsh_bundle_active; then
    bundle_active=1
  fi
  status_json=$(_mu_zsh_status_json) || status_json=
  if [[ -n "$status_json" ]] && command -v jq >/dev/null 2>&1; then
    tsv=$(jq -r '[(.model.canonical // ""), (if ((.context_tokens | type) == "number" and (.context_window | type) == "number" and .context_window > 0) then (.context_tokens * 100 / .context_window) else "" end), (.project_root // ""), (if has("clean") then (.clean|tostring) else "" end), (.context_usage_source // ""), (if ((.context_tokens | type) == "number" and (.compaction_soft_threshold_tokens | type) == "number") then (.context_tokens > .compaction_soft_threshold_tokens) else false end)] | @tsv' <<< "$status_json" 2>/dev/null) || tsv=
  fi
  fields=("${(@ps:\t:)tsv}")
  model=${fields[1]:-}
  [[ -n "$model" ]] || model=mu
  context_raw=${fields[2]:-}
  project_root=${fields[3]:-}
  clean=${fields[4]:-}
  context_source=${fields[5]:-}
  to_compact=${fields[6]:-false}
  if (( bundle_active )) && [[ -n "$MU_ZSH_SESSION_ID" ]]; then
    if [[ -z "$context_raw" || "$context_raw" == null ]]; then
      context=0%
    elif ! printf -v context '%.0f%%' "$context_raw" 2>/dev/null; then
      context=0%
    fi
    [[ "$context_source" == estimated ]] && context="~$context"
    escaped_context=$context
    escaped_context=${escaped_context//\%/%%}
    context_segment=" %F{5}${escaped_context}%f"
  else
    context_segment=
  fi
  if (( bundle_active )) && [[ -n "$MU_ZSH_SESSION_ID" && "$to_compact" == true ]]; then
    compaction_segment=" %F{5}[to compact]%f"
  else
    compaction_segment=
  fi
  cwd=$PWD
  cwd=${cwd//\%/%%}
  escaped_model=$model
  escaped_model=${escaped_model//\%/%%}
  if [[ -z "$project_root" ]]; then
    project_segment=" %F{8}(global)%f"
  elif [[ "$project_root" != "$PWD" ]]; then
    escaped_project_root=$project_root
    escaped_project_root=${escaped_project_root//\%/%%}
    project_segment=" %F{8}(${escaped_project_root})%f"
  else
    project_segment=
  fi

  if (( bundle_active )) && [[ -n "$_MU_ZSH_TRAP" ]]; then
    trap_segment=" %F{3}[trap:${_MU_ZSH_TRAP}]%f"
  else
    trap_segment=
  fi

  # When the tracked session's last turn was interrupted (unclean), surface it
  # so the user knows they can /retry to resume or just type to redirect.
  if [[ "$clean" == false ]]; then
    escaped_unclean_text='interrupted · /retry'
    unclean_segment=" %F{9}[${escaped_unclean_text}]%f"
  else
    unclean_segment=
  fi

  print -r -- "%F{12}${escaped_model}%f${context_segment}${compaction_segment} %F{6}${cwd}%f${project_segment}${unclean_segment}${trap_segment}
mu> "
}

_mu_zsh_refresh_prompt() {
  local mode_prompt

  mode_prompt=$(_mu_zsh_build_mode_prompt) || mode_prompt='mu> '
  [[ "$_MU_ZSH_MODE" == mu ]] && PROMPT=$mode_prompt
}

_mu_zsh_disable_editor_plugins() {
  if (( $+ZSH_HIGHLIGHT_HIGHLIGHTERS )); then
    _MU_ZSH_HAD_HIGHLIGHTERS=1
    _MU_ZSH_SAVED_HIGHLIGHTERS=("${ZSH_HIGHLIGHT_HIGHLIGHTERS[@]}")
    ZSH_HIGHLIGHT_HIGHLIGHTERS=()
  else
    _MU_ZSH_HAD_HIGHLIGHTERS=0
    _MU_ZSH_SAVED_HIGHLIGHTERS=()
  fi

  _MU_ZSH_DISABLED_AUTOSUGGESTIONS=0
  if (( ! ${+_ZSH_AUTOSUGGEST_DISABLED} )) && zle -l autosuggest-disable >/dev/null 2>&1; then
    if zle autosuggest-disable; then
      _MU_ZSH_DISABLED_AUTOSUGGESTIONS=1
    fi
  fi
}

_mu_zsh_restore_editor_plugins() {
  if (( _MU_ZSH_HAD_HIGHLIGHTERS )); then
    ZSH_HIGHLIGHT_HIGHLIGHTERS=("${_MU_ZSH_SAVED_HIGHLIGHTERS[@]}")
  else
    unset ZSH_HIGHLIGHT_HIGHLIGHTERS
  fi

  if (( _MU_ZSH_DISABLED_AUTOSUGGESTIONS )) && zle -l autosuggest-enable >/dev/null 2>&1; then
    zle autosuggest-enable
  fi
  _MU_ZSH_DISABLED_AUTOSUGGESTIONS=0
}

_mu_zsh_reset_mode_prompt() {
  local skip_refresh=${1:-0}
  [[ "$_MU_ZSH_MODE" == mu && "$skip_refresh" != 1 ]] && _mu_zsh_refresh_prompt
  zle reset-prompt
  zle -R
  zle -K mumode 2>/dev/null || true
}

_mu_zsh_slash_command_candidates() {
  local -a commands

  commands=(/load /model /trap)
  _mu_zsh_bundle_active && [[ -n "$MU_ZSH_SESSION_ID" ]] && commands+=(/new /retry /compact)
  commands+=("${(@f)$(_mu_zsh_custom_slash_commands 2>/dev/null || true)}")

  local command
  for command in "${commands[@]}"; do
    [[ -n "$command" ]] && print -r -- "$command"
  done
  return 0
}

_mu_zsh_custom_slash_commands() {
  local json
  json=$(_mu_zsh_status_json --include-commands) || return 1
  command -v jq >/dev/null 2>&1 || return 1
  jq -r '.commands[]?.name | "/" + .' <<< "$json"
}

_mu_zsh_has_custom_slash_command() {
  local slash_command=$1
  local command
  for command in "${(@f)$(_mu_zsh_custom_slash_commands 2>/dev/null || true)}"; do
    [[ "$command" == "$slash_command" ]] && return 0
  done
  return 1
}

_mu_zsh_models_json() {
  local -a command
  command=(mu status --json --include-models)
  _mu_zsh_bundle_active && [[ -n "$MU_ZSH_SESSION_ID" ]] && command+=(-s "$MU_ZSH_SESSION_ID")
  "${command[@]}" 2>/dev/null
}

_mu_zsh_model_query() {
  local fragment=$1 mode=${2:-candidates}
  local json
  if (( ${+_mu_zsh_completion_models} )); then
    json=$_mu_zsh_completion_models
  else
    json=$(_mu_zsh_models_json) || return 1
  fi
  command -v jq >/dev/null 2>&1 || return 1
  jq -r --arg fragment "$fragment" --arg mode "$mode" '
    def dedup:
      reduce .[] as $item ([]; if index($item) then . else . + [$item] end);
    def effort_rank:
      . as $effort
      | if $effort == "minimal" or $effort == "minimum" then 0
        else ((["low", "medium", "high", "xhigh", "max"] | index($effort)) // 5) + 1
        end;
    [.available_models.providers[]?.models[]? | {
      canonical: (.id // ""),
      short: (.model_id // ""),
      efforts: (.supported_efforts // [])
    }] as $models
    | if $mode == "candidates" then
        [
          ("short", "canonical") as $key
          | $models[]
          | if ($fragment | contains(":")) then
              .[$key] as $model | .efforts[]? | "\($model):\(.)"
            else .[$key]
            end
        ] | dedup | .[]
      else
        (if $mode == "transition" then
          ($fragment | if contains("/") then "canonical" else "short" end) as $key
          | select(all($models[];
              .[$key] == $fragment or (.[$key] | startswith($fragment) | not)))
          | [$models[] | select(.[$key] == $fragment)]
          | if $key == "canonical" then .[:1] else . end
        else
          ($fragment | rtrimstr(":")) as $base
          | [$models[] | select(.canonical == $base or .short == $base)]
        end)
        | [.[].efforts[]?] | dedup
        | to_entries
        | sort_by((.value | effort_rank), .key)
        | .[].value | ":" + .
      end
  ' <<< "$json"
}

_mu_zsh_model_completion_transition() {
  local fragment=$1 target=$2
  local -a result
  result=("${(@f)$(_mu_zsh_model_query "$fragment" transition)}")
  result=("${(@)result:#}")
  (( ${#result[@]} )) || return 1
  set -A "$target" "${result[@]}"
}

_mu_zsh_model_command_transition_allowed() {
  local fragment=$1
  local candidate
  local -a matches

  [[ "$fragment" == /model ]] && return 0
  matches=()
  for candidate in "${(@f)$(_mu_zsh_slash_command_candidates 2>/dev/null || true)}"; do
    [[ "${candidate:l}" == "${fragment:l}"* ]] && matches+=("$candidate")
  done
  (( ${#matches[@]} == 1 )) && [[ "${matches[1]}" == /model ]]
}

_mu_zsh_slash_completion_context() {
  local left

  [[ "$BUFFER" == /* ]] || return 1
  left=${BUFFER[1,$CURSOR]}

  if [[ "$left" == "/model "* ]]; then
    left=${left#"/model "}
    [[ "$left" != *[[:space:]]* ]]
    return
  fi

  if [[ "$left" == "/trap "* ]]; then
    left=${left#"/trap "}
    [[ "$left" != *[[:space:]]* ]]
    return
  fi

  [[ "$left" != *[[:space:]]* ]]
}

_mu_zsh_completion_candidates() {
  local left arg

  left=${BUFFER[1,$CURSOR]}

  if [[ "$left" == "/model "* ]]; then
    arg=${left#"/model "}
    [[ "$arg" != *[[:space:]]* ]] || return 1
    _mu_zsh_model_query "$arg"
    return
  fi

  if [[ "$left" == "/trap "* ]]; then
    arg=${left#"/trap "}
    [[ "$arg" != *[[:space:]]* ]] || return 1
    print -l -- off destructive reversible all default
    return
  fi

  [[ "$left" == /* ]] || return 1
  [[ "$left" != *[[:space:]]* ]] || return 1

  _mu_zsh_slash_command_candidates
}

_mu_zsh_fallback_completion() {
  _mu_zsh_complete_candidates fallback
}

_mu_zsh_completion_system() {
  _mu_zsh_complete_candidates compsys
}

_mu_zsh_complete_candidates() {
  local adapter=$1
  local left=${BUFFER[1,$CURSOR]} arg effort_suffix expl
  local group=mu-slash-command description='mu slash command' suffix=' '
  local -a candidates effort_suffixes ordering

  if [[ "$left" == "/model "* ]]; then
    # Direct native/list-choices entry points also get one local snapshot.
    if (( ! ${+_mu_zsh_completion_models} )); then
      local _mu_zsh_completion_models
      _mu_zsh_completion_models=$(_mu_zsh_models_json) || return 1
    fi
    group=mu-model description=model suffix=''
    arg=${left#"/model "}
    if [[ "$arg" == *:* ]]; then
      effort_suffixes=("${(@f)$(_mu_zsh_model_query "${arg%:*}:" suffix)}")
      effort_suffixes=("${(@)effort_suffixes:#}")
      if (( ${#effort_suffixes[@]} )); then
        compset -P '*:' 2>/dev/null || true
        for effort_suffix in "${effort_suffixes[@]}"; do
          candidates+=("${effort_suffix#:}")
        done
        group=mu-model-effort description='model effort'
      fi
    fi
  fi

  if (( ! ${#candidates[@]} )); then
    candidates=("${(@f)$(_mu_zsh_completion_candidates)}")
    candidates=("${(@)candidates:#}")
  fi
  (( ${#candidates[@]} )) || return 1
  [[ "$left" == "/trap "* ]] && suffix=''
  [[ "$group" != mu-slash-command ]] && ordering=(-V)

  if [[ "$adapter" == compsys ]]; then
    _wanted "${ordering[@]}" "$group" expl "$description" \
      compadd -Q -S "$suffix" -- "${candidates[@]}"
  else
    (( ${#ordering[@]} )) && ordering+=("$group")
    compadd "${ordering[@]}" -Q -S "$suffix" -- "${candidates[@]}"
  fi
}

_mu_zsh_use_completion_system() {
  # compinit may be loaded after this plugin, so register lazily.
  (( $+functions[_main_complete] && $+functions[compdef] )) || return 1
  [[ ${_comps[mu-zsh-slash]-} == _mu_zsh_completion_system ]] ||
    compdef _mu_zsh_completion_system mu-zsh-slash
}

_mu_zsh_complete_slash() {
  local before_buffer=$BUFFER before_cursor=$CURSOR
  local before_left=${BUFFER[1,$CURSOR]}
  local completion_mode=complete
  local model_arg effort
  local -a display_efforts effort_suffixes

  _mu_zsh_slash_completion_context || return 1
  if [[ "$before_left" == "/model "* ]]; then
    # Dynamically scoped through ZLE callbacks, and discarded after this Tab.
    local _mu_zsh_completion_models
    _mu_zsh_completion_models=$(_mu_zsh_models_json) || return 1
    model_arg=${before_left#"/model "}
    if [[ "$model_arg" == *: ]]; then
      effort_suffixes=("${(@f)$(_mu_zsh_model_query "$model_arg" suffix)}")
      effort_suffixes=("${(@)effort_suffixes:#}")
      # An empty effort token needs menu-complete; expand-or-complete would
      # spend this Tab rebuilding the already-visible candidate list.
      (( ${#effort_suffixes[@]} )) && completion_mode=menu
    fi
  fi

  if _mu_zsh_use_completion_system; then
    local compcontext=mu-zsh-slash
    if [[ "$completion_mode" == menu ]]; then
      zle menu-complete
    else
      zle expand-or-complete
    fi
  elif [[ "$completion_mode" == menu ]]; then
    zle _mu_zsh_menu_widget
  else
    zle _mu_zsh_complete_widget
  fi

  if [[ "$before_left" != "/model "* &&
    ( "$BUFFER" == "/model" || "$BUFFER" == "/model " ) &&
    $CURSOR -eq ${#BUFFER} ]] &&
    _mu_zsh_model_command_transition_allowed "$before_left"; then
    [[ "$BUFFER" == "/model" ]] && {
      BUFFER+=$' '
      (( CURSOR += 1 ))
    }
    return
  fi

  if [[ "${before_buffer[1,$before_cursor]}" == "/model "* &&
    $before_cursor -eq ${#before_buffer} &&
    "$BUFFER" == "/model "* &&
    "${BUFFER#"/model "}" != *[[:space:]]* ]]; then
    CURSOR=${#BUFFER}
  fi

  if [[ "${before_buffer[1,$before_cursor]}" == "/model "* ]] &&
    [[ "${BUFFER[1,$CURSOR]}" == "/model "* &&
      $CURSOR -eq ${#BUFFER} ]]; then
    model_arg=${BUFFER#"/model "}
    if _mu_zsh_model_completion_transition "$model_arg" effort_suffixes; then
      _mu_zsh_append_speculative_model_colon
      for effort in "${effort_suffixes[@]}"; do
        display_efforts+=("${effort#:}")
      done
      zle -M "${(j:  :)display_efforts}"
    fi
  fi
}

_mu_zsh_list_slash_choices() {
  _mu_zsh_slash_completion_context || return 1
  if _mu_zsh_use_completion_system; then
    local compcontext=mu-zsh-slash
    zle list-choices 2>/dev/null || true
    return
  fi
  zle _mu_zsh_list_widget 2>/dev/null || true
}

_mu_zsh_require_active_session() {
  local command=$1
  if ! _mu_zsh_bundle_active || [[ -z "$MU_ZSH_SESSION_ID" ]]; then
    _mu_zsh_print_block_message "[mu] $command requires an active session in this scope"
    return 1
  fi
  return 0
}

_mu_zsh_validate_no_args() {
  local command=$1
  local rest=$2
  if [[ -n "$rest" ]]; then
    _mu_zsh_print_block_message "[mu] $command does not accept arguments"
    return 1
  fi
  return 0
}

_mu_zsh_validate_model_ref() {
  local model=$1
  local -a command
  local status_json resolved
  command=(mu status --json --model "$model")
  _mu_zsh_bundle_active && [[ -n "$MU_ZSH_SESSION_ID" ]] && command+=(-s "$MU_ZSH_SESSION_ID")
  status_json=$("${command[@]}" 2>/dev/null) || return 1
  resolved=$(jq -r '.model.canonical // empty' <<< "$status_json" 2>/dev/null) || resolved=
  REPLY=${resolved:-$model}
  return 0
}

_mu_zsh_resolve_load() {
  local requested_session=$1
  local target=$2
  local status_json session_id output
  local -a command fields

  command=(mu status --json)
  if [[ -n "$requested_session" ]]; then
    command+=(-s "$requested_session")
  else
    command+=(--continue)
  fi
  status_json=$("${command[@]}") || return $?
  fields=("${(@ps:\t:)$(jq -r '[(.session_id // ""), (.output // "")] | @tsv' <<< "$status_json" 2>/dev/null)}") || {
    print -u2 -- "mu mu.zsh: could not resolve current session from status"
    return 1
  }
  session_id=${fields[1]:-}
  output=${fields[2]:-}
  if [[ -z "$session_id" ]]; then
    _mu_zsh_print_block_message "[mu] no sessions found in active scope"
    return 1
  fi
  if [[ ! "$session_id" =~ '^ses_[0-9a-hjkmnpqrstvwxyz]{8}$' ]]; then
    print -u2 -- "mu mu.zsh: status returned an invalid session id"
    return 1
  fi
  case "$output" in
    final|concise|detail|full) ;;
    *)
      print -u2 -- "mu mu.zsh: status returned an invalid output density"
      return 1
      ;;
  esac
  set -A "$target" "$session_id" "$output"
}

_mu_zsh_run_slash_command() {
  local line=$1
  local command instruction rest session_id scope resolved_model
  local exit_status=0

  command=${line%%[[:space:]]*}
  if [[ "$command" == "$line" ]]; then
    instruction=
  else
    instruction=${line#"$command"}
    instruction=${instruction#?}
  fi
  rest=$instruction
  if [[ -n "$rest" ]]; then
    while [[ "$rest" == [[:space:]]* ]]; do
      rest=${rest#[[:space:]]}
    done
    while [[ "$rest" == *[[:space:]] ]]; do
      rest=${rest%[[:space:]]}
    done
  fi

  _mu_zsh_append_history "$line"
  _mu_zsh_set_scope_key_for_dir "$PWD"
  scope=$REPLY
  case "$command" in
    /model)
      if [[ -z "$rest" ]]; then
        _mu_zsh_print_block_message "[mu] usage: /model <model>"
        return 1
      fi
      if [[ "$rest" == *[[:space:]]* ]]; then
        _mu_zsh_print_block_message "[mu] /model accepts exactly one model reference"
        return 1
      fi
      if ! _mu_zsh_validate_model_ref "$rest"; then
        _mu_zsh_print_block_message "[mu] unknown or unsupported model: $rest"
        return 1
      fi
      resolved_model=$REPLY
      _mu_zsh_activate_scope "$scope"
      _MU_ZSH_MODEL=$resolved_model
      _mu_zsh_print_block_message "[mu] next turns in this scope will use $resolved_model"
      ;;
    /trap)
      if [[ -z "$rest" ]]; then
        _mu_zsh_print_block_message "[mu] usage: /trap <off|destructive|reversible|all|default>"
        return 1
      fi
      if [[ "$rest" == *[[:space:]]* ||
        "$rest" != off && "$rest" != destructive && "$rest" != reversible &&
        "$rest" != all && "$rest" != default ]]; then
        _mu_zsh_print_block_message "[mu] invalid trap level: $rest"
        return 1
      fi
      _mu_zsh_activate_scope "$scope"
      if [[ "$rest" == default ]]; then
        _MU_ZSH_TRAP=
        _mu_zsh_print_block_message "[mu] next turns in this scope will use the configured trap level"
      else
        _MU_ZSH_TRAP=$rest
        _mu_zsh_print_block_message "[mu] next turns in this scope will use trap $rest"
      fi
      ;;
    /load)
      if [[ "$rest" == *[[:space:]]* ]]; then
        _mu_zsh_print_block_message "[mu] /load accepts exactly one session id"
        return 1
      fi
      local -a load
      _mu_zsh_resolve_load "$rest" load || return $?
      session_id=$load[1]
      mu transcript --session "$session_id" --output "$load[2]" || return $?
      _mu_zsh_activate_scope "$scope"
      MU_ZSH_SESSION_ID=$session_id
      _mu_zsh_print_block_message "[mu] loaded session $session_id"
      ;;
    /new)
      _mu_zsh_validate_no_args "$command" "$rest" || return 1
      _mu_zsh_activate_scope "$scope"
      _mu_zsh_require_active_session "$command" || return 1
      _mu_zsh_clear_session_state
      _mu_zsh_print_block_message "[mu] next turn will start a new session"
      ;;
    /retry)
      _mu_zsh_validate_no_args "$command" "$rest" || return 1
      _mu_zsh_activate_scope "$scope"
      _mu_zsh_require_active_session "$command" || return 1
      session_id=$MU_ZSH_SESSION_ID
      local -a retry_command
      retry_command=(mu retry -s "$session_id")
      [[ -n "$_MU_ZSH_MODEL" ]] && retry_command+=(--model "$_MU_ZSH_MODEL")
      [[ -n "$_MU_ZSH_TRAP" ]] && retry_command+=(--trap "$_MU_ZSH_TRAP")
      if "${retry_command[@]}"; then
        exit_status=0
      else
        exit_status=$?
      fi
      ;;
    /compact)
      _mu_zsh_activate_scope "$scope"
      _mu_zsh_require_active_session "$command" || return 1
      session_id=$MU_ZSH_SESSION_ID
      local -a compact_command
      compact_command=(mu compact --session "$session_id")
      [[ -n "$_MU_ZSH_TRAP" ]] && compact_command+=(--trap "$_MU_ZSH_TRAP")
      if [[ -n "$instruction" ]]; then
        print -rn -- "$instruction" | "${compact_command[@]}"
        exit_status=${pipestatus[2]}
      else
        if "${compact_command[@]}"; then
          exit_status=0
        else
          exit_status=$?
        fi
      fi
      print
      ;;
    *)
      if _mu_zsh_has_custom_slash_command "$command"; then
        _mu_zsh_submit_prompt "$instruction" "${command#/}"
        exit_status=$?
      else
        _mu_zsh_print_block_message "[mu] unknown slash command: $command"
        return 1
      fi
      ;;
  esac

  return $exit_status
}

_mu_zsh_enter_mode() {
  [[ "$_MU_ZSH_MODE" == mu ]] && return 0
  command -v jq >/dev/null 2>&1 || {
    print -u2 -- "mu: mu.zsh requires jq; install it with your package manager (apt, brew, dnf, etc.)"
    return 1
  }

  _MU_ZSH_MODE=mu
  _MU_ZSH_SAVED_KEYMAP=${KEYMAP:-main}
  _MU_ZSH_ORIGINAL_PROMPT=$PROMPT
  _MU_ZSH_ORIGINAL_RPROMPT=$RPROMPT
  _mu_zsh_refresh_prompt
  RPROMPT=
  _mu_zsh_disable_editor_plugins
  zle -K mumode 2>/dev/null || true
}

_mu_zsh_exit_mode() {
  [[ "$_MU_ZSH_MODE" == shell ]] && return 0

  _mu_zsh_resolve_speculative_model_colon
  _mu_zsh_reset_history_navigation
  _MU_ZSH_MODE=shell
  zle -K "${_MU_ZSH_SAVED_KEYMAP:-main}" 2>/dev/null || zle -K main 2>/dev/null || true
  PROMPT=$_MU_ZSH_ORIGINAL_PROMPT
  RPROMPT=$_MU_ZSH_ORIGINAL_RPROMPT
  _mu_zsh_restore_editor_plugins
}

_mu_zsh_insert_newline() {
  _mu_zsh_resolve_speculative_model_colon
  [[ "$_MU_ZSH_MODE" == mu ]] || {
    zle self-insert
    return
  }

  BUFFER="${BUFFER[1,CURSOR]}"$'\n'"${BUFFER[CURSOR+1,-1]}"
  (( CURSOR += 1 ))
}

_mu_zsh_history_up() {
  _mu_zsh_resolve_speculative_model_colon
  if [[ "${BUFFER[1,CURSOR]}" == *$'\n'* ]]; then
    zle up-line
    return
  fi

  local origin_event=$_MU_ZSH_HISTORY_EVENT
  local origin_histno=$HISTNO
  local origin_buffer=$BUFFER
  local origin_cursor=$CURSOR
  local candidate entry

  if (( ! _MU_ZSH_HISTORY_LATEST_EVENT )); then
    _MU_ZSH_HISTORY_LATEST_EVENT=$HISTNO
    _MU_ZSH_HISTORY_EVENT=$HISTNO
    _MU_ZSH_HISTORY_DRAFT=$BUFFER
    _MU_ZSH_HISTORY_DRAFT_CURSOR=$CURSOR
  elif (( _MU_ZSH_HISTORY_EVENT == _MU_ZSH_HISTORY_LATEST_EVENT )); then
    _MU_ZSH_HISTORY_DRAFT=$BUFFER
    _MU_ZSH_HISTORY_DRAFT_CURSOR=$CURSOR
  fi

  candidate=$(( _MU_ZSH_HISTORY_EVENT - 1 ))
  while (( candidate > 0 )); do
    HISTNO=$candidate
    entry=$BUFFER
    if _mu_zsh_decode_history "$entry"; then
      _MU_ZSH_HISTORY_EVENT=$candidate
      BUFFER=$REPLY
      CURSOR=${#BUFFER}
      return
    fi
    (( candidate-- ))
  done

  HISTNO=$origin_histno
  BUFFER=$origin_buffer
  CURSOR=$origin_cursor
  if (( ! origin_event )); then
    _mu_zsh_reset_history_navigation
  fi
}

_mu_zsh_history_down() {
  _mu_zsh_resolve_speculative_model_colon
  if [[ "${BUFFER[CURSOR+1,-1]}" == *$'\n'* ]]; then
    zle down-line
    return
  fi
  (( _MU_ZSH_HISTORY_LATEST_EVENT )) || return 0
  (( _MU_ZSH_HISTORY_EVENT < _MU_ZSH_HISTORY_LATEST_EVENT )) || return 0

  local candidate entry
  candidate=$(( _MU_ZSH_HISTORY_EVENT + 1 ))
  while (( candidate < _MU_ZSH_HISTORY_LATEST_EVENT )); do
    HISTNO=$candidate
    entry=$BUFFER
    if _mu_zsh_decode_history "$entry"; then
      _MU_ZSH_HISTORY_EVENT=$candidate
      BUFFER=$REPLY
      CURSOR=${#BUFFER}
      return
    fi
    (( candidate++ ))
  done

  _MU_ZSH_HISTORY_EVENT=$_MU_ZSH_HISTORY_LATEST_EVENT
  BUFFER=$_MU_ZSH_HISTORY_DRAFT
  CURSOR=$_MU_ZSH_HISTORY_DRAFT_CURSOR
}

_mu_zsh_submit_prompt() {
  local input=$1
  local target=${2:-}
  local scope
  local -a command

  _mu_zsh_set_scope_key_for_dir "$PWD"
  scope=$REPLY
  _mu_zsh_activate_scope "$scope"
  [[ -n "$MU_ZSH_SESSION_ID" ]] ||
    _mu_zsh_create_session_for_scope "$scope" || return $?
  _mu_zsh_base_command command "$scope"
  if [[ -n "$target" ]]; then
    command+=("$target")
  else
    # Record the actual command after model-free session creation.
    _mu_zsh_record_history "$input" "${command[@]}"
  fi

  if [[ -z "$target" ]]; then
    "${command[@]}" <<< "$input"
  elif [[ -n "$input" ]]; then
    print -rn -- "$input" | "${command[@]}"
    return ${pipestatus[2]}
  else
    "${command[@]}"
  fi
}

_mu_zsh_tab() {
  if [[ "$_MU_ZSH_MODE" == mu ]]; then
    _mu_zsh_resolve_speculative_model_colon
    if _mu_zsh_slash_completion_context; then
      _mu_zsh_complete_slash
      return
    fi

    if (( CURSOR == 0 )); then
      _mu_zsh_exit_mode
      zle reset-prompt
      zle -K "${_MU_ZSH_SAVED_KEYMAP:-main}" 2>/dev/null || zle -K main 2>/dev/null || true
      return
    fi

    zle self-insert
    return
  fi

  if (( CURSOR == 0 )); then
    if _mu_zsh_enter_mode; then
      _mu_zsh_reset_mode_prompt 1
    else
      zle reset-prompt
      zle -R
    fi
    return
  fi

  [[ -n "$_MU_ZSH_ORIGINAL_TAB_WIDGET" ]] && zle "$_MU_ZSH_ORIGINAL_TAB_WIDGET"
}

_mu_zsh_slash() {
  local should_complete=0

  _mu_zsh_resolve_speculative_model_colon

  if [[ "$_MU_ZSH_MODE" == mu && "$BUFFER" != /* && "$CURSOR" -eq 0 ]]; then
    should_complete=1
  fi

  if [[ -n "$_MU_ZSH_ORIGINAL_SLASH_WIDGET" && "$_MU_ZSH_ORIGINAL_SLASH_WIDGET" != _mu_zsh_slash ]]; then
    zle "$_MU_ZSH_ORIGINAL_SLASH_WIDGET"
  else
    zle .self-insert
  fi

  (( should_complete )) && _mu_zsh_list_slash_choices
}

_mu_zsh_accept() {
  if [[ "$_MU_ZSH_MODE" != mu ]]; then
    zle .accept-line
    return
  fi

  _mu_zsh_resolve_speculative_model_colon discard
  local input=$BUFFER
  _mu_zsh_reset_history_navigation
  if [[ -z "${input//[[:space:]]/}" ]]; then
    zle .accept-line
    return
  fi

  _MU_ZSH_PENDING_INPUT=$input
  _MU_ZSH_PENDING_PROMPT=$PROMPT
  # Accept the visible draft normally. The line-finish hook freezes that
  # display and clears the command before zsh can parse it.
  zle .accept-line
}

_mu_zsh_finish_pending() {
  [[ -n "$_MU_ZSH_PENDING_INPUT" ]] || return 0

  _mu_zsh_resolve_speculative_model_colon
  zle -I
  BUFFER=
  CURSOR=0
}

_mu_zsh_dispatch_pending() {
  [[ -n "$_MU_ZSH_PENDING_INPUT" ]] || return 0

  local input=$_MU_ZSH_PENDING_INPUT
  PROMPT=$_MU_ZSH_PENDING_PROMPT
  _MU_ZSH_PENDING_INPUT=
  _MU_ZSH_PENDING_PROMPT=

  if [[ "$input" == /* ]]; then
    _mu_zsh_run_slash_command "$input"
  else
    _mu_zsh_submit_prompt "$input"
  fi
  [[ "$_MU_ZSH_MODE" == mu ]] && _mu_zsh_refresh_prompt
}

_mu_zsh_line_init() {
  _mu_zsh_resolve_speculative_model_colon
  _mu_zsh_reset_history_navigation
  [[ "$_MU_ZSH_MODE" == mu ]] && _mu_zsh_refresh_prompt
  if [[ "$_MU_ZSH_MODE" == mu ]]; then
    zle -K mumode 2>/dev/null || true
  fi
}

_mu_zsh_self_insert() {
  _mu_zsh_resolve_speculative_model_colon
  zle .self-insert
}

_mu_zsh_model_colon() {
  _mu_zsh_resolve_speculative_model_colon
  (( REPLY )) && return 0
  zle .self-insert
}

_mu_zsh_speculative_backspace() {
  _mu_zsh_resolve_speculative_model_colon discard
  (( REPLY )) && return 0
  zle .backward-delete-char
}

_mu_zsh_speculative_delete() {
  _mu_zsh_resolve_speculative_model_colon
  zle .delete-char
}

_mu_zsh_speculative_cursor() {
  _mu_zsh_resolve_speculative_model_colon
  zle "$1"
}

_mu_zsh_speculative_beginning() {
  _mu_zsh_speculative_cursor .beginning-of-line
}

_mu_zsh_speculative_end() {
  _mu_zsh_speculative_cursor .end-of-line
}

_mu_zsh_speculative_left() {
  _mu_zsh_speculative_cursor .backward-char
}

_mu_zsh_speculative_right() {
  _mu_zsh_speculative_cursor .forward-char
}

mu-zsh-mode() {
  if _mu_zsh_enter_mode; then
    _mu_zsh_reset_mode_prompt 1
  else
    zle reset-prompt
    zle -R
  fi
}

mu-zsh-exit-mode() {
  _mu_zsh_exit_mode
  zle reset-prompt
  zle -K "${_MU_ZSH_SAVED_KEYMAP:-main}" 2>/dev/null || zle -K main 2>/dev/null || true
}

_mu_zsh_configure_keymap() {
  bindkey -R -M mumode ' -~' _mu_zsh_self_insert
  bindkey -M mumode '^M' _mu_zsh_accept
  bindkey -M mumode '^J' _mu_zsh_accept
  bindkey -M mumode $'\e[13;2u' _mu_zsh_insert_newline
  bindkey -M mumode '^I' _mu_zsh_tab
  bindkey -M mumode '/' _mu_zsh_slash
  bindkey -M mumode $'\e[A' _mu_zsh_history_up
  bindkey -M mumode $'\eOA' _mu_zsh_history_up
  bindkey -M mumode $'\e[B' _mu_zsh_history_down
  bindkey -M mumode $'\eOB' _mu_zsh_history_down
  bindkey -M mumode ':' _mu_zsh_model_colon
  bindkey -M mumode '^?' _mu_zsh_speculative_backspace
  bindkey -M mumode '^H' _mu_zsh_speculative_backspace
  bindkey -M mumode $'\e[3~' _mu_zsh_speculative_delete
  bindkey -M mumode '^A' _mu_zsh_speculative_beginning
  bindkey -M mumode '^E' _mu_zsh_speculative_end
  bindkey -M mumode $'\e[D' _mu_zsh_speculative_left
  bindkey -M mumode $'\e[C' _mu_zsh_speculative_right
  bindkey -M mumode $'\e[H' _mu_zsh_speculative_beginning
  bindkey -M mumode $'\e[F' _mu_zsh_speculative_end
  # Ctrl-C is intentionally left inherited from the main keymap: real terminals
  # deliver it as SIGINT (the tty intercepts it before ZLE), which the shell
  # already handles by cancelling the draft and redrawing a fresh mu> prompt.
}

_mu_zsh_adopt_seeded_bundle

if [[ -o zle ]]; then
  autoload -Uz add-zsh-hook 2>/dev/null || true
  autoload -Uz add-zle-hook-widget 2>/dev/null || true
  bindkey -N mumode main 2>/dev/null || true
  _mu_zsh_configure_keymap
  _mu_zsh_save_widget_bindings
  zle -C _mu_zsh_complete_widget complete-word _mu_zsh_fallback_completion
  zle -C _mu_zsh_menu_widget menu-complete _mu_zsh_fallback_completion
  zle -C _mu_zsh_list_widget list-choices _mu_zsh_fallback_completion
  zle -N _mu_zsh_tab
  zle -N _mu_zsh_slash
  zle -N _mu_zsh_accept
  zle -N _mu_zsh_insert_newline
  zle -N _mu_zsh_history_up
  zle -N _mu_zsh_history_down
  zle -N _mu_zsh_finish_pending
  zle -N _mu_zsh_line_init
  zle -N _mu_zsh_self_insert
  zle -N _mu_zsh_model_colon
  zle -N _mu_zsh_speculative_backspace
  zle -N _mu_zsh_speculative_delete
  zle -N _mu_zsh_speculative_beginning
  zle -N _mu_zsh_speculative_end
  zle -N _mu_zsh_speculative_left
  zle -N _mu_zsh_speculative_right
  zle -N mu-zsh-mode
  zle -N mu-zsh-exit-mode
  add-zle-hook-widget line-finish _mu_zsh_finish_pending 2>/dev/null || true
  add-zle-hook-widget line-init _mu_zsh_line_init 2>/dev/null || true
  add-zsh-hook precmd _mu_zsh_dispatch_pending 2>/dev/null || true
  bindkey '^I' _mu_zsh_tab
fi
