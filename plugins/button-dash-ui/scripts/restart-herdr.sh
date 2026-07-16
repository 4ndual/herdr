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
SERVER_PID="$(lsof -t "$SOCK" 2>/dev/null | head -1)"

BIN=""
XDG_STATE=""
XDG_DATA=""
TMUX_HOST=""
GEOM_X=220
GEOM_Y=55

pid_env() { ps eww "$1" 2>/dev/null | tr ' ' '\n' | grep -m1 "^$2=" | cut -d= -f2-; }

if [[ -n "$SERVER_PID" ]]; then
  log "live server pid: $SERVER_PID"
  BIN="$(ps -o comm= -p "$SERVER_PID" 2>/dev/null | sed 's/ server$//')"
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

# Fallbacks by convention when the live server couldn't supply a value.
[[ -z "$BIN"        ]] && BIN="$HOME/Projects/herdr/target/release/herdr"
[[ -z "$XDG_STATE"  ]] && XDG_STATE="$HOME/.herdr-${SESSION}/state"
[[ -z "$XDG_DATA"   ]] && XDG_DATA="$HOME/.herdr-${SESSION}/data"
[[ -z "$TMUX_HOST"  ]] && TMUX_HOST="${SESSION}-host"

log "binary: $BIN"
log "xdg: config=$CFG state=$XDG_STATE data=$XDG_DATA"
log "tmux host: $TMUX_HOST  geom=${GEOM_X}x${GEOM_Y}"

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

# Kill the old tmux host session (drops the old client).
tmux kill-session -t "$TMUX_HOST" 2>/dev/null && log "killed old tmux session $TMUX_HOST"

# Sweep any straggler process still bound to this instance's socket.
for _ in 1 2 3 4 5; do
  STRAG="$(lsof -t "$SOCK" 2>/dev/null)"
  [[ -z "$STRAG" ]] && break
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
# 4. Relaunch FRESH on same binary + session + XDG roots.
# ---------------------------------------------------------------------------
# Prefer the user's launcher if one exists (day-to-day path for `hr`); it sets
# the exact same XDG roots/session. Otherwise reconstruct the exec line.
LAUNCHER="$HOME/.local/bin/$SESSION"
if [[ -x "$LAUNCHER" ]]; then
  LAUNCH_CMD="$LAUNCHER"
  log "relaunch via launcher: $LAUNCHER"
else
  LAUNCH_CMD="env XDG_CONFIG_HOME=$CFG XDG_STATE_HOME=$XDG_STATE XDG_DATA_HOME=$XDG_DATA HERDR_SESSION=$SESSION $BIN --session $SESSION"
  log "relaunch via reconstructed exec line"
fi

tmux new-session -d -s "$TMUX_HOST" -x "$GEOM_X" -y "$GEOM_Y" "$LAUNCH_CMD"

# ---------------------------------------------------------------------------
# 5. Wait for the fresh server to come up on the socket.
# ---------------------------------------------------------------------------
NEW_PID=""
for _ in $(seq 1 40); do
  NEW_PID="$(lsof -t "$SOCK" 2>/dev/null | head -1)"
  if [[ -n "$NEW_PID" && "$NEW_PID" != "$SERVER_PID" ]]; then
    break
  fi
  sleep 0.25
done

if [[ -n "$NEW_PID" ]]; then
  log "RESTART OK: new server pid $NEW_PID (old was ${SERVER_PID:-none})"
  exit 0
else
  log "ERROR: new server did not come up on $SOCK"
  exit 5
fi
