#!/usr/bin/env bash
# remote-control.sh — enable Claude Code Remote Control on the CURRENT pane's
# LIVE conversation so it can be continued from the phone / claude.ai.
#
# The "Remote" button and the ⌃⌥R Karabiner hotkey run this. Claude Code exposes
# Remote Control as the in-session slash command `/remote-control` ("take this
# session with you and pick up right where you left off on any device — open the
# Code tab in the Claude mobile app, or visit claude.ai/code"). So for a claude
# pane we simply TYPE that slash command into the running agent (via
# `herdr pane send-text` + `herdr pane send-keys enter`) — we do NOT `pane run`
# (that would spawn a shell command, not talk to the REPL).
#
# Scope (per the codex/cx constraint): Remote Control continues the conversation
# on Anthropic's teleport service; a codex- or cx-ORCHESTRATED session has no
# bridge from the phone back into this machine, so those panes get a clear toast
# instead. Remote works only when *claude itself* is the orchestrator (it can
# still call codex/gpt locally as a subagent). cx panes are detected by their
# transcript living under the CCR profile dir (herdr labels both cx and daily
# claude as agent="claude"), mirroring cx-resume's own dispatch.
#
# Usage:  remote-control.sh
#   Reads the source pane from the inherited HERDR_PANE_ID (button path) or the
#   focused pane (Karabiner path); resolves agent/session/cwd live via
#   `herdr pane get`. Nothing is hardcoded.

set -u

LOG="${HERDR_REMOTE_LOG:-$HOME/.herdr-remote.log}"
log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG" 2>/dev/null; }

# ---------------------------------------------------------------------------
# 0. Resolve the herdr client binary (prefer the fork build; never assume PATH).
# ---------------------------------------------------------------------------
HERDR=""
for cand in "${HERDR_BIN:-}" "$(command -v herdr 2>/dev/null)" \
  "$HOME/Projects/herdr/target/release/herdr" "$HOME/.local/bin/herdr"; do
  if [[ -n "$cand" && -x "$cand" ]]; then
    HERDR="$cand"
    break
  fi
done
if [[ -z "$HERDR" ]]; then
  log "FATAL: no herdr binary found"
  exit 3
fi

if ! command -v jq >/dev/null 2>&1; then
  for j in /opt/homebrew/bin/jq /usr/local/bin/jq /usr/bin/jq; do
    [[ -x "$j" ]] && { export PATH="$(dirname "$j"):$PATH"; break; }
  done
fi
command -v jq >/dev/null 2>&1 || { log "FATAL: jq not found"; exit 3; }

hd() { "$HERDR" "$@"; }

# Surface a short message to the user. Prefer a herdr toast; always log too.
toast() {
  local title="$1" body="${2:-}"
  log "TOAST: $title — $body"
  hd notification show "$title" ${body:+--body "$body"} >>"$LOG" 2>&1 || true
}

# ---------------------------------------------------------------------------
# 1. Resolve the SOURCE pane (button: HERDR_PANE_ID; Karabiner: focused pane).
# ---------------------------------------------------------------------------
PANE="${HERDR_PANE_ID:-}"
if [[ -z "$PANE" ]]; then
  PANE="$(hd pane list 2>/dev/null | jq -r '.result.panes[] | select(.focused) | .pane_id' | head -1)"
fi
if [[ -z "$PANE" || "$PANE" == "null" ]]; then
  log "FATAL: could not resolve source pane"
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
  claude) ;; # fall through to the cx-vs-daily discrimination below
  codex)
    toast "Remote not available" \
      "codex as orchestrator can't be continued from the phone — remote works when claude is the orchestrator (it can call codex locally)."
    exit 0
    ;;
  pi | omp)
    toast "Remote not available" \
      "Remote Control is a Claude Code feature — this pane runs '$AGENT'."
    exit 0
    ;;
  "")
    toast "Remote: no agent here" \
      "This pane has no running agent to make remote."
    exit 0
    ;;
  *)
    toast "Remote not available" \
      "Remote Control needs a claude orchestrator — this pane runs '$AGENT'."
    exit 0
    ;;
esac

# ---------------------------------------------------------------------------
# 3b. claude pane: distinguish a cx (GPT-via-CCR) session from a daily claude.
#     herdr labels both as agent="claude"; cx's transcript lives ONLY under the
#     CCR profile dir (same signal cx-resume dispatches on). A cx orchestrator
#     can't be bridged from the phone, so it gets the toast, not /remote-control.
# ---------------------------------------------------------------------------
CCR_DIR="${CCR_DIR:-$HOME/.claude-code-router/profiles/default-claude-code/claude}"
if [[ -n "$VALUE" ]]; then
  ENC="$(printf '%s' "$CWD" | LC_ALL=C sed 's/[^A-Za-z0-9]/-/g')"
  CX_JSONL="$CCR_DIR/projects/$ENC/$VALUE.jsonl"
  DAILY_JSONL="$HOME/.claude/projects/$ENC/$VALUE.jsonl"
  if [[ -f "$CX_JSONL" && ! -f "$DAILY_JSONL" ]]; then
    log "pane $PANE is a cx session ($CX_JSONL) — remote not offered"
    toast "Remote not available for cx" \
      "This is a cx (GPT-via-CCR) session — the phone can't bridge into a cx/codex orchestrator. Remote works when claude is the orchestrator."
    exit 0
  fi
fi

# ---------------------------------------------------------------------------
# 4. Daily claude orchestrator: type `/remote-control` into the live REPL.
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
