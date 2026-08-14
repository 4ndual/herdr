#!/usr/bin/env bash
# fork-conversation.sh — fork the CURRENT pane's agent conversation into a new
# pane, either in a NEW TAB (current workspace) or a NEW WORKSPACE.
#
# The "Fork" / "Fork → workspace" buttons run this. It works for every agent
# herdr can resume — Claude Code, Codex, pi and jcode — by reusing herdr's OWN
# per-agent invocation convention (see src/agent_resume.rs) and appending each
# agent's fork flag so the ORIGINAL conversation is left intact and the fork
# gets a fresh session id. The exact per-agent argv lives in ONE place,
# `agent_launch_cmd` in dash-common.sh, shared with the archive dash.
#
# Nothing is hardcoded: pane, agent, session ref and cwd are all read live from
# `herdr pane get <pane>`; the herdr binary and socket are detected from the env
# (button path) or resolved from the focused pane (tab-context path).
#
# Usage:  fork-conversation.sh [tab|workspace]
#   tab        (default) new tab in the CURRENT workspace, running the fork
#   workspace  new workspace, running the fork
#
# Placement mechanics: `tab create` / `workspace create` each come up with a
# single root pane; we run the fork command IN that root pane via `pane run`
# (no stray empty pane, unlike `agent start` which adds a second pane). herdr's
# own agent detection then re-labels the pane from the running process.
#
# Every refusal toasts. A fork that silently does nothing is indistinguishable
# from a dead button.

set -u

LOG="${HERDR_FORK_LOG:-$HOME/.herdr-fork.log}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=dash-common.sh
. "$SCRIPT_DIR/dash-common.sh" || { echo "fork: dash-common.sh missing" >&2; exit 3; }

PLACEMENT="${1:-tab}"
case "$PLACEMENT" in
  tab | workspace) ;;
  *)
    log "FATAL: unknown placement '$PLACEMENT' (want tab|workspace)"
    exit 2
    ;;
esac

if [[ -z "$HERDR" ]]; then
  # No binary means no toast channel through herdr either; desktop_notify still
  # works, and it is the only way the user learns the button did nothing.
  log "FATAL: no herdr binary found (HERDR_BIN/HERDR_BIN_PATH unset and none on PATH)"
  desktop_notify "Fork failed" "no herdr binary found"
  exit 3
fi

# jq is required to parse the JSON responses.
command -v jq >/dev/null 2>&1 || { toast "Fork failed" "jq is not installed"; exit 3; }

# ---------------------------------------------------------------------------
# 1. Resolve the SOURCE pane.
#    Button path: HERDR_PANE_ID is inherited from the server env.
#    Tab-context path: no pane env (the host clears focused_pane_id for a
#    tab-scoped invocation) — pick the focused pane from `pane list`.
# ---------------------------------------------------------------------------
if ! PANE="$(current_pane)"; then
  toast "Fork failed" "could not resolve the current pane"
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
  toast "Fork: no agent here" "This pane has no running agent to fork."
  exit 5
fi

# ---------------------------------------------------------------------------
# 2b. Fallback: agent detected but no session ref reported yet. For claude,
#     recover the newest transcript id for this cwd (mirror cmux-fork-session).
# ---------------------------------------------------------------------------
if [[ -z "$VALUE" ]]; then
  case "$AGENT" in
    claude)
      # Claude Code names a project dir by mapping every non-alphanumeric char
      # of the cwd to '-'. Searched across EVERY claude root, not just the
      # default one: a harness pane's transcript has never been under ~/.claude,
      # so the single-root lookup this replaces reported "no session id" for
      # precisely the panes a fork was most wanted on.
      ENC="$(printf '%s' "$CWD" | LC_ALL=C sed 's/[^A-Za-z0-9]/-/g')"
      newest=""
      while IFS= read -r _root; do
        [ -n "$_root" ] || continue
        _cand="$(ls -t "$_root/projects/$ENC/"*.jsonl 2>/dev/null | head -1)"
        [ -n "$_cand" ] || continue
        if [ -z "$newest" ] || [ "$_cand" -nt "$newest" ]; then
          newest="$_cand"
        fi
      done <<EOF
$(claude_roots)
EOF
      if [[ -n "$newest" ]]; then
        VALUE="$(basename "$newest" .jsonl)"
        KIND="id"
        log "recovered claude session $VALUE"
      fi
      ;;
  esac
fi
if [[ -z "$VALUE" ]]; then
  toast "Fork failed" "no session id reported for this $AGENT pane yet"
  exit 6
fi

# ---------------------------------------------------------------------------
# 3. Build the per-agent fork command. The matrix (and every refusal reason)
#    lives in dash-common.sh so the archive dash cannot drift away from it.
# ---------------------------------------------------------------------------
# On refusal the helper prints the REASON instead of a command (see its header).
if ! FORK_CMD="$(agent_launch_cmd fork "$AGENT" "$VALUE")"; then
  toast "Fork not available" "$FORK_CMD"
  exit 7
fi
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
    [[ -z "$WS" ]] && WS="${HERDR_WORKSPACE_ID:-}"
    CREATE_JSON="$(hd tab create --workspace "$WS" --cwd "$CWD" --label "$LABEL" --focus 2>/dev/null)"
    ROOT_PANE="$(printf '%s' "$CREATE_JSON" | jq -r '.result.root_pane.pane_id // empty')"
    ;;
  workspace)
    CREATE_JSON="$(hd workspace create --cwd "$CWD" --label "$LABEL" --focus 2>/dev/null)"
    ROOT_PANE="$(printf '%s' "$CREATE_JSON" | jq -r '.result.root_pane.pane_id // empty')"
    ;;
esac

if [[ -z "$ROOT_PANE" || "$ROOT_PANE" == "null" ]]; then
  log "tab/workspace create response: $CREATE_JSON"
  toast "Fork failed" "could not create the new $PLACEMENT"
  exit 8
fi
log "destination root pane: $ROOT_PANE"

# Small settle so the root pane's login shell is at its prompt before we type.
sleep 0.4

# Run the fork. `exec` replaces the shell so closing the agent closes the pane,
# matching how a normal agent pane behaves.
if ! hd pane run "$ROOT_PANE" "exec $FORK_CMD" >>"$LOG" 2>&1; then
  toast "Fork failed" "created the $PLACEMENT but could not start $AGENT in it"
  exit 9
fi
log "FORK OK: $AGENT $VALUE -> $ROOT_PANE ($PLACEMENT)"
exit 0
