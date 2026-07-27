#!/usr/bin/env bash
# Hand one failed repair to an agent, VISIBLY.
#
#   escalate-to-cx.sh <fixer-id> <prompt>
#
# Opens a new herdr tab and runs `cx` in it with the prompt pre-typed, so the
# user watches an agent take over instead of being dropped at a shell to work
# out what to type. Referenced from the `escalate` field of a fixers manifest
# entry, so the manifest stays pure data and the mechanism is shared.
#
# Exits 0 only when a tab was created and the agent was started; the runner
# reports a non-zero exit as "escalation also failed" rather than pretending an
# agent is on it.

set -uo pipefail

ID="${1:-unknown}"
PROMPT="${2:-a doctor-dash fix failed; please diagnose and repair it.}"
LOG="${HERDR_DOCTOR_ESCALATE_LOG:-$HOME/.herdr-doctor-escalate.log}"

log() { printf '%s escalate[%s] %s\n' "$(date +%FT%T)" "$ID" "$*" >>"$LOG"; }

# Resolve the herdr client the same way button-dash's scripts do: prefer the
# binary the running server was launched from, never assume PATH.
HERDR_BIN=""
for candidate in "${HERDR_BIN_PATH:-}" "$(command -v herdr 2>/dev/null)" \
  "$HOME/Projects/herdr/target/release/herdr" "$HOME/.local/bin/herdr"; do
  if [ -n "$candidate" ] && [ -x "$candidate" ]; then
    HERDR_BIN="$candidate"
    break
  fi
done
if [ -z "$HERDR_BIN" ]; then
  log "FATAL: no herdr binary found"
  exit 8
fi

CX_BIN="${HERDR_DOCTOR_CX_BIN:-$HOME/bin/cx}"
if [ ! -x "$CX_BIN" ]; then
  log "FATAL: cx not executable at $CX_BIN"
  exit 9
fi

CWD="${HERDR_FOCUS_CWD:-$HOME}"
CREATE_JSON="$("$HERDR_BIN" tab create --cwd "$CWD" --label "doctor: $ID" --focus 2>&1)"
ROOT_PANE="$(printf '%s' "$CREATE_JSON" | jq -r '.result.root_pane.pane_id // empty' 2>/dev/null)"
if [ -z "$ROOT_PANE" ] || [ "$ROOT_PANE" = "null" ]; then
  log "FATAL: could not create tab: $CREATE_JSON"
  exit 10
fi
log "tab root pane $ROOT_PANE"

# Let the login shell reach its prompt before typing into it.
sleep 0.4

# `exec` so closing the agent closes the pane, matching a normal agent pane.
"$HERDR_BIN" pane run "$ROOT_PANE" "exec $(printf '%q' "$CX_BIN") $(printf '%q' "$PROMPT")" \
  >>"$LOG" 2>&1 || {
  log "FATAL: pane run failed"
  exit 11
}
log "OK"
exit 0
