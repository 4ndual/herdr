#!/usr/bin/env bash
# detach-conversation.sh — move the CURRENT pane's agent conversation OUT of herdr
# and into a standalone EXTERNAL tmux session, so a herdr restart (⌃⌥X / crash /
# update) can never kill it. This is for "work that must never stop".
#
# The "Detach" button and the ⌃⌥D Karabiner hotkey run this. herdr restarts only
# reap herdr-managed process groups; a tmux session on the user's default tmux
# server is not one of them, so the conversation survives every herdr restart.
#
# Mechanics (exactly ONE live process per session at the end — no fork, no dup):
#   1. Read the source pane's agent + session id + cwd live from `herdr pane get`.
#   2. CLOSE the herdr pane first — this terminates the in-pane agent process, so
#      the session id is free (Claude/cx refuse a second concurrent resume of the
#      same id). The transcript on disk is the source of truth for the resume.
#   3. Start `tmux new-session -d -s herdr-detached-<slug>` whose command RESUMES
#      the SAME session (cx-resume <id> — NO --fork-session) from the original cwd.
#
# Nothing is hardcoded: pane, agent, session id and cwd come from `herdr pane get`;
# the herdr binary is detected from the env (button path) or PATH (Karabiner path).
#
# Usage:  detach-conversation.sh
#   Reads the source pane from the inherited HERDR_PANE_ID (button path) or the
#   focused pane (Karabiner path).

set -u

LOG="${HERDR_DETACH_LOG:-$HOME/.herdr-detach.log}"
log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG" 2>/dev/null; }

# Detached tmux sessions are named with this prefix so the detached-dash list
# plugin's collector can enumerate them (`tmux list-sessions` filtered here).
DETACHED_PREFIX="${HERDR_DETACHED_PREFIX:-herdr-detached}"

# ---------------------------------------------------------------------------
# 0. Resolve the herdr client binary + jq + tmux (never assume PATH).
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

if ! command -v tmux >/dev/null 2>&1; then
  for t in /opt/homebrew/bin/tmux /usr/local/bin/tmux /usr/bin/tmux; do
    [[ -x "$t" ]] && { export PATH="$(dirname "$t"):$PATH"; break; }
  done
fi
command -v tmux >/dev/null 2>&1 || { log "FATAL: tmux not found"; exit 3; }

hd() { "$HERDR" "$@"; }

toast() {
  local title="$1" body="${2:-}"
  log "TOAST: $title — $body"
  hd notification show "$title" ${body:+--body "$body"} >>"$LOG" 2>&1 || true
}

# ---------------------------------------------------------------------------
# 1. Resolve the SOURCE pane.
# ---------------------------------------------------------------------------
PANE="${HERDR_PANE_ID:-}"
if [[ -z "$PANE" ]]; then
  PANE="$(hd pane list 2>/dev/null | jq -r '.result.panes[] | select(.focused) | .pane_id' | head -1)"
fi
if [[ -z "$PANE" || "$PANE" == "null" ]]; then
  log "FATAL: could not resolve source pane (no HERDR_PANE_ID and no focused pane)"
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
WS="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.workspace_id // empty')"
TAB_ID="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.tab_id // empty')"
[[ -z "$CWD" ]] && CWD="$HOME"
log "agent=$AGENT value=$VALUE cwd=$CWD ws=$WS"

if [[ -z "$AGENT" ]]; then
  toast "Detach: no agent" "This pane has no detected agent conversation to detach."
  log "FATAL: pane $PANE has no detected agent"
  exit 5
fi

# ---------------------------------------------------------------------------
# 2b. Fallback: agent detected but no session ref reported yet. For claude/cx,
#     recover the newest transcript id for this cwd (mirror fork-conversation.sh).
# ---------------------------------------------------------------------------
if [[ -z "$VALUE" ]]; then
  case "$AGENT" in
    claude)
      ENC="$(printf '%s' "$CWD" | LC_ALL=C sed 's/[^A-Za-z0-9]/-/g')"
      for base in "$HOME/.claude-code-router/profiles/default-claude-code/claude" "$HOME/.claude"; do
        newest="$(ls -t "$base/projects/$ENC/"*.jsonl 2>/dev/null | head -1)"
        if [[ -n "$newest" ]]; then
          VALUE="$(basename "$newest" .jsonl)"
          log "recovered claude session $VALUE from $base"
          break
        fi
      done
      ;;
  esac
fi
if [[ -z "$VALUE" ]]; then
  toast "Detach failed" "No session id found for this pane's conversation."
  log "FATAL: no session ref for agent=$AGENT on pane $PANE"
  exit 6
fi

# ---------------------------------------------------------------------------
# 3. Build the per-agent RESUME command (no fork flag — the SAME conversation
#    continues in tmux; herdr's own resume convention, see src/agent_resume.rs).
# ---------------------------------------------------------------------------
RESUME_CMD=""
case "$AGENT" in
  claude)
    # cx-resume routes cx-origin vs daily by transcript location; no --fork-session
    # so the exact same session id is resumed (one live process).
    RESUME_CMD="cx-resume $(printf '%q' "$VALUE")"
    ;;
  codex)
    RESUME_CMD="codex resume $(printf '%q' "$VALUE")"
    ;;
  pi | omp)
    RESUME_CMD="omp --resume=$(printf '%q' "$VALUE")"
    ;;
  *)
    toast "Detach: unsupported agent" "Detach is not wired for agent '$AGENT'."
    log "FATAL: detach not supported for agent '$AGENT'"
    exit 7
    ;;
esac
log "resume cmd: $RESUME_CMD"

# ---------------------------------------------------------------------------
# 4. Derive a unique tmux session name from the SOURCE tab label / cwd basename
#    plus a short session-id suffix. tmux session names can't contain '.' or ':',
#    so sanitize to [A-Za-z0-9_-].
# ---------------------------------------------------------------------------
SRC_NAME=""
if [[ -n "$TAB_ID" ]]; then
  SRC_NAME="$(hd tab list --workspace "$WS" 2>/dev/null \
    | jq -r --arg t "$TAB_ID" '.result.tabs[]? | select(.tab_id == $t) | .label // empty')"
fi
[[ -z "$SRC_NAME" ]] && SRC_NAME="$(basename "$CWD")"
SLUG="$(printf '%s' "$SRC_NAME" | LC_ALL=C sed 's/[^A-Za-z0-9]/-/g' | sed 's/--*/-/g; s/^-//; s/-$//')"
[[ -z "$SLUG" ]] && SLUG="session"
SHORT="$(printf '%s' "$VALUE" | LC_ALL=C sed 's/[^A-Za-z0-9]//g' | cut -c1-8)"
BASE="${DETACHED_PREFIX}-${SLUG}-${SHORT}"
TMUX_NAME="$BASE"
i=2
while tmux has-session -t "$TMUX_NAME" 2>/dev/null; do
  TMUX_NAME="${BASE}-${i}"
  i=$((i + 1))
done
log "tmux session name: $TMUX_NAME"

# ---------------------------------------------------------------------------
# 5. Close the herdr pane FIRST so the in-pane agent process exits and frees the
#    session id, THEN resume the same session inside the external tmux session.
#    Order guarantees exactly one live process for the id (no concurrent-resume
#    conflict, no fork). We stash the resume metadata in tmux user options so the
#    list plugin / restore path can read agent + session id + cwd back later.
# ---------------------------------------------------------------------------
hd pane close "$PANE" >>"$LOG" 2>&1 || log "WARN: pane close returned non-zero (continuing)"

# Give the agent process a moment to terminate and flush its transcript before we
# resume the same id.
sleep 0.6

# Launch the detached session. `exec` replaces tmux's shell with the agent so the
# session dies (and disappears from the list) the instant the agent exits.
if tmux new-session -d -s "$TMUX_NAME" -c "$CWD" "exec $RESUME_CMD"; then
  tmux set-option -t "$TMUX_NAME" @herdr_detached_agent "$AGENT" 2>/dev/null || true
  tmux set-option -t "$TMUX_NAME" @herdr_detached_session "$VALUE" 2>/dev/null || true
  tmux set-option -t "$TMUX_NAME" @herdr_detached_cwd "$CWD" 2>/dev/null || true
  tmux set-option -t "$TMUX_NAME" @herdr_detached_label "$SRC_NAME" 2>/dev/null || true
  toast "Detached: $SRC_NAME" "Now running in tmux '$TMUX_NAME' — survives herdr restarts."
  log "DETACH OK: $AGENT $VALUE -> tmux $TMUX_NAME (cwd=$CWD)"
  exit 0
else
  # The pane is already closed; make sure the session id is not lost. The
  # transcript persists on disk, so it can be resumed in herdr again from here.
  toast "Detach FAILED — conversation preserved" \
    "tmux launch failed. Resume $AGENT session '$VALUE' in herdr (cwd $CWD)."
  log "FATAL: tmux new-session failed for $TMUX_NAME; agent=$AGENT session=$VALUE cwd=$CWD (transcript intact)"
  exit 8
fi
