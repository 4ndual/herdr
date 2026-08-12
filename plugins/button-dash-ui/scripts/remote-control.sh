#!/usr/bin/env bash
# remote-control.sh — enable Claude Code Remote Control on the CURRENT pane's
# LIVE conversation so it can be continued from the phone / claude.ai.
#
# The "Remote" button (⌃⌥R) runs this. Claude Code exposes
# Remote Control as the in-session slash command `/remote-control` ("take this
# session with you and pick up right where you left off on any device — open the
# Code tab in the Claude mobile app, or visit claude.ai/code"). So for a claude
# pane we simply TYPE that slash command into the running agent (via
# `herdr pane send-text` + `herdr pane send-keys enter`) — we do NOT `pane run`
# (that would spawn a shell command, not talk to the REPL).
#
# Scope: Remote Control continues the conversation on Anthropic's teleport
# service, so it works only when *claude itself* is the orchestrator (it can
# still call codex/pi/jcode locally as a subagent). Every other agent pane gets
# a clear toast instead.
#
# Usage:  remote-control.sh
#   Reads the source pane from the inherited HERDR_PANE_ID (button path) or the
#   focused pane (tab-context path); resolves agent/session/cwd live via
#   `herdr pane get`. Nothing is hardcoded.

set -u

LOG="${HERDR_REMOTE_LOG:-$HOME/.herdr-remote.log}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=dash-common.sh
. "$SCRIPT_DIR/dash-common.sh" || { echo "remote: dash-common.sh missing" >&2; exit 3; }

if [[ -z "$HERDR" ]]; then
  log "FATAL: no herdr binary found (HERDR_BIN/HERDR_BIN_PATH unset and none on PATH)"
  desktop_notify "Remote failed" "no herdr binary found"
  exit 3
fi

command -v jq >/dev/null 2>&1 || { toast "Remote failed" "jq is not installed"; exit 3; }

# ---------------------------------------------------------------------------
# 1. Resolve the SOURCE pane (button: HERDR_PANE_ID; tab context: focused pane).
# ---------------------------------------------------------------------------
if ! PANE="$(current_pane)"; then
  toast "Remote failed" "could not resolve the current pane"
  exit 4
fi
log "source pane: $PANE"

# ---------------------------------------------------------------------------
# 2. Read the source pane's agent, session ref and cwd.
# ---------------------------------------------------------------------------
PANE_JSON="$(hd pane get "$PANE" 2>/dev/null)"
AGENT="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.agent // empty')"
VALUE="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.agent_session.value // empty')"
CWD="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.cwd // .result.pane.foreground_cwd // empty')"
[[ -z "$CWD" ]] && CWD="$HOME"
log "agent=$AGENT value=$VALUE cwd=$CWD"

# ---------------------------------------------------------------------------
# 3. Gate on the agent kind. Remote works only for a claude ORCHESTRATOR.
# ---------------------------------------------------------------------------
case "$AGENT" in
  # "cx" is a cosmetic display label for Claude Code driven through a gateway,
  # never a separate runtime — legacy panes can still carry it.
  claude | cx) ;; # the only orchestrator the phone can bridge into
  "")
    toast "Remote: no agent here" \
      "This pane has no running agent to make remote."
    exit 0
    ;;
  *)
    # Deliberately ONE message for codex/pi/jcode and everything else: Remote
    # Control is a Claude Code feature, and inventing per-agent prose implied
    # the other agents had some other remote path. They do not.
    toast "Remote is claude only" \
      "Remote Control is a Claude Code feature — this pane runs '$AGENT'. A claude orchestrator can still call $AGENT locally."
    exit 0
    ;;
esac

# ---------------------------------------------------------------------------
# 4. claude orchestrator: type `/remote-control` into the live REPL.
#    send-text writes the literal command; a brief settle lets the client render
#    the slash-command line; send-keys enter submits it.
# ---------------------------------------------------------------------------
log "enabling remote control on pane $PANE (agent=claude, session=${VALUE:-unknown})"
hd pane send-text "$PANE" "/remote-control" >>"$LOG" 2>&1
sleep 0.4
hd pane send-keys "$PANE" enter >>"$LOG" 2>&1
log "REMOTE OK: /remote-control submitted to pane $PANE"
toast "Remote Control enabling…" \
  "Continue this session from the Claude mobile app (Code tab) or claude.ai/code."
exit 0
