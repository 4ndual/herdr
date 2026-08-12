#!/usr/bin/env bash
# restart-herdr.sh — painless, fast, instance-agnostic restart of a herdr instance.
#
# The "RESTART HR" button and the global ⌃⌥X (ctrl+option+x) Karabiner hotkey both
# run this. It fully kills the running herdr SERVER for a target instance (graceful
# kill → kill -9 fallback, plus the old tmux client session) so a memory-leaked /
# hung process is truly gone, then relaunches FRESH on the SAME binary, session
# name and XDG roots so herdr restores its persisted session (panes, tabs, and the
# Claude conversation running inside `--resume` cleanly).
#
# Nothing is hardcoded: pid, socket, binary path, XDG roots and tmux host session
# are all detected dynamically from the live server (or, if the server is already
# dead, reconstructed from the instance's XDG convention).
#
# Target resolution (first match wins):
#   1. $HERDR_SESSION + $XDG_CONFIG_HOME already in env  → "restart THIS instance"
#      (this is the in-app button path: the action runs as a child of the herdr
#       server and inherits its env).
#   2. First CLI arg = instance name (e.g. `hr`, `btntest`) → XDG root by the
#      launcher convention  $HOME/.herdr-<name>  (this is the Karabiner path).
#
# The script self-detaches (nohup + background re-exec) so that when it kills the
# herdr server it was launched under, it survives to perform the relaunch.

set -u

# ---------------------------------------------------------------------------
# 0. Self-detach: survive the death of the herdr server that spawned us.
# ---------------------------------------------------------------------------
LOG="${HERDR_RESTART_LOG:-$HOME/.herdr-restart.log}"
if [[ "${_HERDR_RESTART_DETACHED:-}" != "1" ]]; then
  _HERDR_RESTART_DETACHED=1 nohup "$0" "$@" >>"$LOG" 2>&1 < /dev/null &
  exit 0
fi

# Shared helpers, sourced only in the DETACHED run (the first pass does nothing
# but re-exec). Provides exe_path / socket_pid / socket_pids / instance_binary —
# the process resolution that makes this script work on Linux — plus
# desktop_notify for the one failure the user must not have to find in a log.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=dash-common.sh
. "$SCRIPT_DIR/dash-common.sh" || { echo "restart: dash-common.sh missing" >&2; exit 3; }

# Redefine AFTER sourcing: this script's stdout is already the log file (nohup
# above), so it logs by printing, not by appending to $LOG a second time.
log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*"; }

# ---------------------------------------------------------------------------
# 1. Resolve target instance identity (session + XDG_CONFIG_HOME).
# ---------------------------------------------------------------------------
SESSION=""
CFG=""

# Path 1: env from inside herdr (button action inherits the server's env).
if [[ -n "${HERDR_SESSION:-}" && -n "${XDG_CONFIG_HOME:-}" ]]; then
  SESSION="$HERDR_SESSION"
  CFG="$XDG_CONFIG_HOME"
fi

# Path 2: explicit instance name argument (Karabiner passes e.g. `hr`).
if [[ -n "${1:-}" ]]; then
  SESSION="$1"
  # Launcher convention: instance <name> lives under ~/.herdr-<name>.
  CFG="$HOME/.herdr-${SESSION}/config"
fi

if [[ -z "$SESSION" || -z "$CFG" ]]; then
  log "FATAL: could not resolve target instance (need HERDR_SESSION+XDG_CONFIG_HOME or an instance-name arg)"
  exit 2
fi

# Safety: never touch prod herdr (~/.config/herdr under the real HOME).
if [[ "$CFG" == "$HOME/.config" || "$CFG" == "$HOME/.config/" ]]; then
  log "REFUSING to restart prod herdr ($CFG). Aborting."
  exit 3
fi

SOCK="$CFG/herdr/sessions/$SESSION/herdr.sock"
log "target: session=$SESSION config=$CFG"
log "socket: $SOCK"

# ---------------------------------------------------------------------------
# 2. Detect the live server: pid, binary, XDG roots, tmux host session.
# ---------------------------------------------------------------------------
# socket_pid (not `lsof -t | head -1`): the server's own CLIENTS hold this socket
# too, so head -1 was a coin flip between the server and whoever connected to it.
# See dash-common.sh.
SERVER_PID="$(socket_pid "$SOCK")"

BIN=""
XDG_STATE=""
XDG_DATA=""
TMUX_HOST=""
GEOM_X=220
GEOM_Y=55

pid_env() { ps eww "$1" 2>/dev/null | tr ' ' '\n' | grep -m1 "^$2=" | cut -d= -f2-; }

if [[ -n "$SERVER_PID" ]]; then
  log "live server pid: $SERVER_PID"
  XDG_STATE="$(pid_env "$SERVER_PID" XDG_STATE_HOME)"
  XDG_DATA="$(pid_env "$SERVER_PID" XDG_DATA_HOME)"
  # tmux host = the pane whose pid is the server's parent (the herdr client).
  CLIENT_PID="$(ps -o ppid= -p "$SERVER_PID" 2>/dev/null | tr -d ' ')"
  if [[ -n "$CLIENT_PID" ]]; then
    TMUX_HOST="$(tmux list-panes -a -F '#{session_name} #{pane_pid} #{pane_width} #{pane_height}' 2>/dev/null \
                 | awk -v p="$CLIENT_PID" '$2==p {print $1; exit}')"
    read -r _s _p GX GY < <(tmux list-panes -a -F '#{session_name} #{pane_pid} #{pane_width} #{pane_height}' 2>/dev/null \
                 | awk -v p="$CLIENT_PID" '$2==p {print; exit}')
    [[ -n "${GX:-}" ]] && GEOM_X="$GX"
    [[ -n "${GY:-}" ]] && GEOM_Y="$GY"
  fi
else
  log "no live server on socket (already dead/hung with stale socket)"
fi

# The binary to relaunch with: the live server's real image (via /proc on Linux,
# `ps -o comm=` on macOS), else the host-injected $HERDR_BIN_PATH, else PATH.
# This used to be `ps -o comm=` with a hardcoded ~/Projects fallback, which on
# Linux yielded the bare string "herdr" — non-empty, so the fallback never fired,
# and not executable, so the relaunch line pointed at nothing.
BIN="$(instance_binary "$SERVER_PID")"

# Fallbacks by convention when the live server couldn't supply a value.
[[ -z "$XDG_STATE"  ]] && XDG_STATE="$HOME/.herdr-${SESSION}/state"
[[ -z "$XDG_DATA"   ]] && XDG_DATA="$HOME/.herdr-${SESSION}/data"
[[ -z "$TMUX_HOST"  ]] && TMUX_HOST="${SESSION}-host"

log "binary: ${BIN:-<unresolved>}"
log "xdg: config=$CFG state=$XDG_STATE data=$XDG_DATA"
log "tmux host: $TMUX_HOST  geom=${GEOM_X}x${GEOM_Y}"

# ---------------------------------------------------------------------------
# 2.5 Drop the restart sentinel BEFORE killing the server.
# ---------------------------------------------------------------------------
# The launcher (~/.local/bin/<session>) runs the client in a self-healing loop:
# when the client exits and finds a FRESH sentinel, it relaunches herdr in its
# OWN pty (repaints in place). Touching it before the kill guarantees every
# attached client sees it on exit, so no terminal is left on "server closed".
SENTINEL="$XDG_STATE/herdr/.restart-request-$SESSION"
mkdir -p "$XDG_STATE/herdr" 2>/dev/null
: > "$SENTINEL" 2>/dev/null && log "sentinel dropped: $SENTINEL" || log "WARN: could not write sentinel $SENTINEL"

# ---------------------------------------------------------------------------
# 3. Fully kill the old instance and verify it is gone (no leak/zombie).
# ---------------------------------------------------------------------------
if [[ -n "$SERVER_PID" ]]; then
  kill "$SERVER_PID" 2>/dev/null
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    kill -0 "$SERVER_PID" 2>/dev/null || break
    sleep 0.2
  done
  if kill -0 "$SERVER_PID" 2>/dev/null; then
    log "graceful kill timed out; sending kill -9"
    kill -9 "$SERVER_PID" 2>/dev/null
    sleep 0.5
  fi
fi

# NOTE: we intentionally DO NOT kill the tmux host session here. Each attached
# client (tmux pane OR a direct terminal like Ghostty) exits when the server
# dies and its self-healing launcher relaunches herdr in place. Killing the
# tmux session would drop an attached client onto a dead shell — the exact bug
# this restart flow is fixing.

# Sweep any straggler process still bound to this instance's socket.
# socket_pids matches the socket's NAME field, so this `kill -9` can only ever
# reach processes actually holding THIS path.
for _ in 1 2 3 4 5; do
  STRAG="$(socket_pids "$SOCK" | tr '\n' ' ')"
  [[ -z "${STRAG// /}" ]] && break
  log "straggler on socket: $STRAG — killing"
  kill -9 $STRAG 2>/dev/null
  sleep 0.3
done

# Verify the old pid is truly gone.
if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
  log "ERROR: old server pid $SERVER_PID still alive after kill"
  exit 4
fi
log "old instance is down"

# ---------------------------------------------------------------------------
# 4. Wait for a self-healing client to relaunch the server IN PLACE.
# ---------------------------------------------------------------------------
# With the sentinel dropped and the server dead, every attached client's
# launcher loop relaunches herdr in its own pty and the first one back brings
# the server up. We just wait for that to happen — no external relaunch, so the
# user's real terminal (Ghostty/tmux) repaints itself.
NEW_PID=""
for _ in $(seq 1 48); do   # ~12s: client exit + settle(0.5) + fresh server boot
  NEW_PID="$(socket_pid "$SOCK")"
  if [[ -n "$NEW_PID" && "$NEW_PID" != "$SERVER_PID" ]]; then
    break
  fi
  sleep 0.25
done

if [[ -n "$NEW_PID" ]]; then
  # Do NOT remove the sentinel here: with several clients attached, one wins the
  # race and brings the server up first, but the OTHERS (incl. the user's
  # visible terminal) may not have exited yet. Removing it now would make the
  # losers find no sentinel and drop to a dead shell. Let it age out via TTL so
  # EVERY attached client relaunches in place.
  log "RESTART OK (self-healed in place): new server pid $NEW_PID (old was ${SERVER_PID:-none})"
  exit 0
fi

# ---------------------------------------------------------------------------
# 5. Last-resort fallback: no attached client self-healed (e.g. the server was
#    running headless with nothing attached). Relaunch into a detached tmux so
#    the instance at least recovers; the user can `tmux attach -t <host>`.
# ---------------------------------------------------------------------------
log "no client self-healed within timeout — falling back to detached tmux relaunch"
LAUNCHER="$HOME/.local/bin/$SESSION"
if [[ -x "$LAUNCHER" ]]; then
  LAUNCH_CMD="$LAUNCHER"
  log "fallback relaunch via launcher: $LAUNCHER"
elif [[ -n "$BIN" ]]; then
  LAUNCH_CMD="env XDG_CONFIG_HOME=$CFG XDG_STATE_HOME=$XDG_STATE XDG_DATA_HOME=$XDG_DATA HERDR_SESSION=$SESSION $BIN --session $SESSION"
  log "fallback relaunch via reconstructed exec line"
else
  # Refuse to build a launch line around an empty/bogus binary path. That is how
  # the old `ps -o comm=` value ("herdr") got here and produced a tmux session
  # running a command that does not exist — indistinguishable from "the button
  # did nothing", with the server already dead.
  log "FATAL: no herdr binary resolved for session $SESSION — cannot relaunch"
  desktop_notify "herdr restart FAILED" \
    "the server for '$SESSION' was stopped but no herdr binary could be resolved to relaunch it"
  exit 6
fi

tmux kill-session -t "$TMUX_HOST" 2>/dev/null
tmux new-session -d -s "$TMUX_HOST" -x "$GEOM_X" -y "$GEOM_Y" "$LAUNCH_CMD"

for _ in $(seq 1 40); do
  NEW_PID="$(socket_pid "$SOCK")"
  if [[ -n "$NEW_PID" && "$NEW_PID" != "$SERVER_PID" ]]; then
    break
  fi
  sleep 0.25
done

if [[ -n "$NEW_PID" ]]; then
  log "RESTART OK (detached-tmux fallback): new server pid $NEW_PID (old was ${SERVER_PID:-none})"
  exit 0
else
  # The server is dead and nothing replaced it. herdr itself is gone, so a herdr
  # toast is impossible — go straight to the desktop notifier.
  log "ERROR: new server did not come up on $SOCK"
  desktop_notify "herdr restart FAILED" \
    "'$SESSION' was stopped but no new server came up on its socket — see $LOG"
  exit 5
fi
