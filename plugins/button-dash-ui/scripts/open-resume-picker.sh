#!/usr/bin/env bash
# open-resume-picker.sh — open the resume-search picker in a NEW TAB whose root
# pane runs the picker (mirrors resume-in-cx.sh's tab-create pattern). If the
# CURRENT pane is a plain terminal (no agent), run it there instead so the
# picker can exec the resumed session into that same pane.
#
# Wired to the button-dash "Resume search" button and (proposed) the ⌃⌥R hotkey.
set -u

LOG="${HERDR_RESUME_PICKER_LOG:-$HOME/.herdr-resume-picker.log}"
log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG" 2>/dev/null; }

# Locate the picker wrapper relative to this script (button-dash-ui/scripts ->
# repo root -> plugins/resume-picker/bin).
HERE="$(cd "$(dirname "$0")" && pwd)"
PICKER="$(cd "$HERE/../../resume-picker/bin" 2>/dev/null && pwd)/resume-picker.sh"
if [[ ! -x "$PICKER" ]]; then
  log "FATAL: picker wrapper not found/executable at $PICKER"
  exit 3
fi

# Resolve the herdr client binary + jq (same discovery as resume-in-cx.sh).
HERDR=""
for cand in "${HERDR_BIN:-}" "$(command -v herdr 2>/dev/null)" \
  "$HOME/Projects/herdr/target/release/herdr" "$HOME/.local/bin/herdr"; do
  if [[ -n "$cand" && -x "$cand" ]]; then HERDR="$cand"; break; fi
done
[[ -n "$HERDR" ]] || { log "FATAL: no herdr binary"; exit 3; }
if ! command -v jq >/dev/null 2>&1; then
  for j in /opt/homebrew/bin/jq /usr/local/bin/jq /usr/bin/jq; do
    [[ -x "$j" ]] && { export PATH="$(dirname "$j"):$PATH"; break; }
  done
fi
hd() { "$HERDR" "$@"; }

# Resolve the source pane (button path inherits HERDR_PANE_ID; hotkey path uses focus).
PANE="${HERDR_PANE_ID:-}"
if [[ -z "$PANE" ]]; then
  PANE="$(hd pane list 2>/dev/null | jq -r '.result.panes[] | select(.focused) | .pane_id' | head -1)"
fi
[[ -n "$PANE" && "$PANE" != "null" ]] || { log "FATAL: no source pane"; exit 4; }

PANE_JSON="$(hd pane get "$PANE" 2>/dev/null)"
AGENT="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.agent // empty')"
CWD="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.cwd // .result.pane.foreground_cwd // empty')"
WS="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.workspace_id // empty')"
[[ -z "$CWD" ]] && CWD="$HOME"
[[ -z "$WS" ]] && WS="${HERDR_WORKSPACE_ID:-}"

# Plain terminal pane -> run in place; agent pane -> new tab.
if [[ -z "$AGENT" ]]; then
  log "plain pane $PANE; running picker in place"
  hd pane run "$PANE" "exec $PICKER" >>"$LOG" 2>&1
  exit 0
fi

CREATE_JSON="$(hd tab create --workspace "$WS" --cwd "$CWD" --label "resume" --focus 2>/dev/null)"
ROOT_PANE="$(printf '%s' "$CREATE_JSON" | jq -r '.result.root_pane.pane_id // empty')"
if [[ -z "$ROOT_PANE" || "$ROOT_PANE" == "null" ]]; then
  log "FATAL: could not create tab (resp: $CREATE_JSON)"
  exit 8
fi
sleep 0.4
hd pane run "$ROOT_PANE" "exec $PICKER" >>"$LOG" 2>&1
log "RESUME-PICKER OK -> $ROOT_PANE"
exit 0
