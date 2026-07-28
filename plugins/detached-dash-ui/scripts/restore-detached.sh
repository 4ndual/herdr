#!/usr/bin/env bash
# restore-detached.sh — TRUE restore of a detached tmux session back INTO herdr.
#
# Invoked by the detached-dash "restore into herdr" menu item. The clicked entry
# is delivered as HERDR_PLUGIN_PAYLOAD_JSON: {name,label,agent,session,cwd}.
#
# It kills the external tmux wrapper (freeing the agent session id) and resumes
# the SAME conversation as a NORMAL herdr pane — cx-resume in a fresh tab's root
# pane, NOT nested inside tmux (a nested tmux would break on herdr restarts). Kill
# happens BEFORE resume so exactly one live process owns the session id.

set -u

LOG="${HERDR_RESTORE_DETACHED_LOG:-$HOME/.herdr-restore-detached.log}"
log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG" 2>/dev/null; }

PAYLOAD="${HERDR_PLUGIN_PAYLOAD_JSON:-}"
if [[ -z "$PAYLOAD" ]]; then
  log "FATAL: no HERDR_PLUGIN_PAYLOAD_JSON"
  exit 2
fi

# ---- resolve tools -----------------------------------------------------------
HERDR=""
for cand in "${HERDR_BIN:-}" "${HERDR_BIN_PATH:-}" "$(command -v herdr 2>/dev/null)" \
  "$HOME/Projects/herdr/target/release/herdr" "$HOME/.local/bin/herdr"; do
  if [[ -n "$cand" && -x "$cand" ]]; then HERDR="$cand"; break; fi
done
[[ -z "$HERDR" ]] && { log "FATAL: no herdr binary"; exit 3; }

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

hd() { "$HERDR" "$@"; }
toast() { hd notification show "$1" ${2:+--body "$2"} >/dev/null 2>&1 || true; }

# ---- parse payload -----------------------------------------------------------
NAME="$(printf '%s' "$PAYLOAD" | jq -r '.name // empty')"
LABEL="$(printf '%s' "$PAYLOAD" | jq -r '.label // empty')"
AGENT="$(printf '%s' "$PAYLOAD" | jq -r '.agent // empty')"
SESSION="$(printf '%s' "$PAYLOAD" | jq -r '.session // empty')"
CWD="$(printf '%s' "$PAYLOAD" | jq -r '.cwd // empty')"
[[ -z "$CWD" ]] && CWD="$HOME"
[[ -z "$LABEL" ]] && LABEL="$NAME"
log "restore name=$NAME agent=$AGENT session=$SESSION cwd=$CWD"

if [[ -z "$NAME" || -z "$SESSION" || -z "$AGENT" ]]; then
  toast "Restore failed" "Detached entry is missing name/session/agent."
  log "FATAL: incomplete payload: $PAYLOAD"
  exit 4
fi

# ---- build the per-agent resume command (no fork; same session) --------------
RESUME_CMD=""
case "$AGENT" in
  claude) RESUME_CMD="cx-resume $(printf '%q' "$SESSION")" ;;
  codex)  RESUME_CMD="codex resume $(printf '%q' "$SESSION")" ;;
  pi|omp) RESUME_CMD="omp --resume=$(printf '%q' "$SESSION")" ;;
  *) toast "Restore failed" "Agent '$AGENT' not supported."; log "FATAL: agent $AGENT"; exit 5 ;;
esac

# ---- kill the tmux wrapper FIRST so the session id is free -------------------
if tmux has-session -t "$NAME" 2>/dev/null; then
  tmux kill-session -t "$NAME" 2>/dev/null && log "killed tmux session $NAME"
  # brief settle so the agent process exits + flushes before we resume its id
  sleep 0.6
else
  log "WARN: tmux session $NAME already gone (restoring from transcript anyway)"
fi

# ---- resume as a normal herdr pane in a fresh tab ----------------------------
WS="${HERDR_WORKSPACE_ID:-}"
if [[ -z "$WS" ]]; then
  WS="$(hd workspace list 2>/dev/null | jq -r '.result.workspaces[] | select(.focused) | .workspace_id' | head -1)"
fi
[[ -z "$WS" ]] && WS="$(hd workspace list 2>/dev/null | jq -r '.result.workspaces[0].workspace_id // empty')"

CREATE_JSON="$(hd tab create --workspace "$WS" --cwd "$CWD" --label "$LABEL" --focus 2>/dev/null)"
ROOT_PANE="$(printf '%s' "$CREATE_JSON" | jq -r '.result.root_pane.pane_id // empty')"
if [[ -z "$ROOT_PANE" || "$ROOT_PANE" == "null" ]]; then
  toast "Restore failed" "Could not create a herdr tab."
  log "FATAL: tab create failed: $CREATE_JSON"
  exit 6
fi

sleep 0.4
hd pane run "$ROOT_PANE" "exec $RESUME_CMD" >>"$LOG" 2>&1
toast "Restored: $LABEL" "Back in herdr (tmux wrapper removed)."
log "RESTORE OK: $AGENT $SESSION -> pane $ROOT_PANE"
exit 0
