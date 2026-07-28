#!/usr/bin/env bash
# open-detached-window.sh — open a detached tmux session in a NEW Ghostty window.
#
# Invoked by the detached-dash "open in new window" menu item. The clicked entry
# is delivered as HERDR_PLUGIN_PAYLOAD_JSON: {name,label,agent,session,cwd}. The
# tmux session KEEPS running (this just attaches a viewer window to it); it is not
# restored into herdr and it still survives herdr restarts.
#
# No osascript (forbidden): a new Ghostty window is spawned with `open -na Ghostty`
# whose command attaches the tmux session.

set -u

LOG="${HERDR_OPEN_DETACHED_LOG:-$HOME/.herdr-open-detached.log}"
log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG" 2>/dev/null; }

PAYLOAD="${HERDR_PLUGIN_PAYLOAD_JSON:-}"
[[ -z "$PAYLOAD" ]] && { log "FATAL: no payload"; exit 2; }

if ! command -v jq >/dev/null 2>&1; then
  for j in /opt/homebrew/bin/jq /usr/local/bin/jq /usr/bin/jq; do
    [[ -x "$j" ]] && { export PATH="$(dirname "$j"):$PATH"; break; }
  done
fi
command -v jq >/dev/null 2>&1 || { log "FATAL: jq not found"; exit 3; }
if ! command -v tmux >/dev/null 2>&1; then
  for t in /opt/homebrew/bin/tmux /usr/local/bin/tmux /usr/bin/tmux; do
    [[ -x "$t" ]] && { export PATH="$(dirname "$t"):$PATH"; break; }
  done
fi
command -v tmux >/dev/null 2>&1 || { log "FATAL: tmux not found"; exit 3; }

HERDR=""
for cand in "${HERDR_BIN:-}" "${HERDR_BIN_PATH:-}" "$(command -v herdr 2>/dev/null)" \
  "$HOME/Projects/herdr/target/release/herdr" "$HOME/.local/bin/herdr"; do
  if [[ -n "$cand" && -x "$cand" ]]; then HERDR="$cand"; break; fi
done
toast() { [[ -n "$HERDR" ]] && "$HERDR" notification show "$1" ${2:+--body "$2"} >/dev/null 2>&1 || true; }

NAME="$(printf '%s' "$PAYLOAD" | jq -r '.name // empty')"
LABEL="$(printf '%s' "$PAYLOAD" | jq -r '.label // empty')"
[[ -z "$LABEL" ]] && LABEL="$NAME"
[[ -z "$NAME" ]] && { toast "Open failed" "Entry missing tmux name."; log "FATAL: no name"; exit 4; }

if ! tmux has-session -t "$NAME" 2>/dev/null; then
  toast "Open failed" "Detached session '$NAME' is no longer running."
  log "FATAL: session $NAME gone"
  exit 5
fi

TMUX_BIN="$(command -v tmux)"

# Prefer the Ghostty CLI when present; otherwise launch the app bundle. `-e`
# runs the given command in the new window instead of a login shell.
if command -v ghostty >/dev/null 2>&1; then
  ghostty -e "$TMUX_BIN" attach -t "$NAME" >/dev/null 2>&1 &
  disown 2>/dev/null || true
  log "opened via ghostty CLI: $NAME"
elif [[ -d "/Applications/Ghostty.app" || -d "$HOME/Applications/Ghostty.app" ]]; then
  open -na Ghostty --args -e "$TMUX_BIN" attach -t "$NAME"
  log "opened via open -na Ghostty: $NAME"
else
  toast "Open failed" "Ghostty not found."
  log "FATAL: Ghostty not installed"
  exit 6
fi

toast "Opened: $LABEL" "Attached tmux '$NAME' in a new Ghostty window."
exit 0
