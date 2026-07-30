# zsh integration for mu.
#
# Source this file from .zshrc to add a shell-native mu prompt mode:
# press Tab at cursor position 0 to toggle "mu>" mode while preserving the
# current buffer, Enter to submit one non-blank mu turn, Ctrl+C to cancel the
# current mu prompt while leaving the typed line in scrollback, Ctrl+D to keep
# normal shell EOF behavior even from "mu>" mode, and Up/Down to stay within
# the current buffer instead of browsing shell history.

typeset -g MU_ZSH_MODE=${MU_ZSH_MODE:-shell}
typeset -g MU_ZSH_TRACKED_SCOPE=${MU_ZSH_TRACKED_SCOPE:-}
typeset -g MU_ZSH_SESSION_ID=${MU_ZSH_SESSION_ID:-}
typeset -g MU_ZSH_EFFECTIVE_SESSION_ID=${MU_ZSH_EFFECTIVE_SESSION_ID:-}
typeset -g MU_ZSH_MODEL=${MU_ZSH_MODEL:-}
typeset -g MU_ZSH_EFFECTIVE_MODEL=${MU_ZSH_EFFECTIVE_MODEL:-}
typeset -gi MU_ZSH_EFFECTIVE_ATTACHMENT_COUNT=0
typeset -g MU_ZSH_BIN=${MU_ZSH_BIN:-mu}
typeset -g MU_ZSH_OUTPUT=${MU_ZSH_OUTPUT:-}
typeset -g MU_ZSH_PROMPT_INPUT=${MU_ZSH_PROMPT_INPUT:-${MU_ZSH_PROMPT:-'mu> '}}
typeset -g MU_ZSH_PROMPT=${MU_ZSH_PROMPT:-$MU_ZSH_PROMPT_INPUT}
typeset -g MU_ZSH_PENDING_INPUT=
typeset -g MU_ZSH_PENDING_PROMPT=
typeset -gi MU_ZSH_PENDING_SUBMIT=0
typeset -ga MU_ZSH_PENDING_ATTACHMENTS
# 16-color theme slots (webterm/xterm Tango). model=cyan, ctx=magenta,
# pwd=bright-blue, project=bright-black, unclean=bright-red.
typeset -g MU_ZSH_PROMPT_MODEL_COLOR=${MU_ZSH_PROMPT_MODEL_COLOR:-6}
typeset -g MU_ZSH_PROMPT_CONTEXT_COLOR=${MU_ZSH_PROMPT_CONTEXT_COLOR:-5}
typeset -g MU_ZSH_PROMPT_PWD_COLOR=${MU_ZSH_PROMPT_PWD_COLOR:-12}
typeset -g MU_ZSH_PROMPT_PROJECT_COLOR=${MU_ZSH_PROMPT_PROJECT_COLOR:-8}
typeset -g MU_ZSH_PROMPT_UNCLEAN_COLOR=${MU_ZSH_PROMPT_UNCLEAN_COLOR:-9}
typeset -g MU_ZSH_PROMPT_UNCLEAN_TEXT=${MU_ZSH_PROMPT_UNCLEAN_TEXT:-'interrupted · /retry'}
typeset -g MU_ZSH_ORIGINAL_PROMPT=${MU_ZSH_ORIGINAL_PROMPT:-}
typeset -g MU_ZSH_ORIGINAL_RPROMPT=${MU_ZSH_ORIGINAL_RPROMPT:-}
typeset -g MU_ZSH_SAVED_KEYMAP=${MU_ZSH_SAVED_KEYMAP:-main}
typeset -g MU_ZSH_ORIGINAL_TAB_WIDGET=${MU_ZSH_ORIGINAL_TAB_WIDGET:-}
typeset -g MU_ZSH_ORIGINAL_SLASH_WIDGET=${MU_ZSH_ORIGINAL_SLASH_WIDGET:-}
typeset -gi MU_ZSH_HAD_HIGHLIGHTERS=${MU_ZSH_HAD_HIGHLIGHTERS:-0}
typeset -gi MU_ZSH_DISABLED_AUTOSUGGESTIONS=${MU_ZSH_DISABLED_AUTOSUGGESTIONS:-0}
typeset -ga MU_ZSH_COMMAND_REPLY
typeset -ga MU_ZSH_SAVED_HIGHLIGHTERS
typeset -ga MU_ZSH_ENTER_HOOKS
typeset -ga MU_ZSH_EXIT_HOOKS

_mu_zsh_save_widget_bindings() {
  local binding
  if [[ -z "$MU_ZSH_ORIGINAL_TAB_WIDGET" ]]; then
    binding=${${(z)$(bindkey '^I' 2>/dev/null)}[2]}
    [[ "$binding" != _mu_zsh_tab ]] && MU_ZSH_ORIGINAL_TAB_WIDGET=$binding
  fi
  [[ -z "$MU_ZSH_ORIGINAL_TAB_WIDGET" ]] && MU_ZSH_ORIGINAL_TAB_WIDGET=expand-or-complete

  if [[ -z "$MU_ZSH_ORIGINAL_SLASH_WIDGET" ]]; then
    binding=${${(z)$(bindkey '/' 2>/dev/null)}[2]}
    [[ "$binding" != _mu_zsh_slash ]] && MU_ZSH_ORIGINAL_SLASH_WIDGET=$binding
  fi
  [[ -z "$MU_ZSH_ORIGINAL_SLASH_WIDGET" ]] && MU_ZSH_ORIGINAL_SLASH_WIDGET=.self-insert
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

_mu_zsh_sync_state() {
  local scope=${1:-}
  [[ -n "$scope" ]] || {
    _mu_zsh_set_scope_key_for_dir "$PWD"
    scope=$REPLY
  }

  if [[ -z "$MU_ZSH_TRACKED_SCOPE" ]] &&
    [[ -n "$MU_ZSH_SESSION_ID" || -n "$MU_ZSH_MODEL" || ${#MU_ZSH_PENDING_ATTACHMENTS[@]} -gt 0 ]]; then
    MU_ZSH_TRACKED_SCOPE=$scope
  fi

  if [[ -n "$MU_ZSH_TRACKED_SCOPE" && "$MU_ZSH_TRACKED_SCOPE" == "$scope" ]]; then
    MU_ZSH_EFFECTIVE_SESSION_ID=$MU_ZSH_SESSION_ID
    MU_ZSH_EFFECTIVE_MODEL=$MU_ZSH_MODEL
    MU_ZSH_EFFECTIVE_ATTACHMENT_COUNT=${#MU_ZSH_PENDING_ATTACHMENTS[@]}
  else
    MU_ZSH_EFFECTIVE_SESSION_ID=
    MU_ZSH_EFFECTIVE_MODEL=
    MU_ZSH_EFFECTIVE_ATTACHMENT_COUNT=0
  fi
}

_mu_zsh_clear_session_state() {
  MU_ZSH_SESSION_ID=
  MU_ZSH_EFFECTIVE_SESSION_ID=
}

_mu_zsh_clear_model_state() {
  MU_ZSH_MODEL=
  MU_ZSH_EFFECTIVE_MODEL=
}

_mu_zsh_clear_tracked_state() {
  _mu_zsh_clear_session_state
  _mu_zsh_clear_model_state
  MU_ZSH_PENDING_ATTACHMENTS=()
  MU_ZSH_TRACKED_SCOPE=
  MU_ZSH_EFFECTIVE_ATTACHMENT_COUNT=0
}

_mu_zsh_activate_scope() {
  local scope=$1

  if [[ -n "$MU_ZSH_TRACKED_SCOPE" && "$MU_ZSH_TRACKED_SCOPE" != "$scope" ]]; then
    _mu_zsh_clear_tracked_state
  fi
  MU_ZSH_TRACKED_SCOPE=$scope
  _mu_zsh_sync_state "$scope"
}

_mu_zsh_record_history() {
  local input=$1
  local scope=${2:-}
  local session_id
  local model
  local quoted=${(qqq)input}
  _mu_zsh_sync_state "$scope"
  session_id=$MU_ZSH_EFFECTIVE_SESSION_ID
  model=$MU_ZSH_EFFECTIVE_MODEL

  local attachments=
  local output=
  local attachment
  for attachment in "${MU_ZSH_PENDING_ATTACHMENTS[@]}"; do
    attachments+=" -a ${(q)attachment}"
  done
  [[ -n "$MU_ZSH_OUTPUT" ]] && output=" --output ${(q)MU_ZSH_OUTPUT}"

  if [[ -n "$session_id" ]]; then
    if [[ -n "$model" ]]; then
      print -sr -- "$MU_ZSH_BIN -s ${(q)session_id} --model ${(q)model}${attachments}${output} <<< $quoted"
    else
      print -sr -- "$MU_ZSH_BIN -s ${(q)session_id}${attachments}${output} <<< $quoted"
    fi
  elif [[ -n "$model" ]]; then
    print -sr -- "$MU_ZSH_BIN --model ${(q)model}${attachments}${output} <<< $quoted"
  else
    print -sr -- "$MU_ZSH_BIN${attachments}${output} <<< $quoted"
  fi
}

_mu_zsh_print_block_message() {
  print -r -- "$1"
  print
}

_mu_zsh_create_session_for_scope() {
  local scope=$1
  local id
  local -a command
  command=("$MU_ZSH_BIN")
  id=$("${command[@]}" session new) || return $?
  id=${id//$'\n'/}
  [[ "$id" =~ '^ses_[0-9a-hjkmnpqrstvwxyz]{8}$' ]] || {
    print -u2 -- "mu: session new returned an invalid session id"
    return 1
  }
  _mu_zsh_activate_scope "$scope"
  MU_ZSH_SESSION_ID=$id
  MU_ZSH_EFFECTIVE_SESSION_ID=$id
}

_mu_zsh_base_command_reply() {
  local scope=${1:-}
  [[ -n "$scope" ]] || {
    _mu_zsh_set_scope_key_for_dir "$PWD"
    scope=$REPLY
  }

  _mu_zsh_sync_state "$scope"
  MU_ZSH_COMMAND_REPLY=("$MU_ZSH_BIN")
  [[ -n "$MU_ZSH_OUTPUT" ]] && MU_ZSH_COMMAND_REPLY+=(--output "$MU_ZSH_OUTPUT")
  [[ -n "$MU_ZSH_EFFECTIVE_SESSION_ID" ]] && MU_ZSH_COMMAND_REPLY+=(-s "$MU_ZSH_EFFECTIVE_SESSION_ID")
  [[ -n "$MU_ZSH_EFFECTIVE_MODEL" ]] && MU_ZSH_COMMAND_REPLY+=(--model "$MU_ZSH_EFFECTIVE_MODEL")
  return 0
}

_mu_zsh_status_json() {
  local -a command
  _mu_zsh_sync_state
  command=("$MU_ZSH_BIN" status --json "$@")
  [[ -n "$MU_ZSH_EFFECTIVE_SESSION_ID" ]] && command+=(-s "$MU_ZSH_EFFECTIVE_SESSION_ID")
  [[ -n "$MU_ZSH_EFFECTIVE_MODEL" ]] && command+=(--model "$MU_ZSH_EFFECTIVE_MODEL")
  "${command[@]}" 2>/dev/null
}

_mu_zsh_build_mode_prompt() {
  local status_json model context_raw context context_source context_segment cwd project_root project_segment attachment_segment
  local clean unclean_segment
  local escaped_model escaped_context escaped_project_root escaped_unclean_text

  # One jq pass extracts every prompt field as TSV; forking jq per field
  # dominates prompt-draw latency, so keep this to a single invocation.
  local tsv
  local -a fields
  _mu_zsh_sync_state
  status_json=$(_mu_zsh_status_json) || status_json=
  if [[ -n "$status_json" ]] && command -v jq >/dev/null 2>&1; then
    tsv=$(jq -r '[(.model.canonical // ""), (.context_percent // ""), (.project_root // ""), (if has("clean") then (.clean|tostring) else "" end), (.context_usage_source // "")] | @tsv' <<< "$status_json" 2>/dev/null) || tsv=
  fi
  fields=("${(@ps:\t:)tsv}")
  model=${fields[1]:-}
  [[ -n "$model" ]] || model=mu
  context_raw=${fields[2]:-}
  project_root=${fields[3]:-}
  clean=${fields[4]:-}
  context_source=${fields[5]:-}
  if [[ -n "$MU_ZSH_EFFECTIVE_SESSION_ID" ]]; then
    if [[ -z "$context_raw" || "$context_raw" == null ]]; then
      context=0%
    elif ! printf -v context '%.0f%%' "$context_raw" 2>/dev/null; then
      context=0%
    fi
    [[ "$context_source" == estimated ]] && context="~$context"
    escaped_context=$context
    escaped_context=${escaped_context//\%/%%}
    context_segment=" %F{$MU_ZSH_PROMPT_CONTEXT_COLOR}${escaped_context}%f"
  else
    context_segment=
  fi
  cwd=$PWD
  cwd=${cwd//\%/%%}
  escaped_model=$model
  escaped_model=${escaped_model//\%/%%}
  if [[ -z "$project_root" ]]; then
    project_segment=" %F{$MU_ZSH_PROMPT_PROJECT_COLOR}(global)%f"
  elif [[ "$project_root" != "$PWD" ]]; then
    escaped_project_root=$project_root
    escaped_project_root=${escaped_project_root//\%/%%}
    project_segment=" %F{$MU_ZSH_PROMPT_PROJECT_COLOR}(${escaped_project_root})%f"
  else
    project_segment=
  fi

  if (( MU_ZSH_EFFECTIVE_ATTACHMENT_COUNT )); then
    attachment_segment=" %F{$MU_ZSH_PROMPT_CONTEXT_COLOR}[${MU_ZSH_EFFECTIVE_ATTACHMENT_COUNT} attachments]%f"
  else
    attachment_segment=
  fi

  # When the tracked session's last turn was interrupted (unclean), surface it
  # so the user knows they can /retry to resume or just type to redirect.
  if [[ "$clean" == false ]]; then
    escaped_unclean_text=$MU_ZSH_PROMPT_UNCLEAN_TEXT
    escaped_unclean_text=${escaped_unclean_text//\%/%%}
    unclean_segment=" %F{$MU_ZSH_PROMPT_UNCLEAN_COLOR}[${escaped_unclean_text}]%f"
  else
    unclean_segment=
  fi

  print -r -- "%F{$MU_ZSH_PROMPT_MODEL_COLOR}${escaped_model}%f${context_segment} %F{$MU_ZSH_PROMPT_PWD_COLOR}${cwd}%f${project_segment}${unclean_segment}${attachment_segment}
${MU_ZSH_PROMPT_INPUT}"
}

_mu_zsh_refresh_prompt() {
  local mode_prompt

  mode_prompt=$(_mu_zsh_build_mode_prompt) || mode_prompt=$MU_ZSH_PROMPT_INPUT
  MU_ZSH_PROMPT=$mode_prompt
  [[ "$MU_ZSH_MODE" == mu ]] && PROMPT=$mode_prompt
}

_mu_zsh_disable_editor_plugins() {
  if (( $+ZSH_HIGHLIGHT_HIGHLIGHTERS )); then
    MU_ZSH_HAD_HIGHLIGHTERS=1
    MU_ZSH_SAVED_HIGHLIGHTERS=("${ZSH_HIGHLIGHT_HIGHLIGHTERS[@]}")
    ZSH_HIGHLIGHT_HIGHLIGHTERS=()
  else
    MU_ZSH_HAD_HIGHLIGHTERS=0
    MU_ZSH_SAVED_HIGHLIGHTERS=()
  fi

  MU_ZSH_DISABLED_AUTOSUGGESTIONS=0
  if (( ! ${+_ZSH_AUTOSUGGEST_DISABLED} )) && zle -l autosuggest-disable >/dev/null 2>&1; then
    if zle autosuggest-disable; then
      MU_ZSH_DISABLED_AUTOSUGGESTIONS=1
    fi
  fi
}

_mu_zsh_restore_editor_plugins() {
  if (( MU_ZSH_HAD_HIGHLIGHTERS )); then
    ZSH_HIGHLIGHT_HIGHLIGHTERS=("${MU_ZSH_SAVED_HIGHLIGHTERS[@]}")
  else
    unset ZSH_HIGHLIGHT_HIGHLIGHTERS
  fi

  if (( MU_ZSH_DISABLED_AUTOSUGGESTIONS )) && zle -l autosuggest-enable >/dev/null 2>&1; then
    zle autosuggest-enable
  fi
  MU_ZSH_DISABLED_AUTOSUGGESTIONS=0
}

_mu_zsh_run_hooks() {
  local hook
  for hook in "$@"; do
    [[ -z "$hook" ]] && continue
    if (( $+functions[$hook] )); then
      "$hook"
    else
      print -u2 -- "mu mu.zsh: hook function not found: $hook"
    fi
  done
}

_mu_zsh_reset_mode_prompt() {
  local skip_refresh=${1:-0}
  [[ "$MU_ZSH_MODE" == mu && "$skip_refresh" != 1 ]] && _mu_zsh_refresh_prompt
  zle reset-prompt
  zle -R
  zle -K mumode 2>/dev/null || true
}

_mu_zsh_slash_command_candidates() {
  local -a commands

  commands=(/attach /model)
  _mu_zsh_sync_state
  [[ -n "$MU_ZSH_EFFECTIVE_SESSION_ID" ]] && commands+=(/new /retry /compact)
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

_mu_zsh_model_completion_candidates() {
  local fragment=$1
  local suffix_only=${2:-0}
  local json
  json=$(_mu_zsh_status_json --include-models) || return 1
  command -v jq >/dev/null 2>&1 || return 1
  jq -r --arg fragment "$fragment" --arg suffix_only "$suffix_only" '
    [.available_models.providers[]?.models[]? | {
      canonical: (.id // ""),
      short: (.model_id // ""),
      efforts: (.supported_efforts // [])
    }] as $models
    | (reduce $models[] as $model ({}; .[$model.short] += 1)) as $counts
    | [
        if $suffix_only == "1" then
          $models[] as $model
          | select(
              $fragment == $model.canonical
              or ($fragment == $model.short and $counts[$model.short] == 1)
            )
          | $model.efforts[]? | ":" + .
        elif ($fragment | contains(":")) then
          $models[] as $model | $model.efforts[]? as $effort
          | "\($model.canonical):\($effort)",
            (if $counts[$model.short] == 1 then "\($model.short):\($effort)" else empty end)
        else
          $models[]
          | .canonical, (if $counts[.short] == 1 then .short else empty end)
        end
      ]
    | unique[]
  ' <<< "$json"
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

  [[ "$left" == "/attach "* ]] && return 0

  [[ "$left" != *[[:space:]]* ]]
}

_mu_zsh_completion_candidates() {
  local left arg

  left=${BUFFER[1,$CURSOR]}

  if [[ "$left" == "/model "* ]]; then
    arg=${left#"/model "}
    [[ "$arg" != *[[:space:]]* ]] || return 1
    _mu_zsh_model_completion_candidates "$arg"
    return
  fi

  [[ "$left" == "/attach "* ]] && return 1

  [[ "$left" == /* ]] || return 1
  [[ "$left" != *[[:space:]]* ]] || return 1

  _mu_zsh_slash_command_candidates
}

_mu_zsh_fallback_completion() {
  local left arg suffix
  local -a candidates effort_suffixes

  left=${BUFFER[1,$CURSOR]}
  if [[ "$left" == "/model "* ]]; then
    arg=${left#"/model "}
    effort_suffixes=("${(@f)$(_mu_zsh_model_completion_candidates "$arg" 1)}")
    effort_suffixes=("${(@)effort_suffixes:#}")
    if (( ${#effort_suffixes[@]} )); then
      compset -P "${(b)arg}"
      compadd -Q -S '' -- "${effort_suffixes[@]}"
      return
    fi
  fi

  candidates=("${(@f)$(_mu_zsh_completion_candidates)}")
  candidates=("${(@)candidates:#}")
  (( ${#candidates[@]} )) || return 1

  suffix=' '
  [[ "$left" == "/model "* ]] && suffix=''
  compadd -Q -S "$suffix" -- "${candidates[@]}"
}

_mu_zsh_completion_system() {
  local left arg suffix
  local -a candidates effort_suffixes
  local expl

  left=${BUFFER[1,$CURSOR]}
  if [[ "$left" == "/attach "* ]]; then
    compset -P '/attach '
    _files
    return
  fi

  if [[ "$left" == "/model "* ]]; then
    arg=${left#"/model "}
    effort_suffixes=("${(@f)$(_mu_zsh_model_completion_candidates "$arg" 1)}")
    effort_suffixes=("${(@)effort_suffixes:#}")
    if (( ${#effort_suffixes[@]} )); then
      compset -P "${(b)arg}"
      _wanted mu-model-effort expl 'model effort' \
        compadd -Q -S '' -- "${effort_suffixes[@]}"
      return
    fi
  fi

  candidates=("${(@f)$(_mu_zsh_completion_candidates)}")
  candidates=("${(@)candidates:#}")
  (( ${#candidates[@]} )) || return 1

  suffix=' '
  [[ "$left" == "/model "* ]] && suffix=''
  _wanted mu-slash-command expl 'mu slash command' \
    compadd -Q -S "$suffix" -- "${candidates[@]}"
}

_mu_zsh_use_completion_system() {
  # compinit may be loaded after this plugin, so register lazily.
  (( $+functions[_main_complete] && $+functions[compdef] )) || return 1
  [[ ${_comps[mu-zsh-slash]-} == _mu_zsh_completion_system ]] ||
    compdef _mu_zsh_completion_system mu-zsh-slash
}

_mu_zsh_complete_slash() {
  local before_buffer=$BUFFER before_cursor=$CURSOR

  _mu_zsh_slash_completion_context || return 1
  if _mu_zsh_use_completion_system; then
    local compcontext=mu-zsh-slash
    zle expand-or-complete
  else
    zle _mu_zsh_complete_widget
  fi

  if [[ "${before_buffer[1,$before_cursor]}" == "/model "* ]] &&
    [[ "$BUFFER" != "$before_buffer" || $CURSOR -ne $before_cursor ]] &&
    [[ "${BUFFER[1,$CURSOR]}" == "/model "* && "${BUFFER[1,$CURSOR]}" != *:* ]]; then
    _mu_zsh_list_slash_choices
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

_mu_zsh_require_effective_session() {
  local command=$1
  _mu_zsh_sync_state
  if [[ -z "$MU_ZSH_EFFECTIVE_SESSION_ID" ]]; then
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
  _mu_zsh_sync_state
  command=("$MU_ZSH_BIN" status --json --model "$model")
  [[ -n "$MU_ZSH_EFFECTIVE_SESSION_ID" ]] && command+=(-s "$MU_ZSH_EFFECTIVE_SESSION_ID")
  status_json=$("${command[@]}" 2>/dev/null) || return 1
  resolved=$(jq -r '.model.canonical // empty' <<< "$status_json" 2>/dev/null) || resolved=
  REPLY=${resolved:-$model}
  return 0
}

_mu_zsh_run_custom_slash_command() {
  local slash_command=$1
  local instruction=${2-}
  local name=${slash_command#/}
  local exit_status scope session_id
  local -a command

  _mu_zsh_set_scope_key_for_dir "$PWD"
  scope=$REPLY
  _mu_zsh_activate_scope "$scope"
  [[ -n "$MU_ZSH_EFFECTIVE_SESSION_ID" ]] ||
    _mu_zsh_create_session_for_scope "$scope" || return $?
  _mu_zsh_base_command_reply "$scope"
  session_id=$MU_ZSH_EFFECTIVE_SESSION_ID

  command=("${MU_ZSH_COMMAND_REPLY[@]}")
  local attachment
  for attachment in "${MU_ZSH_PENDING_ATTACHMENTS[@]}"; do
    command+=(-a "$attachment")
  done
  command+=("$name")
  MU_ZSH_PENDING_ATTACHMENTS=()
  MU_ZSH_EFFECTIVE_ATTACHMENT_COUNT=0

  if [[ -n "$instruction" ]]; then
    print -rn -- "$instruction" | "${command[@]}"
    exit_status=${pipestatus[2]}
  else
    "${command[@]}"
    exit_status=$?
  fi

  return $exit_status
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

  print -sr -- "$line"
  _mu_zsh_set_scope_key_for_dir "$PWD"
  scope=$REPLY
  case "$command" in
    /attach)
      if [[ -z "$rest" ]]; then
        _mu_zsh_activate_scope "$scope"
        if (( ${#MU_ZSH_PENDING_ATTACHMENTS[@]} )); then
          _mu_zsh_print_block_message "[mu] pending attachments: ${(j:, :)MU_ZSH_PENDING_ATTACHMENTS}"
        else
          _mu_zsh_print_block_message "[mu] no pending attachments"
        fi
        return 0
      fi
      if [[ "$rest" == --clear ]]; then
        _mu_zsh_activate_scope "$scope"
        MU_ZSH_PENDING_ATTACHMENTS=()
        MU_ZSH_EFFECTIVE_ATTACHMENT_COUNT=0
        _mu_zsh_print_block_message "[mu] cleared pending attachments"
        return 0
      fi
      if [[ "$rest" == *$'\n'* ]]; then
        _mu_zsh_print_block_message "[mu] /attach accepts exactly one file"
        return 1
      fi
      local attachment_path=$rest
      [[ "$attachment_path" == '~/'* ]] && attachment_path="${HOME:-}${attachment_path#\~}"
      attachment_path=${attachment_path:A}
      if [[ ! -f "$attachment_path" || ! -r "$attachment_path" ]]; then
        _mu_zsh_print_block_message "[mu] attachment is not a readable file: $rest"
        return 1
      fi
      _mu_zsh_activate_scope "$scope"
      MU_ZSH_PENDING_ATTACHMENTS+=("$attachment_path")
      local attachment_count=${#MU_ZSH_PENDING_ATTACHMENTS[@]}
      MU_ZSH_EFFECTIVE_ATTACHMENT_COUNT=$attachment_count
      local attachment_label=files
      (( attachment_count == 1 )) && attachment_label=file
      _mu_zsh_print_block_message "[mu] attached ${attachment_path:t} for the next message ($attachment_count $attachment_label)"
      ;;
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
      MU_ZSH_MODEL=$resolved_model
      MU_ZSH_EFFECTIVE_MODEL=$resolved_model
      _mu_zsh_print_block_message "[mu] next turns in this scope will use $resolved_model"
      ;;
    /new)
      _mu_zsh_validate_no_args "$command" "$rest" || return 1
      _mu_zsh_activate_scope "$scope"
      _mu_zsh_require_effective_session "$command" || return 1
      _mu_zsh_clear_session_state
      _mu_zsh_print_block_message "[mu] next turn will start a new session"
      ;;
    /retry)
      _mu_zsh_validate_no_args "$command" "$rest" || return 1
      _mu_zsh_activate_scope "$scope"
      _mu_zsh_require_effective_session "$command" || return 1
      session_id=$MU_ZSH_EFFECTIVE_SESSION_ID
      local -a retry_command
      retry_command=("$MU_ZSH_BIN" retry -s "$session_id")
      [[ -n "$MU_ZSH_EFFECTIVE_MODEL" ]] && retry_command+=(--model "$MU_ZSH_EFFECTIVE_MODEL")
      [[ -n "$MU_ZSH_OUTPUT" ]] && retry_command+=(--output "$MU_ZSH_OUTPUT")
      if "${retry_command[@]}"; then
        exit_status=0
      else
        exit_status=$?
      fi
      ;;
    /compact)
      _mu_zsh_activate_scope "$scope"
      _mu_zsh_require_effective_session "$command" || return 1
      session_id=$MU_ZSH_EFFECTIVE_SESSION_ID
      if [[ -n "$instruction" ]]; then
        print -rn -- "$instruction" | "$MU_ZSH_BIN" compact --session "$session_id"
        exit_status=${pipestatus[2]}
      else
        if "$MU_ZSH_BIN" compact --session "$session_id"; then
          exit_status=0
        else
          exit_status=$?
        fi
      fi
      print
      ;;
    *)
      if _mu_zsh_has_custom_slash_command "$command"; then
        _mu_zsh_run_custom_slash_command "$command" "$instruction"
        exit_status=$?
      else
        _mu_zsh_print_block_message "[mu] unknown slash command: $command"
        return 1
      fi
      ;;
  esac

  _mu_zsh_sync_state "$scope"
  return $exit_status
}

_mu_zsh_enter_mode() {
  [[ "$MU_ZSH_MODE" == mu ]] && return 0

  MU_ZSH_MODE=mu
  MU_ZSH_SAVED_KEYMAP=${KEYMAP:-main}
  MU_ZSH_ORIGINAL_PROMPT=$PROMPT
  MU_ZSH_ORIGINAL_RPROMPT=$RPROMPT
  _mu_zsh_refresh_prompt
  RPROMPT=
  _mu_zsh_disable_editor_plugins
  _mu_zsh_run_hooks "${MU_ZSH_ENTER_HOOKS[@]}"
  zle -K mumode 2>/dev/null || true
}

_mu_zsh_exit_mode() {
  [[ "$MU_ZSH_MODE" == shell ]] && return 0

  MU_ZSH_MODE=shell
  zle -K "${MU_ZSH_SAVED_KEYMAP:-main}" 2>/dev/null || zle -K main 2>/dev/null || true
  PROMPT=$MU_ZSH_ORIGINAL_PROMPT
  RPROMPT=$MU_ZSH_ORIGINAL_RPROMPT
  _mu_zsh_restore_editor_plugins
  _mu_zsh_run_hooks "${MU_ZSH_EXIT_HOOKS[@]}"
}

_mu_zsh_insert_newline() {
  [[ "$MU_ZSH_MODE" == mu ]] || {
    zle self-insert
    return
  }

  BUFFER="${BUFFER[1,CURSOR]}"$'\n'"${BUFFER[CURSOR+1,-1]}"
  (( CURSOR += 1 ))
}

_mu_zsh_submit_prompt() {
  local input=$1
  local exit_status
  local scope session_id
  local -a command

  _mu_zsh_set_scope_key_for_dir "$PWD"
  scope=$REPLY
  _mu_zsh_activate_scope "$scope"
  [[ -n "$MU_ZSH_EFFECTIVE_SESSION_ID" ]] ||
    _mu_zsh_create_session_for_scope "$scope" || return $?
  # Create the session before recording history so the replay command can
  # address the exact session even for the first turn in a scope.
  _mu_zsh_record_history "$input" "$scope"
  _mu_zsh_base_command_reply "$scope"
  session_id=$MU_ZSH_EFFECTIVE_SESSION_ID
  command=("${MU_ZSH_COMMAND_REPLY[@]}")
  local attachment
  for attachment in "${MU_ZSH_PENDING_ATTACHMENTS[@]}"; do
    command+=(-a "$attachment")
  done
  MU_ZSH_PENDING_ATTACHMENTS=()
  MU_ZSH_EFFECTIVE_ATTACHMENT_COUNT=0

  "${command[@]}" <<< "$input"
  exit_status=$?

  return $exit_status
}

_mu_zsh_tab() {
  if [[ "$MU_ZSH_MODE" == mu ]]; then
    if _mu_zsh_slash_completion_context; then
      _mu_zsh_complete_slash
      return
    fi

    if (( CURSOR == 0 )); then
      _mu_zsh_exit_mode
      zle reset-prompt
      zle -K "${MU_ZSH_SAVED_KEYMAP:-main}" 2>/dev/null || zle -K main 2>/dev/null || true
      return
    fi

    zle self-insert
    return
  fi

  if (( CURSOR == 0 )); then
    _mu_zsh_enter_mode
    _mu_zsh_reset_mode_prompt 1
    return
  fi

  [[ -n "$MU_ZSH_ORIGINAL_TAB_WIDGET" ]] && zle "$MU_ZSH_ORIGINAL_TAB_WIDGET"
}

_mu_zsh_slash() {
  local should_complete=0

  if [[ "$MU_ZSH_MODE" == mu && "$BUFFER" != /* && "$CURSOR" -eq 0 ]]; then
    should_complete=1
  fi

  if [[ -n "$MU_ZSH_ORIGINAL_SLASH_WIDGET" && "$MU_ZSH_ORIGINAL_SLASH_WIDGET" != _mu_zsh_slash ]]; then
    zle "$MU_ZSH_ORIGINAL_SLASH_WIDGET"
  else
    zle .self-insert
  fi

  (( should_complete )) && _mu_zsh_list_slash_choices
}

_mu_zsh_accept() {
  if [[ "$MU_ZSH_MODE" != mu ]]; then
    zle .accept-line
    return
  fi

  local input=$BUFFER
  if [[ -z "${input//[[:space:]]/}" ]]; then
    zle .accept-line
    return
  fi

  MU_ZSH_PENDING_INPUT=$input
  MU_ZSH_PENDING_PROMPT=$PROMPT
  MU_ZSH_PENDING_SUBMIT=1
  # Accept the visible draft normally. The line-finish hook freezes that
  # display and clears the command before zsh can parse it.
  zle .accept-line
}

_mu_zsh_finish_pending() {
  (( MU_ZSH_PENDING_SUBMIT )) || return 0

  zle -I
  BUFFER=
  CURSOR=0
}

_mu_zsh_dispatch_pending() {
  (( MU_ZSH_PENDING_SUBMIT )) || return 0

  local input=$MU_ZSH_PENDING_INPUT
  PROMPT=$MU_ZSH_PENDING_PROMPT
  MU_ZSH_PENDING_INPUT=
  MU_ZSH_PENDING_PROMPT=
  MU_ZSH_PENDING_SUBMIT=0

  if [[ "$input" == /* ]]; then
    _mu_zsh_run_slash_command "$input"
  else
    _mu_zsh_submit_prompt "$input"
  fi
  [[ "$MU_ZSH_MODE" == mu ]] && _mu_zsh_refresh_prompt
}

_mu_zsh_line_init() {
  [[ "$MU_ZSH_MODE" == mu ]] && _mu_zsh_refresh_prompt
  if [[ "$MU_ZSH_MODE" == mu ]]; then
    zle -K mumode 2>/dev/null || true
  fi
}

mu-zsh-mode() {
  _mu_zsh_enter_mode
  _mu_zsh_reset_mode_prompt 1
}

mu-zsh-exit-mode() {
  _mu_zsh_exit_mode
  zle reset-prompt
  zle -K "${MU_ZSH_SAVED_KEYMAP:-main}" 2>/dev/null || zle -K main 2>/dev/null || true
}

_mu_zsh_configure_keymap() {
  bindkey -M mumode '^M' _mu_zsh_accept
  bindkey -M mumode '^J' _mu_zsh_accept
  bindkey -M mumode $'\e[13;2u' _mu_zsh_insert_newline
  bindkey -M mumode '^I' _mu_zsh_tab
  bindkey -M mumode '/' _mu_zsh_slash
  bindkey -M mumode $'\e[A' up-line
  bindkey -M mumode $'\eOA' up-line
  bindkey -M mumode $'\e[B' down-line
  bindkey -M mumode $'\eOB' down-line
  # Ctrl-C is intentionally left inherited from the main keymap: real terminals
  # deliver it as SIGINT (the tty intercepts it before ZLE), which the shell
  # already handles by cancelling the draft and redrawing a fresh mu> prompt.
}

_mu_zsh_sync_state

if [[ -o zle ]]; then
  autoload -Uz add-zsh-hook 2>/dev/null || true
  autoload -Uz add-zle-hook-widget 2>/dev/null || true
  bindkey -N mumode main 2>/dev/null || true
  _mu_zsh_configure_keymap
  _mu_zsh_save_widget_bindings
  zle -C _mu_zsh_complete_widget complete-word _mu_zsh_fallback_completion
  zle -C _mu_zsh_list_widget list-choices _mu_zsh_fallback_completion
  zle -N _mu_zsh_tab
  zle -N _mu_zsh_slash
  zle -N _mu_zsh_accept
  zle -N _mu_zsh_insert_newline
  zle -N _mu_zsh_finish_pending
  zle -N _mu_zsh_line_init
  zle -N mu-zsh-mode
  zle -N mu-zsh-exit-mode
  add-zle-hook-widget line-finish _mu_zsh_finish_pending 2>/dev/null || true
  add-zle-hook-widget line-init _mu_zsh_line_init 2>/dev/null || true
  add-zsh-hook precmd _mu_zsh_dispatch_pending 2>/dev/null || true
  bindkey '^I' _mu_zsh_tab
fi
