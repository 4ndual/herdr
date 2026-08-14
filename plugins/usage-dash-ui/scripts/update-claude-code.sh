#!/usr/bin/env bash
# Update Claude Code, then resume this pane's session on the new build.
#
# Invoked from the usage strip's Claude version token, and ONLY after the guest
# has seen a confirming second click (see `handle_version_click` in src/lib.rs) —
# this restarts the user's agent, so it must never be one stray click away.
#
# Install shape on this machine: Claude Code is a STANDALONE native install
# (~/.local/share/claude/versions/<v>), not an npm package, so the updater is
# `claude update`, which re-checks against its own release channel. The version
# the strip displays comes from the npm registry as a cheap proxy; `claude
# update` is the authority and will simply report "already up to date" if the
# proxy was ahead.
#
# Resume mechanism: ~/.zshrc wraps claude in `_claude_loop`, which relaunches
# `claude --resume <sid>` when `${CLAUDE_CONFIG_DIR:-~/.claude}/.relocate-next` exists (two lines:
# target dir, then session id) and otherwise exits. Writing that handoff and
# then asking claude to exit therefore hands the session to the new binary
# without sending blind keystrokes into a live TUI.
#
# Fail-safe: if the session id or cwd cannot be determined, the update still
# happens but the pane is NOT touched. A pane left running an old build is a
# minor annoyance; a pane killed without a resumable session is data loss.

set -uo pipefail

LOG="$HOME/.herdr-cli-update.log"
log() { printf '[%s] %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*" >>"$LOG"; }

HERDR="${HERDR_BIN_PATH:-herdr}"
PANE="${HERDR_PANE_ID:-}"

log "update-claude-code: start (pane=${PANE:-none})"

# ── 1. Update ───────────────────────────────────────────────────────────────
# Resolve the real binary rather than the `claude` shell function from .zshrc,
# which is interactive-only and not defined in this non-interactive shell.
CLAUDE_BIN=""
for cand in "$HOME/.local/bin/claude" "$(command -v claude 2>/dev/null || true)"; do
  if [[ -n "$cand" && -x "$cand" ]]; then CLAUDE_BIN="$cand"; break; fi
done
if [[ -z "$CLAUDE_BIN" ]]; then
  # Fall back to the newest versioned native build.
  CLAUDE_BIN="$(ls -1 "$HOME/.local/share/claude/versions" 2>/dev/null | sort -V | tail -1)"
  [[ -n "$CLAUDE_BIN" ]] && CLAUDE_BIN="$HOME/.local/share/claude/versions/$CLAUDE_BIN"
fi
if [[ -z "$CLAUDE_BIN" || ! -x "$CLAUDE_BIN" ]]; then
  log "update-claude-code: FAILED — no claude binary found"
  exit 3
fi

BEFORE="$("$CLAUDE_BIN" --version 2>/dev/null | tr -d '\n')"
OUT="$("$CLAUDE_BIN" update 2>&1)"
RC=$?
AFTER="$("$CLAUDE_BIN" --version 2>/dev/null | tr -d '\n')"
log "update-claude-code: rc=$RC before='$BEFORE' after='$AFTER' out='${OUT//$'\n'/ | }'"

if [[ $RC -ne 0 ]]; then
  log "update-claude-code: updater failed, leaving the pane alone"
  exit "$RC"
fi

# Nothing changed → nothing to resume onto. Restarting the pane would interrupt
# the user for no benefit whatsoever.
if [[ -n "$BEFORE" && "$BEFORE" == "$AFTER" ]]; then
  log "update-claude-code: already current ($AFTER) — pane left running"
  exit 0
fi

# ── 2. Resume this pane on the new build ────────────────────────────────────
if [[ -z "$PANE" ]]; then
  log "update-claude-code: no HERDR_PANE_ID — updated to $AFTER, pane not restarted"
  exit 0
fi

# Ask herdr which session and cwd this pane holds; both are required to build a
# resumable handoff. `pane get` answers with
# {"result":{"pane":{"agent_session":{"value":"<sid>"},"cwd":"<dir>",...}}}.
INFO="$("$HERDR" pane get "$PANE" 2>/dev/null)"
read -r SID CWD <<<"$(printf '%s' "$INFO" | python3 -c '
import sys, json
try:
    pane = json.load(sys.stdin)["result"]["pane"]
except Exception:
    print(" ")
    raise SystemExit
sid = (pane.get("agent_session") or {}).get("value") or ""
cwd = pane.get("cwd") or ""
# Space-separated is safe here: session ids are uuids and a cwd containing a
# space would fail the -d test below and take the fail-safe path.
print(f"{sid} {cwd}")
' 2>/dev/null)"

if [[ -z "$SID" || -z "$CWD" || ! -d "$CWD" ]]; then
  log "update-claude-code: updated to $AFTER but sid/cwd unresolved (sid='$SID' cwd='$CWD') — pane left running, resume it yourself"
  exit 0
fi

# The handoff the relocate-loop consumes: line 1 target dir, line 2 session id.
# Under the ACCOUNT root, not $HOME: a pane running in a fleet harness has its
# own CLAUDE_CONFIG_DIR, and writing the handoff to the default root means the
# harness's relocate loop never sees it while the default account picks up a
# handoff for a session it does not own.
CLAUDE_ROOT="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
mkdir -p "$CLAUDE_ROOT"
printf '%s\n%s\n' "$CWD" "$SID" >"$CLAUDE_ROOT/.relocate-next"
log "update-claude-code: handoff written (sid=$SID cwd=$CWD); asking pane $PANE to exit"

# `/exit` is Claude Code's own clean shutdown: it finishes writing the
# transcript, which is what makes the subsequent --resume work.
"$HERDR" pane send-text "$PANE" "/exit" >/dev/null 2>&1
sleep 1
"$HERDR" pane send-keys "$PANE" Enter >/dev/null 2>&1

log "update-claude-code: done — $BEFORE → $AFTER, pane $PANE resuming session $SID"
