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

# Say it out loud, every time. The strip's version token INVITES this click and
# then arms a confirmation, so a branch that exits quietly reads as a dead
# affordance — which is exactly how this script was reported as a broken button
# while its own log showed three successful runs (canon §7: an action that
# cannot be performed says so).
say() {
  log "TOAST: $*"
  local shown=""
  if [[ -n "${HERDR:-}" ]]; then
    shown="$("$HERDR" notification show "Claude Code update" --body "$*" 2>&1)"
  fi
  case "$shown" in
    *'"shown":true'* | *'"shown": true'*) return 0 ;;
  esac
  # herdr toasts are delivery-off by default; the desktop notifier is the
  # fallback the dash scripts already standardised on.
  command -v notify-send >/dev/null 2>&1 &&
    notify-send -a herdr -- "Claude Code update" "$*" >>"$LOG" 2>&1
  return 0
}

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
  say "the updater failed (rc=$RC) — the pane was left running"
  log "update-claude-code: updater failed, leaving the pane alone"
  exit "$RC"
fi

# ── 1b. What is the PANE running? ───────────────────────────────────────────
#
# THE BUG THIS CLOSES. The version the strip shows — and therefore the version
# in "2.1.231→2.1.232 click to update?" — is the one stamped on the pane's own
# transcript records, i.e. the build the agent was STARTED with. The binary on
# disk is a different fact. Once `claude update` has installed 2.1.232, every
# further click found BEFORE == AFTER, logged "already current", and exited
# silently — while the pane went on running 2.1.231 and the affordance went on
# asking to be clicked. Three real runs in the plugin log, exit 0 each time, and
# a user correctly reporting a dead button.
#
# So the question is not "did the binary change" but "is this pane behind the
# binary". The strip's own snapshot is the authority for the first half, since
# it is literally what the user is looking at.
STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/herdr/plugins/builtin.usage-dash"
PANE_VER=""
if command -v jq >/dev/null 2>&1 && [[ -r "$STATE_DIR/usage_input.json" ]]; then
  PANE_VER="$(jq -r '.snapshot.versions.claude.current // empty' \
    "$STATE_DIR/usage_input.json" 2>/dev/null)"
fi
# `claude --version` prints "2.1.232 (Claude Code)"; the transcript stamps the
# bare number. Compare the numbers.
INSTALLED_VER="${AFTER%% *}"
log "update-claude-code: installed='$INSTALLED_VER' pane='${PANE_VER:-unknown}'"

if [[ -n "$PANE_VER" && "$PANE_VER" == "$INSTALLED_VER" ]]; then
  # Truly nothing to do: the pane is already on the installed build.
  say "already on $INSTALLED_VER — nothing to update"
  log "update-claude-code: pane already on $INSTALLED_VER — nothing to do"
  exit 0
fi

if [[ -n "$BEFORE" && "$BEFORE" == "$AFTER" ]]; then
  # The binary did not move, but the PANE is behind it: relaunching this session
  # on the installed build is exactly what the user asked for by clicking. Fall
  # through to the resume handoff instead of exiting.
  log "update-claude-code: binary already $AFTER; pane on ${PANE_VER:-unknown} — relaunching the pane onto it"
fi

# ── 2. Resume this pane on the new build ────────────────────────────────────
if [[ -z "$PANE" ]]; then
  say "updated to $AFTER — open a new pane to use it (no pane in this click's context)"
  log "update-claude-code: no HERDR_PANE_ID — updated to $AFTER, pane not restarted"
  exit 0
fi

# Ask herdr which session and cwd this pane holds; both are required to build a
# resumable handoff. `pane get` answers with
# {"result":{"pane":{"agent_session":{"value":"<sid>"},"cwd":"<dir>",...}}}.
INFO="$("$HERDR" pane get "$PANE" 2>/dev/null)"
# jq, not python3: jq is already a hard dependency of every dash script on this
# box, and a bare `python3` is one `command not found` away from turning this
# whole path into another silent no-op.
SID="$(printf '%s' "$INFO" | jq -r '.result.pane.agent_session.value // empty' 2>/dev/null)"
CWD="$(printf '%s' "$INFO" | jq -r '.result.pane.cwd // .result.pane.foreground_cwd // empty' 2>/dev/null)"

if [[ -z "$SID" || -z "$CWD" || ! -d "$CWD" ]]; then
  say "$AFTER is installed, but this pane has no resumable session yet — restart it yourself"
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

say "relaunching this pane on $INSTALLED_VER"
log "update-claude-code: done — $BEFORE → $AFTER, pane $PANE resuming session $SID"
