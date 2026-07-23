#!/usr/bin/env bash
# fork-conversation.sh — fork the CURRENT pane's agent conversation into a new
# pane, either in a NEW TAB (current workspace) or a NEW WORKSPACE.
#
# The "Fork" / "Fork → workspace" buttons and the ⌃⌥F / ⌃⌥⇧F Karabiner hotkeys
# all run this. It works for every agent herdr can resume — Claude Code, cx
# (GPT-via-CCR, detected as claude), Codex, and pi — by reusing herdr's OWN
# per-agent invocation convention (see src/agent_resume.rs) and appending each
# agent's fork flag so the ORIGINAL conversation is left intact and the fork
# gets a fresh session id:
#
#   claude / cx : cx-resume <id> --fork-session
#                 (cx-resume routes cx-origin vs daily by transcript location and
#                  forwards --fork-session to `claude --resume <id> --fork-session`)
#   codex       : codex fork <id>
#   pi          : pi --fork <value>
#
# Nothing is hardcoded: pane, agent, session ref and cwd are all read live from
# `herdr pane get <pane>`; the herdr binary and socket are detected from the env
# (button path) or supplied by the Karabiner rule.
#
# Usage:  fork-conversation.sh [tab|workspace]
#   tab        (default) new tab in the CURRENT workspace, running the fork
#   workspace  new workspace, running the fork
#
# Placement mechanics: `tab create` / `workspace create` each come up with a
# single root pane; we run the fork command IN that root pane via `pane run`
# (no stray empty pane, unlike `agent start` which adds a second pane). herdr's
# own agent detection then re-labels the pane from the running process.

set -u

LOG="${HERDR_FORK_LOG:-$HOME/.herdr-fork.log}"
log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG" 2>/dev/null; }

PLACEMENT="${1:-tab}"
case "$PLACEMENT" in
  tab | workspace) ;;
  *)
    log "FATAL: unknown placement '$PLACEMENT' (want tab|workspace)"
    exit 2
    ;;
esac

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

# jq is required to parse the JSON responses.
if ! command -v jq >/dev/null 2>&1; then
  # Homebrew default location as a fallback when PATH is minimal (Karabiner).
  for j in /opt/homebrew/bin/jq /usr/local/bin/jq /usr/bin/jq; do
    [[ -x "$j" ]] && { export PATH="$(dirname "$j"):$PATH"; break; }
  done
fi
command -v jq >/dev/null 2>&1 || { log "FATAL: jq not found"; exit 3; }

hd() { "$HERDR" "$@"; }

# ---------------------------------------------------------------------------
# 1. Resolve the SOURCE pane.
#    Button path: HERDR_PANE_ID is inherited from the server env.
#    Karabiner path: no pane env — pick the focused pane from `pane list`.
# ---------------------------------------------------------------------------
PANE="${HERDR_PANE_ID:-}"
if [[ -z "$PANE" ]]; then
  PANE="$(hd pane list 2>/dev/null | jq -r '.result.panes[] | select(.focused) | .pane_id' | head -1)"
fi
if [[ -z "$PANE" || "$PANE" == "null" ]]; then
  log "FATAL: could not resolve source pane (no HERDR_PANE_ID and no focused pane)"
  exit 4
fi
log "source pane: $PANE (placement=$PLACEMENT)"

# ---------------------------------------------------------------------------
# 2. Read the source pane's agent, session ref and cwd.
# ---------------------------------------------------------------------------
PANE_JSON="$(hd pane get "$PANE" 2>/dev/null)"
AGENT="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.agent // empty')"
KIND="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.agent_session.kind // empty')"
VALUE="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.agent_session.value // empty')"
CWD="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.cwd // .result.pane.foreground_cwd // empty')"
WS="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.workspace_id // empty')"
[[ -z "$CWD" ]] && CWD="$HOME"
log "agent=$AGENT kind=$KIND value=$VALUE cwd=$CWD ws=$WS"

if [[ -z "$AGENT" ]]; then
  log "FATAL: pane $PANE has no detected agent — nothing to fork"
  exit 5
fi

# ---------------------------------------------------------------------------
# 2b. Fallback: agent detected but no session ref reported yet. For claude/cx,
#     recover the newest transcript id for this cwd (mirror cmux-fork-session).
# ---------------------------------------------------------------------------
if [[ -z "$VALUE" ]]; then
  case "$AGENT" in
    claude)
      ENC="$(printf '%s' "$CWD" | LC_ALL=C sed 's/[^A-Za-z0-9]/-/g')"
      for base in "$HOME/.claude-code-router/profiles/default-claude-code/claude" "$HOME/.claude"; do
        newest="$(ls -t "$base/projects/$ENC/"*.jsonl 2>/dev/null | head -1)"
        if [[ -n "$newest" ]]; then
          VALUE="$(basename "$newest" .jsonl)"
          KIND="id"
          log "recovered claude session $VALUE from $base"
          break
        fi
      done
      ;;
  esac
fi
if [[ -z "$VALUE" ]]; then
  log "FATAL: no session ref for agent=$AGENT on pane $PANE"
  exit 6
fi

# ---------------------------------------------------------------------------
# 3. Build the per-agent fork command (reuses herdr's resume convention +
#    each agent's fork flag; leaves the original conversation intact).
# ---------------------------------------------------------------------------
FORK_CMD=""
case "$AGENT" in
  claude)
    # Covers both plain Claude Code and cx (cx-resume auto-dispatches by where
    # the transcript lives; --fork-session threads through to claude).
    FORK_CMD="cx-resume $(printf '%q' "$VALUE") --fork-session"
    ;;
  codex)
    FORK_CMD="codex fork $(printf '%q' "$VALUE")"
    ;;
  pi | omp)
    FORK_CMD="pi --fork $(printf '%q' "$VALUE")"
    ;;
  *)
    log "FATAL: fork not supported for agent '$AGENT'"
    exit 7
    ;;
esac
log "fork cmd: $FORK_CMD"

# ---------------------------------------------------------------------------
# 3b. Derive the fork's label from the SOURCE tab's name: "<name> fork" (e.g.
#     the "herdr" tab forks to "herdr fork"). Falls back to the cwd basename,
#     then a bare "fork" if the source tab is unnamed. Re-forking a "<name> fork"
#     tab keeps a single " fork" suffix (no "herdr fork fork").
# ---------------------------------------------------------------------------
TAB_ID="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.tab_id // empty')"
SRC_NAME=""
if [[ -n "$TAB_ID" ]]; then
  SRC_NAME="$(hd tab list --workspace "$WS" 2>/dev/null \
    | jq -r --arg t "$TAB_ID" '.result.tabs[]? | select(.tab_id == $t) | .label // empty')"
fi
[[ -z "$SRC_NAME" ]] && SRC_NAME="$(basename "$CWD")"
# strip a trailing " fork" so a re-fork stays "<name> fork", not "<name> fork fork"
SRC_NAME="${SRC_NAME% fork}"
if [[ -n "$SRC_NAME" ]]; then
  LABEL="$SRC_NAME fork"
else
  LABEL="fork"
fi
log "fork label: $LABEL (source tab=$TAB_ID)"

# ---------------------------------------------------------------------------
# 4. Create the destination surface and run the fork in its root pane.
# ---------------------------------------------------------------------------
case "$PLACEMENT" in
  tab)
    [[ -z "$WS" ]] && WS="$HERDR_WORKSPACE_ID"
    CREATE_JSON="$(hd tab create --workspace "$WS" --cwd "$CWD" --label "$LABEL" --focus 2>/dev/null)"
    ROOT_PANE="$(printf '%s' "$CREATE_JSON" | jq -r '.result.root_pane.pane_id // empty')"
    ;;
  workspace)
    CREATE_JSON="$(hd workspace create --cwd "$CWD" --label "$LABEL" --focus 2>/dev/null)"
    ROOT_PANE="$(printf '%s' "$CREATE_JSON" | jq -r '.result.root_pane.pane_id // empty')"
    ;;
esac

if [[ -z "$ROOT_PANE" || "$ROOT_PANE" == "null" ]]; then
  log "FATAL: could not create $PLACEMENT (response: $CREATE_JSON)"
  exit 8
fi
log "destination root pane: $ROOT_PANE"

# Small settle so the root pane's login shell is at its prompt before we type.
sleep 0.4

# Run the fork. `exec` replaces the shell so closing the agent closes the pane,
# matching how a normal agent pane behaves.
hd pane run "$ROOT_PANE" "exec $FORK_CMD" >>"$LOG" 2>&1
log "FORK OK: $AGENT $VALUE -> $ROOT_PANE ($PLACEMENT)"
exit 0
