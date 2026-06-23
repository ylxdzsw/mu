# mu zsh plugin — agent mode for interactive shells
# Install: eval "$(mu init zsh)"

autoload -Uz add-zsh-hook
autoload -Uz add-zle-hook-widget 2>/dev/null || true

typeset -g _MU_AGENT_MODE=0
typeset -g _MU_SAVED_PROMPT
typeset -g _MU_SAVED_RPROMPT
typeset -ga _MU_SAVED_HIGHLIGHTERS
typeset -g _MU_PROMPT
typeset -g _MU_INSERT_KEYMAP=main
typeset -g MU_SESSION_ID
typeset -g MU_SESSION_FILE="${XDG_RUNTIME_DIR:-/tmp}/mu/session.$$"

_mu_config_path() {
  print -r -- "${MU_CONFIG_DIR:-${XDG_CONFIG_HOME:-$HOME/.config}/mu}/config.jsonc"
}

_mu_config_agent_key() {
  local cfg="$(_mu_config_path)"
  if [[ -f "$cfg" ]]; then
    local key
    key=$(grep -o '"agent_mode_key"[[:space:]]*:[[:space:]]*"[^"]*"' "$cfg" 2>/dev/null | head -1 | sed 's/.*: *"\(.*\)"/\1/')
    # The JSON value escapes a backslash as "\\"; collapse it back to a single
    # backslash so e.g. "\\eM" becomes "\eM", which bindkey reads as ESC-M.
    key=${key//\\\\/\\}
    [[ -n "$key" ]] && print -r -- "$key" && return
  fi
  print -r -- $'\eM'
}

_mu_define_keymap() {
  local base_keymap="${1:-main}"
  bindkey -D mu >/dev/null 2>&1
  bindkey -N mu "$base_keymap" 2>/dev/null || bindkey -N mu main
  bindkey -M mu '^M' _mu_accept_line
  bindkey -M mu '^?' _mu_backspace
  bindkey -M mu '^[' _mu_exit_agent_mode
  bindkey -M mu '^J' _mu_insert_newline
  bindkey -M mu '^[[13;2u' _mu_insert_newline 2>/dev/null
}

_mu_enter_agent_mode() {
  [[ $_MU_AGENT_MODE -eq 1 ]] && return 0
  _MU_AGENT_MODE=1
  _MU_INSERT_KEYMAP="${KEYMAP:-main}"
  _mu_define_keymap "$_MU_INSERT_KEYMAP"

  _MU_SAVED_PROMPT="$PROMPT"
  _MU_SAVED_RPROMPT="$RPROMPT"
  PROMPT=$'%F{cyan}μ%f '
  RPROMPT=''

  if (( $+ZSH_HIGHLIGHT_HIGHLIGHTERS )); then
    _MU_SAVED_HIGHLIGHTERS=("${ZSH_HIGHLIGHT_HIGHLIGHTERS[@]}")
    ZSH_HIGHLIGHT_HIGHLIGHTERS=()
  fi
  if (( $+functions[autosuggest-disable] )); then
    autosuggest-disable
  fi

  zle -K mu
  zle reset-prompt
}

_mu_exit_agent_mode() {
  [[ $_MU_AGENT_MODE -eq 0 ]] && return 0
  _MU_AGENT_MODE=0
  PROMPT="$_MU_SAVED_PROMPT"
  RPROMPT="$_MU_SAVED_RPROMPT"

  if (( ${#_MU_SAVED_HIGHLIGHTERS[@]} )); then
    ZSH_HIGHLIGHT_HIGHLIGHTERS=("${_MU_SAVED_HIGHLIGHTERS[@]}")
  fi
  if (( $+functions[autosuggest-enable] )); then
    autosuggest-enable
  fi

  zle -K "${_MU_INSERT_KEYMAP:-main}" 2>/dev/null || zle -K main
  zle reset-prompt
}

_mu_insert_newline() {
  LBUFFER+=$'\n'
  CURSOR=${#LBUFFER}
}

_mu_backspace() {
  if [[ -z "$BUFFER" ]]; then
    _mu_exit_agent_mode
    return 0
  fi
  zle backward-delete-char
}

_mu_accept_line() {
  _MU_PROMPT="$BUFFER"
  [[ -z "$_MU_PROMPT" ]] && return 0
  print -s -- "$_MU_PROMPT"
  BUFFER="_mu_send"
  CURSOR=${#BUFFER}
  zle accept-line
}

_mu_send() {
  if [[ -n "$MU_SESSION_ID" ]]; then
    command mu --session "$MU_SESSION_ID" <<< "$_MU_PROMPT"
  else
    export MU_SESSION_FILE
    mkdir -p "${MU_SESSION_FILE:h}"
    command mu <<< "$_MU_PROMPT"
    if [[ -f "$MU_SESSION_FILE" ]]; then
      export MU_SESSION_ID="$(<"$MU_SESSION_FILE")"
    fi
  fi
}

_mu_precmd_rearm() {
  if [[ $_MU_AGENT_MODE -eq 1 ]]; then
    PROMPT=$'%F{cyan}μ%f '
    RPROMPT=''
  fi
}

_mu_line_init() {
  if [[ $_MU_AGENT_MODE -eq 1 ]]; then
    _mu_define_keymap "$_MU_INSERT_KEYMAP"
    zle -K mu
  fi
}

_mu_magic_space() {
  if [[ "$BUFFER" == "mu" ]]; then
    BUFFER=""
    _mu_enter_agent_mode
  else
    zle self-insert
  fi
}

mu-new() {
  unset MU_SESSION_ID
  rm -f "$MU_SESSION_FILE"
  print -u2 "new session — next turn creates a fresh session"
}

mu-attach() {
  if [[ -z "$1" ]]; then
    print -u2 "usage: mu-attach <session-id>"
    return 1
  fi
  export MU_SESSION_ID="$1"
  print -u2 "attached to session $MU_SESSION_ID"
}

mu-sessions() {
  command mu session list
}

mu-compact() {
  if [[ -z "$MU_SESSION_ID" ]]; then
    print -u2 "no active session"
    return 1
  fi
  command mu compact --session "$MU_SESSION_ID"
}

_mu_bind_entry_key() {
  local key="$(_mu_config_agent_key)"
  bindkey -M main "$key" _mu_enter_agent_mode
  bindkey -M viins "$key" _mu_enter_agent_mode 2>/dev/null
}

_mu_maybe_magic_space() {
  local cfg="$(_mu_config_path)"
  if [[ -f "$cfg" ]] && grep -q '"magic_space"[[:space:]]*:[[:space:]]*true' "$cfg" 2>/dev/null; then
    bindkey -M main ' ' _mu_magic_space
    bindkey -M viins ' ' _mu_magic_space 2>/dev/null
  fi
}

# Keep the internal `_mu_send` dispatch out of shell history regardless of the
# user's HIST_IGNORE_SPACE setting. The user's actual prompt is recorded
# separately via `print -s` in _mu_accept_line.
_mu_addhistory() {
  [[ "${1%%$'\n'}" == "_mu_send" ]] && return 1
  return 0
}

zle -N _mu_enter_agent_mode
zle -N _mu_exit_agent_mode
zle -N _mu_accept_line
zle -N _mu_backspace
zle -N _mu_insert_newline
zle -N _mu_magic_space

_mu_bind_entry_key
_mu_maybe_magic_space
add-zsh-hook precmd _mu_precmd_rearm
add-zsh-hook zshaddhistory _mu_addhistory
add-zle-hook-widget line-init _mu_line_init 2>/dev/null || true
