#!/usr/bin/env bash
# auto-restart-hr.sh — one-shot watcher: when EVERY agent pane in the live hr
# instance is idle/done (nothing mid-turn), stage the approved binary and
# restart hr via the standard restart-herdr.sh flow.
#
# Which binary gets staged is read AT TRIGGER TIME from the sibling file
# hr-restart-source (a single line: absolute path to a release binary).
#   - default: the task/wave1-no-images build (all wave-1 changes minus the
#     image renderer) so a restart never ships an unproven image path;
#   - once the image renderer is proven end-to-end, point the file at the
#     full-tip build instead — no script edit needed.
#
# Safety: staging file is created NEXT TO the destination (same volume) and
# atomic-mv'd over it; a cross-volume mv would copy-overwrite the running
# inode and SIGKILL the live server. Restart itself is delegated to
# restart-herdr.sh (graceful kill → relaunch → session restore; the new
# auto-continue feature nudges panes whose turn was interrupted).
set -u

DIR="$(cd "$(dirname "$0")" && pwd)"
SRC_FILE="$DIR/hr-restart-source"
LOG="$HOME/.herdr-hr-autorestart.log"
SOCK="$HOME/.herdr-hr/config/herdr/sessions/hr/herdr.sock"
DEST="$HOME/Projects/herdr/target/release/herdr"
RESTART="$HOME/Projects/herdr/plugins/button-dash-ui/scripts/restart-herdr.sh"
POLL=30           # seconds between checks
NEED_CALM=2       # consecutive all-idle polls required before acting

log() { printf '%s %s\n' "$(date '+%F %H:%M:%S')" "$*" >>"$LOG"; }

unset HERDR_ENV HERDR_SOCKET_PATH HERDR_CLIENT_SOCKET_PATH HERDR_SESSION \
      HERDR_WORKSPACE_ID HERDR_TAB_ID HERDR_PANE_ID
export HERDR_SOCKET_PATH="$SOCK"

log "watcher started (pid $$), source file: $SRC_FILE"
calm=0
while :; do
  if [ ! -S "$SOCK" ]; then
    log "hr socket missing; waiting"
    calm=0; sleep "$POLL"; continue
  fi
  busy=$(herdr pane list 2>/dev/null \
    | jq -r '[.result.panes[] | select(.agent_status == "working")] | length' 2>/dev/null)
  if [ -z "$busy" ]; then
    log "pane list failed; waiting"
    calm=0; sleep "$POLL"; continue
  fi
  if [ -z "${polled_ok:-}" ]; then polled_ok=1; log "first poll ok ($busy working)"; fi
  if [ "$busy" -eq 0 ]; then
    calm=$((calm + 1))
    log "all agents idle/done (calm $calm/$NEED_CALM)"
  else
    [ "$calm" -ne 0 ] && log "activity resumed ($busy working); calm reset"
    calm=0
  fi
  if [ "$calm" -ge "$NEED_CALM" ]; then
    SRC="$(head -n1 "$SRC_FILE" 2>/dev/null)"
    if [ -z "$SRC" ] || [ ! -x "$SRC" ]; then
      log "FATAL: source binary missing/not executable: '$SRC'"; exit 2
    fi
    VER="$("$SRC" --version 2>/dev/null | head -1)"
    if [ -z "$VER" ]; then
      log "FATAL: source binary failed --version: $SRC"; exit 2
    fi
    log "staging $SRC ($VER) -> $DEST"
    if ! cp "$SRC" "$DEST.staged"; then log "FATAL: cp to staging failed"; exit 2; fi
    if ! mv -f "$DEST.staged" "$DEST"; then log "FATAL: atomic mv failed"; exit 2; fi
    log "binary staged; invoking restart-herdr.sh hr"
    "$RESTART" hr
    log "restart dispatched; watcher exiting"
    exit 0
  fi
  sleep "$POLL"
done
