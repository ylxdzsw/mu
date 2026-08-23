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
typeset -ga _MU_ZSH_PENDING_ATTACHMENTS=()
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
  _MU_ZSH_PENDING_ATTACHMENTS=()
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
  local scope=${2:-}
  local session_id
  local model
  local trap
  local quoted=${(qqq)input}
  if _mu_zsh_bundle_active "$scope"; then
    session_id=$MU_ZSH_SESSION_ID
    model=$_MU_ZSH_MODEL
    trap=$_MU_ZSH_TRAP
  fi

  local attachments=
  local trap_arg=
  local replay
  local attachment
  for attachment in "${_MU_ZSH_PENDING_ATTACHMENTS[@]}"; do
    attachments+=" -a ${(q)attachment}"
  done
  [[ -n "$trap" ]] && trap_arg=" --trap ${(q)trap}"
  if [[ -n "$session_id" ]]; then
    if [[ -n "$model" ]]; then
      replay="mu -s ${(q)session_id} --model ${(q)model}${trap_arg}${attachments} <<< $quoted"
    else
      replay="mu -s ${(q)session_id}${trap_arg}${attachments} <<< $quoted"
    fi
  elif [[ -n "$model" ]]; then
    replay="mu --model ${(q)model}${trap_arg}${attachments} <<< $quoted"
  else
    replay="mu${trap_arg}${attachments} <<< $quoted"
  fi
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
  local status_json model context_raw context context_source context_segment to_compact compaction_segment cwd project_root project_segment attachment_segment trap_segment
  local clean unclean_segment bundle_active=0 attachment_count=0
  local escaped_model escaped_context escaped_project_root escaped_unclean_text

  # One jq pass extracts every prompt field as TSV; forking jq per field
  # dominates prompt-draw latency, so keep this to a single invocation.
  local tsv
  local -a fields
  if _mu_zsh_bundle_active; then
    bundle_active=1
    attachment_count=${#_MU_ZSH_PENDING_ATTACHMENTS[@]}
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

  if (( attachment_count )); then
    attachment_segment=" %F{5}[${attachment_count} attachments]%f"
  else
    attachment_segment=
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

  print -r -- "%F{12}${escaped_model}%f${context_segment}${compaction_segment} %F{6}${cwd}%f${project_segment}${unclean_segment}${attachment_segment}${trap_segment}
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

  commands=(/attach /load /model /trap)
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

_mu_zsh_model_completion_candidates() {
  local fragment=$1
  local suffix_only=${2:-0}
  local -a command
  local json
  command=(mu status --json --include-models)
  _mu_zsh_bundle_active && [[ -n "$MU_ZSH_SESSION_ID" ]] && command+=(-s "$MU_ZSH_SESSION_ID")
  json=$("${command[@]}" 2>/dev/null) || return 1
  command -v jq >/dev/null 2>&1 || return 1
  jq -r --arg fragment "$fragment" --arg suffix_only "$suffix_only" '
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
    | [
        if $suffix_only == "1" then
          ($fragment | if endswith(":") then .[:-1] else . end) as $base
          | $models[] as $model
          | select(
              $base == $model.canonical
              or $base == $model.short
            )
          | $model.efforts[]? | ":" + .
        elif ($fragment | contains(":")) then
          ($models[] as $model | $model.efforts[]? as $effort
            | "\($model.canonical):\($effort)",
              "\($model.short):\($effort)")
        else
          $models[]
          | .canonical, .short
        end
      ]
    | dedup
    | if $suffix_only == "1" then
        to_entries
        | sort_by((.value | ltrimstr(":") | effort_rank), .key)
        | .[].value
      else
        .[]
      end
  ' <<< "$json"
}

_mu_zsh_model_completion_transition() {
  local fragment=$1
  local target=$2
  local -a command result
  local json

  command=(mu status --json --include-models)
  _mu_zsh_bundle_active && [[ -n "$MU_ZSH_SESSION_ID" ]] && command+=(-s "$MU_ZSH_SESSION_ID")
  json=$("${command[@]}" 2>/dev/null) || return 1
  command -v jq >/dev/null 2>&1 || return 1
  result=("${(@f)$(
    jq -r --arg fragment "$fragment" '
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
      | if ($fragment | contains("/")) then
          ($models | map(select(.canonical == $fragment)) | .[0]) as $model
          | select($model != null)
          | select(([
              $models[]
              | select(.canonical != $fragment and (.canonical | startswith($fragment)))
            ] | length) == 0)
          | [$fragment]
            + (($model.efforts | dedup)
              | to_entries
              | sort_by((.value | effort_rank), .key)
              | map(":" + .value))
        else
          select(([
            $models[] | select(.short == $fragment)
          ] | length) > 0)
          | select(([
              $models[]
              | select(.short != $fragment and (.short | startswith($fragment)))
            ] | length) == 0)
          | [$fragment]
            + ([
                $models[]
                | select(.short == $fragment)
                | .efforts[]?
              ] | dedup
              | to_entries
              | sort_by((.value | effort_rank), .key)
              | map(":" + .value))
        end
      | .[]
    ' <<< "$json"
  )}")
  (( ${#result[@]} > 1 )) || return 1
  set -A "$target" "${result[@]:1}"
  return 0
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

  if [[ "$left" == "/trap "* ]]; then
    arg=${left#"/trap "}
    [[ "$arg" != *[[:space:]]* ]] || return 1
    print -l -- off destructive reversible all default
    return
  fi

  [[ "$left" == "/attach "* ]] && return 1

  [[ "$left" == /* ]] || return 1
  [[ "$left" != *[[:space:]]* ]] || return 1

  _mu_zsh_slash_command_candidates
}

_mu_zsh_fallback_completion() {
  local left arg model_fragment suffix effort_suffix
  local -a candidates effort_suffixes effort_candidates

  left=${BUFFER[1,$CURSOR]}
  if [[ "$left" == "/model "* ]]; then
    arg=${left#"/model "}
    if [[ "$arg" == *:* ]]; then
      model_fragment=${arg%:*}
      effort_suffixes=("${(@f)$(_mu_zsh_model_completion_candidates "$model_fragment:" 1)}")
      effort_suffixes=("${(@)effort_suffixes:#}")
      if (( ${#effort_suffixes[@]} )); then
        compset -P '*:' 2>/dev/null || true
        for effort_suffix in "${effort_suffixes[@]}"; do
          effort_candidates+=("${effort_suffix#:}")
        done
        compadd -V mu-model-effort -Q -S '' -- "${effort_candidates[@]}"
        return
      fi
    fi
  fi

  candidates=("${(@f)$(_mu_zsh_completion_candidates)}")
  candidates=("${(@)candidates:#}")
  (( ${#candidates[@]} )) || return 1

  suffix=' '
  [[ "$left" == "/model "* ]] && suffix=''
  [[ "$left" == "/trap "* ]] && suffix=''
  compadd -Q -S "$suffix" -- "${candidates[@]}"
}

_mu_zsh_completion_system() {
  local left arg model_fragment suffix effort_suffix
  local -a candidates effort_suffixes effort_candidates
  local expl

  left=${BUFFER[1,$CURSOR]}
  if [[ "$left" == "/attach "* ]]; then
    compset -P '/attach '
    _files
    return
  fi

  if [[ "$left" == "/model "* ]]; then
    arg=${left#"/model "}
    if [[ "$arg" == *:* ]]; then
      model_fragment=${arg%:*}
      effort_suffixes=("${(@f)$(_mu_zsh_model_completion_candidates "$model_fragment:" 1)}")
      effort_suffixes=("${(@)effort_suffixes:#}")
      if (( ${#effort_suffixes[@]} )); then
        compset -P '*:' 2>/dev/null || true
        for effort_suffix in "${effort_suffixes[@]}"; do
          effort_candidates+=("${effort_suffix#:}")
        done
        _wanted -V mu-model-effort expl 'model effort' \
          compadd -Q -S '' -- "${effort_candidates[@]}"
        return
      fi
    fi
  fi

  candidates=("${(@f)$(_mu_zsh_completion_candidates)}")
  candidates=("${(@)candidates:#}")
  (( ${#candidates[@]} )) || return 1

  suffix=' '
  [[ "$left" == "/model "* ]] && suffix=''
  [[ "$left" == "/trap "* ]] && suffix=''
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
  local before_left=${BUFFER[1,$CURSOR]}
  local completion_mode=complete
  local model_arg effort
  local -a display_efforts effort_suffixes

  _mu_zsh_slash_completion_context || return 1
  if [[ "$before_left" == "/model "* ]]; then
    model_arg=${before_left#"/model "}
    if [[ "$model_arg" == *: ]]; then
      effort_suffixes=("${(@f)$(_mu_zsh_model_completion_candidates "$model_arg" 1)}")
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

_mu_zsh_resolve_load_output() {
  local session_id=$1
  local status_json output

  status_json=$(mu status --json -s "$session_id") || return $?
  output=$(jq -r '.output // empty' <<< "$status_json" 2>/dev/null) || output=
  case "$output" in
    final|concise|detail|full)
      REPLY=$output
      ;;
    *)
      print -u2 -- "mu mu.zsh: status returned an invalid output density"
      return 1
      ;;
  esac
}

_mu_zsh_resolve_load_session() {
  local requested_session=$1
  local status_json session_id

  if [[ -n "$requested_session" ]]; then
    REPLY=$requested_session
    return 0
  fi

  status_json=$(mu status --json --continue) || return $?
  session_id=$(jq -r '.session_id // empty' <<< "$status_json" 2>/dev/null) || {
    print -u2 -- "mu mu.zsh: could not resolve current session from status"
    return 1
  }
  if [[ -z "$session_id" ]]; then
    _mu_zsh_print_block_message "[mu] no sessions found in active scope"
    return 1
  fi
  if [[ ! "$session_id" =~ '^ses_[0-9a-hjkmnpqrstvwxyz]{8}$' ]]; then
    print -u2 -- "mu mu.zsh: status returned an invalid session id"
    return 1
  fi
  REPLY=$session_id
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
  [[ -n "$MU_ZSH_SESSION_ID" ]] ||
    _mu_zsh_create_session_for_scope "$scope" || return $?
  _mu_zsh_base_command command "$scope"
  session_id=$MU_ZSH_SESSION_ID

  local attachment
  for attachment in "${_MU_ZSH_PENDING_ATTACHMENTS[@]}"; do
    command+=(-a "$attachment")
  done
  command+=("$name")
  _MU_ZSH_PENDING_ATTACHMENTS=()

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
  local command instruction rest session_id scope resolved_model load_output
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
    /attach)
      if [[ -z "$rest" ]]; then
        _mu_zsh_activate_scope "$scope"
        if (( ${#_MU_ZSH_PENDING_ATTACHMENTS[@]} )); then
          _mu_zsh_print_block_message "[mu] pending attachments: ${(j:, :)_MU_ZSH_PENDING_ATTACHMENTS}"
        else
          _mu_zsh_print_block_message "[mu] no pending attachments"
        fi
        return 0
      fi
      if [[ "$rest" == --clear ]]; then
        _mu_zsh_activate_scope "$scope"
        _MU_ZSH_PENDING_ATTACHMENTS=()
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
      _MU_ZSH_PENDING_ATTACHMENTS+=("$attachment_path")
      local attachment_count=${#_MU_ZSH_PENDING_ATTACHMENTS[@]}
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
      _mu_zsh_resolve_load_session "$rest" || return $?
      session_id=$REPLY
      _mu_zsh_resolve_load_output "$session_id" || return $?
      load_output=$REPLY
      mu transcript --session "$session_id" --output "$load_output" || return $?
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
        _mu_zsh_run_custom_slash_command "$command" "$instruction"
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
  local exit_status
  local scope session_id
  local -a command

  _mu_zsh_set_scope_key_for_dir "$PWD"
  scope=$REPLY
  _mu_zsh_activate_scope "$scope"
  [[ -n "$MU_ZSH_SESSION_ID" ]] ||
    _mu_zsh_create_session_for_scope "$scope" || return $?
  # Create the session before recording history so the replay command can
  # address the exact session even for the first turn in a scope.
  _mu_zsh_record_history "$input" "$scope"
  _mu_zsh_base_command command "$scope"
  session_id=$MU_ZSH_SESSION_ID
  local attachment
  for attachment in "${_MU_ZSH_PENDING_ATTACHMENTS[@]}"; do
    command+=(-a "$attachment")
  done
  _MU_ZSH_PENDING_ATTACHMENTS=()

  "${command[@]}" <<< "$input"
  exit_status=$?

  return $exit_status
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
    _mu_zsh_enter_mode
    _mu_zsh_reset_mode_prompt 1
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
  _mu_zsh_enter_mode
  _mu_zsh_reset_mode_prompt 1
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
