# zsh integration for mu.
#
# Source this file from .zshrc to add a shell-native mu prompt mode:
# press Tab at cursor position 0 to toggle "mu>" mode while preserving the
# current buffer, Enter to submit one non-blank mu turn, Ctrl+C to cancel the
# current mu prompt while leaving the typed line in scrollback, and Ctrl+D to
# keep normal shell EOF behavior even from "mu>" mode.

typeset -g MU_ZSH_MODE=${MU_ZSH_MODE:-shell}
typeset -g MU_ZSH_SESSION_ID=${MU_ZSH_SESSION_ID:-}
typeset -g MU_ZSH_SESSION_FILE=${MU_ZSH_SESSION_FILE:-${TMPDIR:-/tmp}/mu-zsh-${$}.session}
typeset -g MU_ZSH_BIN=${MU_ZSH_BIN:-mu}
typeset -g MU_ZSH_OUTPUT=${MU_ZSH_OUTPUT:-terminal}
typeset -g MU_ZSH_PROMPT_INPUT=${MU_ZSH_PROMPT_INPUT:-${MU_ZSH_PROMPT:-'mu> '}}
typeset -g MU_ZSH_PROMPT=${MU_ZSH_PROMPT:-$MU_ZSH_PROMPT_INPUT}
typeset -g MU_ZSH_PROMPT_MODEL_COLOR=${MU_ZSH_PROMPT_MODEL_COLOR:-green}
typeset -g MU_ZSH_PROMPT_CONTEXT_COLOR=${MU_ZSH_PROMPT_CONTEXT_COLOR:-magenta}
typeset -g MU_ZSH_PROMPT_PWD_COLOR=${MU_ZSH_PROMPT_PWD_COLOR:-yellow}
typeset -g MU_ZSH_PROMPT_PROJECT_COLOR=${MU_ZSH_PROMPT_PROJECT_COLOR:-cyan}
typeset -g MU_ZSH_ORIGINAL_PROMPT=${MU_ZSH_ORIGINAL_PROMPT:-}
typeset -g MU_ZSH_ORIGINAL_RPROMPT=${MU_ZSH_ORIGINAL_RPROMPT:-}
typeset -g MU_ZSH_SAVED_KEYMAP=${MU_ZSH_SAVED_KEYMAP:-main}
typeset -g MU_ZSH_ORIGINAL_TAB_WIDGET=${MU_ZSH_ORIGINAL_TAB_WIDGET:-}
typeset -g MU_ZSH_ORIGINAL_STTY=${MU_ZSH_ORIGINAL_STTY:-}
typeset -g MU_ZSH_HISTORY_BUFFER=${MU_ZSH_HISTORY_BUFFER:-}
typeset -gi MU_ZSH_HISTORY_CURSOR=${MU_ZSH_HISTORY_CURSOR:-0}
typeset -gi MU_ZSH_HISTORY_HISTNO=${MU_ZSH_HISTORY_HISTNO:-0}
typeset -gi MU_ZSH_HAD_HIGHLIGHTERS=${MU_ZSH_HAD_HIGHLIGHTERS:-0}
typeset -gi MU_ZSH_DISABLED_AUTOSUGGESTIONS=${MU_ZSH_DISABLED_AUTOSUGGESTIONS:-0}
typeset -ga MU_ZSH_SAVED_HIGHLIGHTERS
typeset -ga MU_ZSH_ENTER_HOOKS
typeset -ga MU_ZSH_EXIT_HOOKS
typeset -gA MU_ZSH_SHELL_UP_WIDGETS
typeset -gA MU_ZSH_SHELL_DOWN_WIDGETS

_mu_zsh_widget_for_key() {
  local key=$1
  local binding
  binding=${${(z)$(bindkey "$key" 2>/dev/null)}[2]}
  [[ -n "$binding" ]] && print -r -- "$binding"
}

_mu_zsh_save_widget_bindings() {
  [[ -z "$MU_ZSH_ORIGINAL_TAB_WIDGET" ]] && MU_ZSH_ORIGINAL_TAB_WIDGET=$(_mu_zsh_widget_for_key '^I')
  [[ "$MU_ZSH_ORIGINAL_TAB_WIDGET" == _mu_zsh_tab ]] && MU_ZSH_ORIGINAL_TAB_WIDGET=

  [[ -z "$MU_ZSH_ORIGINAL_TAB_WIDGET" ]] && MU_ZSH_ORIGINAL_TAB_WIDGET=expand-or-complete

  local key widget
  for key in $'\e[A' $'\eOA'; do
    widget=$(_mu_zsh_widget_for_key "$key")
    [[ -z "$widget" || "$widget" == _mu_zsh_history_up ]] && widget=up-line-or-history
    MU_ZSH_SHELL_UP_WIDGETS[$key]=$widget
  done

  for key in $'\e[B' $'\eOB'; do
    widget=$(_mu_zsh_widget_for_key "$key")
    [[ -z "$widget" || "$widget" == _mu_zsh_shell_down ]] && widget=down-line-or-history
    MU_ZSH_SHELL_DOWN_WIDGETS[$key]=$widget
  done
  return 0
}

_mu_zsh_call_original_widget() {
  local widget=$1
  if [[ -n "$widget" && "$widget" != _mu_zsh_tab ]]; then
    zle "$widget"
  fi
}

_mu_zsh_saved_up_widget() {
  local key=$1
  local widget=${MU_ZSH_SHELL_UP_WIDGETS[$key]:-up-line-or-history}
  [[ -z "$widget" || "$widget" == _mu_zsh_history_up ]] && widget=up-line-or-history
  print -r -- "$widget"
}

_mu_zsh_saved_down_widget() {
  local key=$1
  local widget=${MU_ZSH_SHELL_DOWN_WIDGETS[$key]:-down-line-or-history}
  [[ -z "$widget" || "$widget" == _mu_zsh_shell_down ]] && widget=down-line-or-history
  print -r -- "$widget"
}

_mu_zsh_quote_prompt() {
  print -r -- "${(qqq)1}"
}

_mu_zsh_record_history() {
  local prompt=$1
  local quoted
  quoted=$(_mu_zsh_quote_prompt "$prompt")

  if [[ -n "$MU_ZSH_SESSION_ID" ]]; then
    print -sr -- "$MU_ZSH_BIN -s ${(q)MU_ZSH_SESSION_ID} --output ${(q)MU_ZSH_OUTPUT} <<< $quoted"
  else
    print -sr -- "$MU_ZSH_BIN --output ${(q)MU_ZSH_OUTPUT} <<< $quoted"
  fi
}

_mu_zsh_read_session_file() {
  [[ -r "$MU_ZSH_SESSION_FILE" ]] || return 0

  local id
  id=$(<"$MU_ZSH_SESSION_FILE")
  id=${id//$'\n'/}
  [[ -n "$id" ]] && MU_ZSH_SESSION_ID=$id
}

_mu_zsh_build_command() {
  local -a command
  command=("$MU_ZSH_BIN" --output "$MU_ZSH_OUTPUT")
  [[ -n "$MU_ZSH_SESSION_ID" ]] && command+=(-s "$MU_ZSH_SESSION_ID")
  print -r -- "${(j: :)${(q)command[@]}}"
}

_mu_zsh_escape_prompt_text() {
  local text=$1
  text=${text//\%/%%}
  print -r -- "$text"
}

_mu_zsh_status_json() {
  local -a command
  command=("$MU_ZSH_BIN" status --json)
  [[ -n "$MU_ZSH_SESSION_ID" ]] && command+=(-s "$MU_ZSH_SESSION_ID")
  "${command[@]}" 2>/dev/null
}

_mu_zsh_status_field() {
  local json=$1
  local key=$2
  local remainder

  remainder=${json#*\"$key\":}
  [[ "$remainder" == "$json" ]] && return 1

  if [[ "$remainder" == \"* ]]; then
    remainder=${remainder#\"}
    print -r -- "${remainder%%\"*}"
    return 0
  fi

  remainder=${remainder%%,*}
  remainder=${remainder%%\}*}
  print -r -- "$remainder"
}

_mu_zsh_format_context_percent() {
  local raw=$1
  local formatted

  if [[ -z "$raw" || "$raw" == null ]]; then
    print -r -- "0%"
    return 0
  fi

  formatted=$(printf '%.0f%%' "$raw" 2>/dev/null) || {
    print -r -- "0%"
    return 0
  }
  print -r -- "$formatted"
}

_mu_zsh_build_mode_prompt() {
  local status_json model context_raw context cwd project_root project_segment

  status_json=$(_mu_zsh_status_json) || status_json=
  model=$(_mu_zsh_status_field "$status_json" model_id 2>/dev/null) || model=mu
  context_raw=$(_mu_zsh_status_field "$status_json" context_percent 2>/dev/null) || context_raw=
  project_root=$(_mu_zsh_status_field "$status_json" project_root 2>/dev/null) || project_root=
  [[ "$project_root" == null ]] && project_root=
  context=$(_mu_zsh_format_context_percent "$context_raw")
  cwd=$(_mu_zsh_escape_prompt_text "$PWD")
  if [[ -n "$project_root" && "$project_root" != "$PWD" ]]; then
    project_segment=" %F{$MU_ZSH_PROMPT_PROJECT_COLOR}($(_mu_zsh_escape_prompt_text "$project_root"))%f"
  else
    project_segment=
  fi

  print -r -- "%F{$MU_ZSH_PROMPT_MODEL_COLOR}$(_mu_zsh_escape_prompt_text "$model")%f %F{$MU_ZSH_PROMPT_CONTEXT_COLOR}$(_mu_zsh_escape_prompt_text "$context")%f %F{$MU_ZSH_PROMPT_PWD_COLOR}${cwd}%f${project_segment}
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
      print -u2 -- "mu.zsh: hook function not found: $hook"
    fi
  done
}

_mu_zsh_capture_tty_state() {
  [[ -n "$MU_ZSH_ORIGINAL_STTY" ]] && return 0
  [[ -t 0 ]] || return 0
  MU_ZSH_ORIGINAL_STTY=$(stty -g 2>/dev/null || true)
}

_mu_zsh_restore_tty_state() {
  [[ -n "$MU_ZSH_ORIGINAL_STTY" ]] || return 0
  stty "$MU_ZSH_ORIGINAL_STTY" 2>/dev/null || true
  MU_ZSH_ORIGINAL_STTY=
}

_mu_zsh_apply_prompt_tty() {
  [[ -n "$MU_ZSH_ORIGINAL_STTY" ]] || return 0
  stty eof '^]' 2>/dev/null || true
}

_mu_zsh_clear_history_return() {
  MU_ZSH_HISTORY_BUFFER=
  MU_ZSH_HISTORY_CURSOR=0
  MU_ZSH_HISTORY_HISTNO=0
}

_mu_zsh_reset_mode_prompt() {
  [[ "$MU_ZSH_MODE" == mu ]] && _mu_zsh_refresh_prompt
  zle reset-prompt
  _mu_zsh_apply_prompt_tty
  zle -K mumode 2>/dev/null || true
}

_mu_zsh_redraw_mode_prompt() {
  zle -I
  print
  _mu_zsh_clear_prompt
  _mu_zsh_reset_mode_prompt
}

_mu_zsh_enter_mode() {
  [[ "$MU_ZSH_MODE" == mu ]] && return 0

  _mu_zsh_capture_tty_state
  _mu_zsh_clear_history_return
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

_mu_zsh_clear_prompt() {
  BUFFER=
  CURSOR=0
}

_mu_zsh_submit_prompt() {
  local prompt=$1
  local exit_status

  _mu_zsh_record_history "$prompt"

  if [[ -n "$MU_ZSH_SESSION_ID" ]]; then
    "$MU_ZSH_BIN" -s "$MU_ZSH_SESSION_ID" --output "$MU_ZSH_OUTPUT" <<< "$prompt"
    exit_status=$?
  else
    MU_SESSION_FILE=$MU_ZSH_SESSION_FILE "$MU_ZSH_BIN" --output "$MU_ZSH_OUTPUT" <<< "$prompt"
    exit_status=$?
    _mu_zsh_read_session_file
  fi

  return $exit_status
}

_mu_zsh_shell_eof() {
  if [[ -z "$BUFFER" ]]; then
    _mu_zsh_clear_history_return
    _mu_zsh_restore_tty_state
    BUFFER=exit
    CURSOR=${#BUFFER}
    zle .accept-line
    return
  fi

  if (( CURSOR < ${#BUFFER} )); then
    zle delete-char
  fi
}

_mu_zsh_tab() {
  if [[ "$MU_ZSH_MODE" == mu ]]; then
    if (( CURSOR == 0 )); then
      _mu_zsh_clear_history_return
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
    _mu_zsh_reset_mode_prompt
    return
  fi

  _mu_zsh_call_original_widget "$MU_ZSH_ORIGINAL_TAB_WIDGET"
}

_mu_zsh_backspace() {
  zle backward-delete-char
}

_mu_zsh_interrupt() {
  if [[ "$MU_ZSH_MODE" != mu ]]; then
    zle send-break
    return
  fi

  _mu_zsh_redraw_mode_prompt
}

_mu_zsh_eof() {
  if [[ "$MU_ZSH_MODE" != mu ]]; then
    _mu_zsh_shell_eof
    return
  fi

  if [[ -z "$BUFFER" ]]; then
    _mu_zsh_exit_mode
    _mu_zsh_shell_eof
    return
  fi

  if (( CURSOR < ${#BUFFER} )); then
    zle delete-char
  fi
}

_mu_zsh_accept() {
  if [[ "$MU_ZSH_MODE" != mu ]]; then
    _mu_zsh_clear_history_return
    _mu_zsh_restore_tty_state
    zle .accept-line
    return
  fi

  local prompt=$BUFFER
  if [[ -z "${prompt//[[:space:]]/}" ]]; then
    _mu_zsh_redraw_mode_prompt
    return
  fi

  zle -I
  _mu_zsh_clear_prompt
  _mu_zsh_restore_tty_state
  _mu_zsh_submit_prompt "$prompt"
  _mu_zsh_capture_tty_state
  _mu_zsh_apply_prompt_tty
  _mu_zsh_reset_mode_prompt
}

_mu_zsh_history_up() {
  local before_histno=$HISTNO
  local before_buffer=$BUFFER
  local before_cursor=$CURSOR
  local key=${KEYS:-$'\e[A'}
  local widget

  widget=$(_mu_zsh_saved_up_widget "$key")
  zle "$widget"

  if [[ "$MU_ZSH_MODE" == mu && "$HISTNO" -ne "$before_histno" ]]; then
    MU_ZSH_HISTORY_BUFFER=$before_buffer
    MU_ZSH_HISTORY_CURSOR=$before_cursor
    MU_ZSH_HISTORY_HISTNO=$before_histno
    _mu_zsh_exit_mode
    zle reset-prompt
  fi
}

_mu_zsh_history_down() {
  local key=${KEYS:-$'\e[B'}
  local widget

  widget=$(_mu_zsh_saved_down_widget "$key")
  zle "$widget"
}

_mu_zsh_shell_down() {
  local key=${KEYS:-$'\e[B'}
  local widget cursor

  widget=$(_mu_zsh_saved_down_widget "$key")
  zle "$widget"

  if [[ -n "$MU_ZSH_HISTORY_BUFFER" ]] &&
     (( HISTNO == MU_ZSH_HISTORY_HISTNO )) &&
     [[ "$BUFFER" == "$MU_ZSH_HISTORY_BUFFER" ]]; then
    cursor=$MU_ZSH_HISTORY_CURSOR
    _mu_zsh_enter_mode
    CURSOR=$cursor
    _mu_zsh_reset_mode_prompt
  fi
}

_mu_zsh_line_init() {
  _mu_zsh_capture_tty_state
  [[ "$MU_ZSH_MODE" == mu ]] && _mu_zsh_refresh_prompt
  _mu_zsh_apply_prompt_tty
  if [[ "$MU_ZSH_MODE" == mu ]]; then
    zle -K mumode 2>/dev/null || true
  fi
}

_mu_zsh_cleanup() {
  _mu_zsh_restore_tty_state
}

mu-zsh-mode() {
  _mu_zsh_enter_mode
  _mu_zsh_reset_mode_prompt
}

mu-zsh-exit-mode() {
  _mu_zsh_clear_history_return
  _mu_zsh_exit_mode
  zle reset-prompt
  zle -K "${MU_ZSH_SAVED_KEYMAP:-main}" 2>/dev/null || zle -K main 2>/dev/null || true
}

_mu_zsh_configure_keymap() {
  bindkey -M mumode '^M' _mu_zsh_accept
  bindkey -M mumode '^J' _mu_zsh_accept
  bindkey -M mumode '^I' _mu_zsh_tab
  bindkey -M mumode $'\e[A' _mu_zsh_history_up
  bindkey -M mumode $'\eOA' _mu_zsh_history_up
  bindkey -M mumode $'\e[B' _mu_zsh_history_down
  bindkey -M mumode $'\eOB' _mu_zsh_history_down
  bindkey -M mumode '^?' _mu_zsh_backspace
  bindkey -M mumode '^H' _mu_zsh_backspace
  bindkey -M mumode '^C' _mu_zsh_interrupt
  bindkey -M mumode '^D' _mu_zsh_eof
}

if [[ -o zle ]]; then
  autoload -Uz add-zsh-hook 2>/dev/null || true
  autoload -Uz add-zle-hook-widget 2>/dev/null || true
  bindkey -N mumode main 2>/dev/null || true
  _mu_zsh_configure_keymap
  _mu_zsh_save_widget_bindings
  zle -N _mu_zsh_tab
  zle -N _mu_zsh_accept
  zle -N _mu_zsh_backspace
  zle -N _mu_zsh_interrupt
  zle -N _mu_zsh_eof
  zle -N _mu_zsh_history_up
  zle -N _mu_zsh_history_down
  zle -N _mu_zsh_shell_down
  zle -N _mu_zsh_line_init
  zle -N mu-zsh-mode
  zle -N mu-zsh-exit-mode
  add-zsh-hook zshexit _mu_zsh_cleanup
  add-zle-hook-widget line-init _mu_zsh_line_init 2>/dev/null || true
  _mu_zsh_capture_tty_state
  _mu_zsh_apply_prompt_tty
  bindkey '^I' _mu_zsh_tab
  bindkey '^D' _mu_zsh_eof
  bindkey $'\e[B' _mu_zsh_shell_down
  bindkey $'\eOB' _mu_zsh_shell_down
fi
