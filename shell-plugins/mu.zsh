# mu zsh plugin
# Install: eval "$(mu init zsh)"

typeset -g MU_SESSION_ID
typeset -g MU_SESSION_FILE="${XDG_RUNTIME_DIR:-/tmp}/mu/session.$$"

_mu_config_path() {
  print -r -- "${MU_CONFIG_DIR:-$HOME/.mu}/config.jsonc"
}

_mu_config_agent_key() {
  local cfg="$(_mu_config_path)"
  if [[ -f "$cfg" ]]; then
    local key
    key=$(grep -o '"agent_mode_key"[[:space:]]*:[[:space:]]*"[^"]*"' "$cfg" 2>/dev/null | head -1 | sed 's/.*: *"\(.*\)"/\1/')
    key=${key//\\\\/\\}
    [[ -n "$key" ]] && print -r -- "$key" && return
  fi
  print -r -- $'\eM'
}

_mu_run_turn() {
  local prompt="$1"
  if [[ -n "$MU_SESSION_ID" ]]; then
    command mu -s "$MU_SESSION_ID" <<< "$prompt"
  else
    export MU_SESSION_FILE
    mkdir -p "${MU_SESSION_FILE:h}"
    command mu <<< "$prompt"
    if [[ -f "$MU_SESSION_FILE" ]]; then
      export MU_SESSION_ID="$(<"$MU_SESSION_FILE")"
    fi
  fi
}

_mu_widget() {
  if [[ -z "$BUFFER" ]]; then
    zle -I
    if [[ -n "$MU_SESSION_ID" ]]; then
      command mu-cli -s "$MU_SESSION_ID"
    else
      command mu-cli
    fi
    zle reset-prompt
    return 0
  fi

  local prompt="$BUFFER"
  print -s -- "$prompt"
  BUFFER=
  zle -I
  _mu_run_turn "$prompt"
  zle reset-prompt
}

_mu_magic_space() {
  if [[ "$BUFFER" == "mu" ]]; then
    BUFFER=""
    _mu_widget
  else
    zle self-insert
  fi
}

mu-new() {
  unset MU_SESSION_ID
  rm -f "$MU_SESSION_FILE"
  print -u2 "new session - next turn creates a fresh session"
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
  bindkey -M main "$key" _mu_widget
  bindkey -M viins "$key" _mu_widget 2>/dev/null
}

_mu_maybe_magic_space() {
  local cfg="$(_mu_config_path)"
  if [[ -f "$cfg" ]] && grep -q '"magic_space"[[:space:]]*:[[:space:]]*true' "$cfg" 2>/dev/null; then
    bindkey -M main ' ' _mu_magic_space
    bindkey -M viins ' ' _mu_magic_space 2>/dev/null
  fi
}

zle -N _mu_widget
zle -N _mu_magic_space

_mu_bind_entry_key
_mu_maybe_magic_space
