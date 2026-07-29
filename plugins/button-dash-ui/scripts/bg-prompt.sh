#!/usr/bin/env bash
# bg-prompt.sh — runs IN the pane that bg-dispatch.sh creates.
#
# `claude --bg "<prompt>"` starts a DETACHED background agent and returns
# immediately (manage it later with `claude agents`). It REQUIRES a prompt, and a
# herdr button cannot take typed input (UiEvent::Key never reaches panel
# modules) — so the prompt is collected HERE, by the terminal, which has no such
# limitation.
#
# The pane deliberately STAYS ALIVE afterwards (`exec "$SHELL" -l`): this is the
# only place the dispatched agent's id is ever shown, and it lets the user re-run
# `claude agents` at will.
#
# Cancelling (empty prompt / Ctrl-C) is NOT a failure — it is printed here and
# never toasted.

set -u

LOG="${HERDR_BG_LOG:-$HOME/.herdr-bg-dispatch.log}"
log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG" 2>/dev/null; }

# Resolve `claude` as a BINARY. In the user's zsh, `claude` is a function that
# re-launches itself inside tmux; a non-interactive bash never sees it, but be
# explicit so a stray PATH can't surprise us.
CLAUDE=""
for cand in "${CLAUDE_BIN:-}" "$(command -v claude 2>/dev/null)" \
  "$HOME/.local/bin/claude" "/opt/homebrew/bin/claude"; do
  [[ -n "$cand" && -x "$cand" ]] && { CLAUDE="$cand"; break; }
done
if [[ -z "$CLAUDE" ]]; then
  echo "claude not found — cannot dispatch a background agent."
  log "FATAL(bg-prompt): claude not found"
  exec "${SHELL:-/bin/zsh}" -l
fi

printf 'Prompt for the background agent (Ctrl-C to cancel):\n> '
IFS= read -r PROMPT || PROMPT=""
if [[ -z "$PROMPT" ]]; then
  echo "cancelled — nothing dispatched"
  log "cancelled at prompt (cwd=$PWD)"
  exec "${SHELL:-/bin/zsh}" -l
fi

log "dispatching background agent (cwd=$PWD): $PROMPT"
"$CLAUDE" --bg "$PROMPT"
RC=$?
if [[ $RC -ne 0 ]]; then
  echo "claude --bg exited with status $RC — nothing was dispatched."
  log "FATAL(bg-prompt): claude --bg rc=$RC"
  exec "${SHELL:-/bin/zsh}" -l
fi
log "dispatched OK (cwd=$PWD)"

# Show the background agents for this cwd so the new agent's id is visible.
# NOTE: background entries carry `state` + `name` (interactive ones carry
# `status`); there is no `summary` field.
echo
if command -v jq >/dev/null 2>&1; then
  "$CLAUDE" agents --json --cwd "$PWD" 2>/dev/null \
    | jq -r '.[] | select(.kind == "background")
             | "\(.id)  \(.state // .status // "?")  \(.name // "")"' || true
else
  "$CLAUDE" agents --json --cwd "$PWD" 2>/dev/null || true
fi

exec "${SHELL:-/bin/zsh}" -l
