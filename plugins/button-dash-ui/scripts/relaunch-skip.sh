#!/usr/bin/env bash
# relaunch-skip.sh — relaunch the CURRENT pane's conversation IN PLACE, in
# skip/yolo permission mode.
#
# The "Relaunch skip" button runs this. Unlike `fork`, nothing is branched and no
# new tab appears: the SAME session is resumed in the SAME tab with the runtime's
# permission bypass flag on, and the old pane goes away. It is the button for
# "I started this agent in ask-me mode and I am now tired of approving things".
#
#   claude -> claude --resume <id> --dangerously-skip-permissions
#   codex  -> codex resume <id> --dangerously-bypass-approvals-and-sandbox
#   pi/jcode -> REFUSED with a toast. Neither has a verified bypass flag
#               (`pi --help` has none; jcode is not installed here), and
#               relaunching them WITHOUT the flag would be a no-op that
#               destroyed and recreated the pane for nothing.
#
# The argv (and the harness `env CLAUDE_CONFIG_DIR=… / CODEX_HOME=…` prefix a
# fleet-harness session needs) comes from `agent_launch_cmd` in dash-common.sh —
# the single source of truth shared with fork and the archive dash.
#
# ── Why spawn-then-close, and not "restart the pane" ────────────────────────
# There is no respawn/exec/replace method in the herdr API (checked the whole
# Method enum: TabCreate, AgentStart, AgentSpawn, PaneSplit/Close/Run/SendText,
# LayoutApply, …). And the obvious fallback — send Ctrl-D, then type the new
# command — cannot work: an agent pane created by `pane run "exec …"` or by
# `agent.spawn` IS the agent process, with no shell underneath it, so killing the
# agent kills the pane.
#
# So: `herdr agent start <label> --tab <tab> --split right -- <argv>` spawns the
# resumed session as a real agent pane (argv passed directly — no shell-prompt
# race, no quoting through `pane run`), and only THEN is the old pane closed, at
# which point the split collapses and the survivor fills the tab. Spawning first
# is load-bearing: in a single-pane tab, closing first would take the tab with it.
#
# Every refusal toasts. A button that silently does nothing is a dead button.

set -u

LOG="${HERDR_RELAUNCH_LOG:-$HOME/.herdr-relaunch.log}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=dash-common.sh
. "$SCRIPT_DIR/dash-common.sh" || {
  echo "relaunch: dash-common.sh missing" >&2
  exit 3
}

if [[ -z "$HERDR" ]]; then
  log "FATAL: no herdr binary found (HERDR_BIN/HERDR_BIN_PATH unset and none on PATH)"
  desktop_notify "Relaunch failed" "no herdr binary found"
  exit 3
fi
command -v jq >/dev/null 2>&1 || {
  toast "Relaunch failed" "jq is not installed"
  exit 3
}

# 1. Source pane. Button path inherits HERDR_PANE_ID; the tab menu nulls it, so
#    current_pane() falls back to the focused pane.
if ! PANE="$(current_pane)"; then
  toast "Relaunch failed" "could not resolve the current pane"
  exit 4
fi

PANE_JSON="$(hd pane get "$PANE" 2>/dev/null)"
AGENT="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.agent // empty')"
VALUE="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.agent_session.value // empty')"
CWD="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.cwd // .result.pane.foreground_cwd // empty')"
TAB_ID="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.tab_id // empty')"
[[ -z "$CWD" ]] && CWD="$HOME"
log "pane=$PANE agent=$AGENT value=$VALUE cwd=$CWD tab=$TAB_ID"

if [[ -z "$AGENT" ]]; then
  toast "Relaunch: no agent here" "This pane has no running agent to relaunch."
  exit 5
fi
# "cx" is a cosmetic label for Claude Code (dash-common.sh maps it too).
[[ "$AGENT" == "cx" ]] && AGENT="claude"

case "$AGENT" in
  claude | codex) ;;
  *)
    toast "Relaunch not available" \
      "skip/bypass mode is a claude and codex flag; '$AGENT' has no verified equivalent"
    exit 5
    ;;
esac

# 2. Session id. Same claude fallback as fork-conversation.sh: the detector may
#    not have reported a session ref yet, and the newest transcript for this cwd
#    is the pane's own. Both the default root and every fleet-harness root are
#    searched, because a harness pane's transcript is not under ~/.claude.
if [[ -z "$VALUE" && "$AGENT" == "claude" ]]; then
  ENC="$(printf '%s' "$CWD" | LC_ALL=C sed 's/[^A-Za-z0-9]/-/g')"
  newest="$(ls -t "${CLAUDE_CONFIG_DIR:-$HOME/.claude}/projects/$ENC/"*.jsonl \
    "$HOME"/harnesses/*/config/projects/"$ENC"/*.jsonl 2>/dev/null | head -1)"
  if [[ -n "$newest" ]]; then
    VALUE="$(basename "$newest" .jsonl)"
    log "recovered claude session $VALUE from $newest"
  fi
fi
if [[ -z "$VALUE" ]]; then
  toast "Relaunch failed" "no session id reported for this $AGENT pane yet"
  exit 6
fi
if [[ -z "$TAB_ID" ]]; then
  toast "Relaunch failed" "could not resolve this pane's tab"
  exit 6
fi

# 3. Build the resume argv WITH the skip flag (mode defaults to skip).
if ! CMD="$(agent_launch_cmd resume "$AGENT" "$VALUE")"; then
  toast "Relaunch not available" "$CMD"
  exit 7
fi
log "relaunch cmd: $CMD"

# `agent_launch_cmd` emits a shell-quoted command LINE; `agent start` wants an
# argv vector, so split it back into words the same way a shell would. Safe to
# eval: every field was produced by printf %q in dash-common.sh.
eval "set -- $CMD"
ENV_ARGS=()
if [[ "${1:-}" == "env" ]]; then
  # Harness-owned session: hand the config root to `agent start --env` instead of
  # spawning `env` as the pane's process, so the pane's argv[0] is the agent
  # itself and herdr's detector sees a plain claude/codex process.
  shift
  ENV_ARGS=(--env "$1")
  shift
fi
if [[ $# -eq 0 ]]; then
  toast "Relaunch failed" "could not parse the resume command"
  exit 7
fi
log "argv: $* ${ENV_ARGS[*]:-}"

# 4. Spawn the replacement pane in the SAME tab, then close the old one.
# Agent names are unique per session (`agent_name_taken` otherwise), and a
# previously relaunched pane keeps its name for its whole life — suffix with the
# session id so two skip-relaunched panes can coexist.
LABEL="$AGENT skip ${VALUE:0:6}"
SPAWN_JSON="$(hd agent start "$LABEL" --tab "$TAB_ID" --split right --cwd "$CWD" \
  "${ENV_ARGS[@]}" --focus -- "$@" 2>&1)"
# agent.spawn answers {"result":{"type":"agent_started","agent":{…,"pane_id":…}}}
# (verified live against the herdrtest instance); the two fallbacks are there so
# a future response shape does not silently strand the old pane.
NEW_PANE="$(printf '%s' "$SPAWN_JSON" |
  jq -r '.result.agent.pane_id // .result.pane_id // .result.pane.pane_id // empty' 2>/dev/null)"
if [[ -z "$NEW_PANE" || "$NEW_PANE" == "null" ]]; then
  log "agent start response: $SPAWN_JSON"
  toast "Relaunch failed" "could not spawn the skip-mode pane"
  exit 8
fi
log "new pane: $NEW_PANE"

if ! hd pane close "$PANE" >>"$LOG" 2>&1; then
  # The new pane is live and usable; the only damage is that the old one is still
  # there, so say that rather than pretending it worked.
  toast "Relaunch partly done" "started $AGENT in skip mode, but could not close the old pane"
  exit 9
fi
log "RELAUNCH OK: $AGENT $VALUE -> $NEW_PANE (closed $PANE)"
exit 0
