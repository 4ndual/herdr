#!/usr/bin/env bash
# bg-dispatch.sh — open a NEW TAB whose pane collects a prompt and dispatches it
# as a DETACHED Claude Code background agent (`claude --bg "<prompt>"`).
#
# The "Background agent" button runs this. `--bg` REQUIRES a prompt and a herdr
# button cannot take typed input (UiEvent::Key does not reach panel modules), so
# the button's job is only to open a terminal that CAN: the new tab's root pane
# runs scripts/bg-prompt.sh, which reads the prompt, dispatches it, prints the
# resulting agent id, and stays alive.
#
# The new tab inherits the SOURCE pane's cwd — that becomes the agent's project
# context, and `claude agents --cwd` filters on it, so it must be correct.
#
# Every failure path toasts. A silent no-op is how the resume-search bug stayed
# invisible for weeks.

set -u

LOG="${HERDR_BG_LOG:-$HOME/.herdr-bg-dispatch.log}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=dash-common.sh
. "$SCRIPT_DIR/dash-common.sh" || { echo "bg-dispatch: dash-common.sh missing" >&2; exit 3; }

PROMPT_SCRIPT="$SCRIPT_DIR/bg-prompt.sh"

if [[ -z "$HERDR" ]]; then
  log "FATAL: no herdr binary found"
  desktop_notify "Background agent failed" "no herdr binary found"
  exit 3
fi

if ! command -v jq >/dev/null 2>&1; then
  toast "Background agent failed" "jq is not installed"
  exit 3
fi

# ---------------------------------------------------------------------------
# 0b. `claude` must exist — dispatching is pointless otherwise, and failing here
#     is far clearer than failing inside a freshly-opened pane.
#
#     This button is claude-only BY DESIGN, and not because of the pane it was
#     pressed over: `--bg` is a Claude Code primitive with no codex / pi / jcode
#     equivalent, so the source pane's agent is deliberately NOT consulted — you
#     can dispatch a claude background agent from any pane, and the new tab
#     inherits only its cwd.
# ---------------------------------------------------------------------------
if ! CLAUDE="$(find_exe claude)"; then
  toast "Background agent unavailable" \
    "background agents are a Claude Code feature (claude --bg) and claude is not installed"
  exit 3
fi

if [[ ! -x "$PROMPT_SCRIPT" ]]; then
  toast "Background agent failed" "bg-prompt.sh missing or not executable"
  exit 3
fi

# ---------------------------------------------------------------------------
# 1. Resolve the SOURCE pane (button: HERDR_PANE_ID; tab context: focused pane).
# ---------------------------------------------------------------------------
if ! PANE="$(current_pane)"; then
  toast "Background agent failed" "could not resolve the current pane"
  exit 4
fi
log "source pane: $PANE"

# ---------------------------------------------------------------------------
# 2. Read the source pane's cwd (the agent's project context) and workspace.
# ---------------------------------------------------------------------------
if ! PANE_JSON="$(hd pane get "$PANE" 2>/dev/null)" || [[ -z "$PANE_JSON" ]]; then
  toast "Background agent failed" "could not read the current pane"
  exit 4
fi
CWD="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.cwd // .result.pane.foreground_cwd // empty')"
WS="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.workspace_id // empty')"
[[ -z "$CWD" ]] && CWD="$HOME"
[[ -z "$WS" ]] && WS="${HERDR_WORKSPACE_ID:-}"
log "cwd=$CWD ws=$WS"

# ---------------------------------------------------------------------------
# 3. Create the tab and run the prompt wrapper in its root pane.
# ---------------------------------------------------------------------------
CREATE_JSON="$(hd tab create --workspace "$WS" --cwd "$CWD" --label "bg: $(basename "$CWD")" --focus 2>>"$LOG")"
CREATE_RC=$?
ROOT_PANE="$(printf '%s' "$CREATE_JSON" | jq -r '.result.root_pane.pane_id // empty')"
if [[ $CREATE_RC -ne 0 || -z "$ROOT_PANE" || "$ROOT_PANE" == "null" ]]; then
  log "tab create rc=$CREATE_RC response: $CREATE_JSON"
  toast "Background agent failed" "could not create tab"
  exit 8
fi
log "destination root pane: $ROOT_PANE"

# Small settle so the root pane's login shell is at its prompt before we type.
sleep 0.4

# `exec` replaces the shell so the wrapper owns the pane; the wrapper itself
# execs a login shell at the end, keeping the pane alive.
if ! hd pane run "$ROOT_PANE" "exec $(printf '%q' "$PROMPT_SCRIPT")" >>"$LOG" 2>&1; then
  toast "Background agent failed" "could not start the prompt in the new tab"
  exit 9
fi

log "BG OK: wrapper launched in $ROOT_PANE (cwd=$CWD)"
toast "Background agent" "Type your prompt in the new tab"
exit 0
