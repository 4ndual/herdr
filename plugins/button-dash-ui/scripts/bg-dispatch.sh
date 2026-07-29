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
log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG" 2>/dev/null; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROMPT_SCRIPT="$SCRIPT_DIR/bg-prompt.sh"

# ---------------------------------------------------------------------------
# 0. Resolve the herdr client binary (prefer the fork build; never assume PATH).
#    PATH LAST, deliberately: `command -v herdr` resolves to ~/.local/bin/herdr,
#    a symlink to a June-19 pre-prod build missing this fork's subcommands —
#    the root cause of the broken resume-search button.
# ---------------------------------------------------------------------------
HERDR=""
for cand in "${HERDR_BIN:-}" \
  "$HOME/.herdr-hr/bin/herdr" \
  "$HOME/Projects/herdr-worktrees/imgapi-127/target/release/herdr" \
  "$HOME/Projects/herdr/target/release/herdr" \
  "$(command -v herdr 2>/dev/null)"; do
  if [[ -n "$cand" && -x "$cand" ]]; then
    HERDR="$cand"
    break
  fi
done

hd() { "$HERDR" "$@"; }

# Surface a short message to the user. Prefer a herdr toast; always log too.
toast() {
  local title="$1" body="${2:-}"
  log "TOAST: $title — $body"
  [[ -n "$HERDR" ]] || return 0
  hd notification show "$title" ${body:+--body "$body"} >>"$LOG" 2>&1 || true
}

if [[ -z "$HERDR" ]]; then
  # No binary means no toast channel either; the log is all we have.
  log "FATAL: no herdr binary found"
  exit 3
fi

if ! command -v jq >/dev/null 2>&1; then
  for j in /opt/homebrew/bin/jq /usr/local/bin/jq /usr/bin/jq; do
    [[ -x "$j" ]] && { export PATH="$(dirname "$j"):$PATH"; break; }
  done
fi
if ! command -v jq >/dev/null 2>&1; then
  toast "Background agent failed" "jq not found"
  exit 3
fi

# ---------------------------------------------------------------------------
# 0b. `claude` must exist — dispatching is pointless otherwise, and failing here
#     is far clearer than failing inside a freshly-opened pane.
# ---------------------------------------------------------------------------
CLAUDE=""
for cand in "${CLAUDE_BIN:-}" "$(command -v claude 2>/dev/null)" \
  "$HOME/.local/bin/claude" "/opt/homebrew/bin/claude"; do
  [[ -n "$cand" && -x "$cand" ]] && { CLAUDE="$cand"; break; }
done
if [[ -z "$CLAUDE" ]]; then
  toast "Background agent failed" "claude not found"
  exit 3
fi

if [[ ! -x "$PROMPT_SCRIPT" ]]; then
  toast "Background agent failed" "bg-prompt.sh missing or not executable"
  exit 3
fi

# ---------------------------------------------------------------------------
# 1. Resolve the SOURCE pane (button: HERDR_PANE_ID; hotkey: focused pane).
# ---------------------------------------------------------------------------
PANE="${HERDR_PANE_ID:-}"
if [[ -z "$PANE" ]]; then
  PANE="$(hd pane list 2>/dev/null | jq -r '.result.panes[] | select(.focused) | .pane_id' | head -1)"
fi
if [[ -z "$PANE" || "$PANE" == "null" ]]; then
  toast "Background agent failed" "could not resolve the current pane"
  exit 4
fi
log "source pane: $PANE"

# ---------------------------------------------------------------------------
# 2. Read the source pane's cwd (the agent's project context) and workspace.
# ---------------------------------------------------------------------------
PANE_JSON="$(hd pane get "$PANE" 2>/dev/null)"
if [[ $? -ne 0 || -z "$PANE_JSON" ]]; then
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
