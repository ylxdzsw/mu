# zsh integration for mu.
#
# Source this file from .zshrc to add a shell-native mu prompt mode:
# press Tab on an empty shell prompt to enter "mu>", Enter to submit one mu
# turn, Ctrl+C to clear the current mu prompt, and Ctrl+D or Backspace on an
# empty mu prompt to return to the normal shell prompt.

typeset -g MU_ZSH_MODE=${MU_ZSH_MODE:-shell}
typeset -g MU_ZSH_SESSION_ID=${MU_ZSH_SESSION_ID:-}
typeset -g MU_ZSH_SESSION_FILE=${MU_ZSH_SESSION_FILE:-${TMPDIR:-/tmp}/mu-zsh-${$}.session}
typeset -g MU_ZSH_BIN=${MU_ZSH_BIN:-mu}
typeset -g MU_ZSH_OUTPUT=${MU_ZSH_OUTPUT:-terminal}
typeset -g MU_ZSH_PROMPT=${MU_ZSH_PROMPT:-'mu> '}
typeset -g MU_ZSH_ORIGINAL_PROMPT=${MU_ZSH_ORIGINAL_PROMPT:-}
typeset -g MU_ZSH_ORIGINAL_RPROMPT=${MU_ZSH_ORIGINAL_RPROMPT:-}
typeset -g MU_ZSH_SAVED_BUFFER=${MU_ZSH_SAVED_BUFFER:-}
typeset -g MU_ZSH_SAVED_CURSOR=${MU_ZSH_SAVED_CURSOR:-0}
typeset -g MU_ZSH_SAVED_KEYMAP=${MU_ZSH_SAVED_KEYMAP:-main}
typeset -g MU_ZSH_ORIGINAL_TAB_WIDGET=${MU_ZSH_ORIGINAL_TAB_WIDGET:-}

_mu_zsh_widget_for_key() {
  local key=$1
  local binding
  binding=${${(z)$(bindkey "$key" 2>/dev/null)}[2]}
  [[ -n "$binding" ]] && print -r -- "$binding"
}

_mu_zsh_save_widget_bindings() {
  [[ -z "$MU_ZSH_ORIGINAL_TAB_WIDGET" ]] && MU_ZSH_ORIGINAL_TAB_WIDGET=$(_mu_zsh_widget_for_key '^I')

  [[ -z "$MU_ZSH_ORIGINAL_TAB_WIDGET" ]] && MU_ZSH_ORIGINAL_TAB_WIDGET=expand-or-complete
  return 0
}

_mu_zsh_call_original_widget() {
  local widget=$1
  if [[ -n "$widget" && "$widget" != _mu_zsh_tab ]]; then
    zle "$widget"
  fi
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

_mu_zsh_enter_mode() {
  [[ "$MU_ZSH_MODE" == mu ]] && return 0

  MU_ZSH_MODE=mu
  MU_ZSH_SAVED_KEYMAP=${KEYMAP:-main}
  MU_ZSH_SAVED_BUFFER=$BUFFER
  MU_ZSH_SAVED_CURSOR=$CURSOR
  MU_ZSH_ORIGINAL_PROMPT=$PROMPT
  MU_ZSH_ORIGINAL_RPROMPT=$RPROMPT
  PROMPT=$MU_ZSH_PROMPT
  RPROMPT=
  BUFFER=
  CURSOR=0
  zle -K mumode 2>/dev/null || true
}

_mu_zsh_exit_mode() {
  [[ "$MU_ZSH_MODE" == shell ]] && return 0

  MU_ZSH_MODE=shell
  zle -K "${MU_ZSH_SAVED_KEYMAP:-main}" 2>/dev/null || zle -K main 2>/dev/null || true
  PROMPT=$MU_ZSH_ORIGINAL_PROMPT
  RPROMPT=$MU_ZSH_ORIGINAL_RPROMPT
  BUFFER=$MU_ZSH_SAVED_BUFFER
  CURSOR=$MU_ZSH_SAVED_CURSOR
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

_mu_zsh_tab() {
  if [[ "$MU_ZSH_MODE" == mu ]]; then
    zle self-insert
    return
  fi

  if [[ -z "$BUFFER" ]]; then
    _mu_zsh_enter_mode
    zle reset-prompt
    return
  fi

  _mu_zsh_call_original_widget "$MU_ZSH_ORIGINAL_TAB_WIDGET"
}

_mu_zsh_backspace() {
  if [[ "$MU_ZSH_MODE" == mu && -z "$BUFFER" && "$CURSOR" -eq 0 ]]; then
    _mu_zsh_exit_mode
    zle reset-prompt
    return
  fi

  zle backward-delete-char
}

_mu_zsh_interrupt() {
  if [[ "$MU_ZSH_MODE" != mu ]]; then
    zle send-break
    return
  fi

  _mu_zsh_clear_prompt
  zle -I
  print
  zle reset-prompt
}

_mu_zsh_eof() {
  if [[ "$MU_ZSH_MODE" != mu ]]; then
    zle delete-char-or-list
    return
  fi

  _mu_zsh_exit_mode
  zle reset-prompt
}

_mu_zsh_accept() {
  if [[ "$MU_ZSH_MODE" != mu ]]; then
    zle accept-line
    return
  fi

  local prompt=$BUFFER
  if [[ -z "$prompt" ]]; then
    return
  fi

  _mu_zsh_clear_prompt
  zle -I
  print -r -- "$MU_ZSH_PROMPT$prompt"
  _mu_zsh_submit_prompt "$prompt"
  zle reset-prompt
}

mu-zsh-mode() {
  _mu_zsh_enter_mode
  zle reset-prompt
}

mu-zsh-exit-mode() {
  _mu_zsh_exit_mode
  zle reset-prompt
}

_mu_zsh_configure_keymap() {
  bindkey -M mumode '^M' _mu_zsh_accept
  bindkey -M mumode '^J' _mu_zsh_accept
  bindkey -M mumode '^I' self-insert
  bindkey -M mumode '^?' _mu_zsh_backspace
  bindkey -M mumode '^H' _mu_zsh_backspace
  bindkey -M mumode '^C' _mu_zsh_interrupt
  bindkey -M mumode '^D' _mu_zsh_eof
}

if [[ -o zle ]]; then
  bindkey -N mumode main 2>/dev/null || true
  _mu_zsh_configure_keymap
  _mu_zsh_save_widget_bindings
  zle -N _mu_zsh_tab
  zle -N _mu_zsh_accept
  zle -N _mu_zsh_backspace
  zle -N _mu_zsh_interrupt
  zle -N _mu_zsh_eof
  zle -N mu-zsh-mode
  zle -N mu-zsh-exit-mode
  bindkey '^I' _mu_zsh_tab
fi
