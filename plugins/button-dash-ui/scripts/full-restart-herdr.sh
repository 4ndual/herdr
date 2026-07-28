#!/usr/bin/env bash
# full-restart-herdr.sh — HARD, instance-agnostic full restart of a herdr instance.
#
# Sibling of (but distinct from) restart-herdr.sh. The soft restart only kills the
# herdr SERVER and its tmux host, then relaunches; it may leave the Claude Code /
# node processes that were running inside the panes alive (orphaned, still holding
# RAM). This FULL restart is for the case where the RAM hog is NOT herdr itself but
# the Claude Code agents (and their node children) running inside the panes.
#
# It:
#   1. Resolves the target instance dynamically (env from inside herdr, or an
#      instance-name arg) — identical resolution to restart-herdr.sh.
#   2. Enumerates the ENTIRE process subtree of the herdr server (every claude /
#      node / child process in the panes) plus the tmux host session's pane pids
#      and their descendants, capturing pid + RSS BEFORE killing.
#   3. HARD kills that whole subtree (TERM then KILL), the tmux host session, and
#      the herdr server, then VERIFIES every enumerated pid is gone (proving the
#      RAM is actually reclaimed — no leaked/zombie agents left behind).
#   4. Relaunches FRESH on the SAME binary / session / XDG roots (launcher if
#      present, else reconstructed exec line) in a fresh tmux session, so herdr
#      restores its persisted panes and each pane's Claude conversation --resumes.
#
# Nothing is hardcoded: pid, socket, binary, XDG roots, tmux host and the child
# process set are all detected dynamically from the live server / process tree.
#
# Self-detaches (nohup + guarded re-exec) so it survives killing the very herdr
# server it was launched under (the in-app button path).

set -u

# ---------------------------------------------------------------------------
# 0. Self-detach: survive the death of the herdr server that spawned us.
# ---------------------------------------------------------------------------
LOG="${HERDR_FULL_RESTART_LOG:-$HOME/.herdr-full-restart.log}"
if [[ "${_HERDR_FULL_RESTART_DETACHED:-}" != "1" ]]; then
  _HERDR_FULL_RESTART_DETACHED=1 nohup "$0" "$@" >>"$LOG" 2>&1 < /dev/null &
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
  CFG="$HOME/.herdr-${SESSION}/config"
fi

if [[ -z "$SESSION" || -z "$CFG" ]]; then
  log "FATAL: could not resolve target instance (need HERDR_SESSION+XDG_CONFIG_HOME or an instance-name arg)"
  exit 2
fi

# Safety: never touch prod herdr (~/.config/herdr under the real HOME).
if [[ "$CFG" == "$HOME/.config" || "$CFG" == "$HOME/.config/" ]]; then
  log "REFUSING to full-restart prod herdr ($CFG). Aborting."
  exit 3
fi

SOCK="$CFG/herdr/sessions/$SESSION/herdr.sock"
log "=== FULL (HARD) restart ==="
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

CLIENT_PID=""
if [[ -n "$SERVER_PID" ]]; then
  log "live server pid: $SERVER_PID"
  BIN="$(ps -o comm= -p "$SERVER_PID" 2>/dev/null | sed 's/ server$//')"
  XDG_STATE="$(pid_env "$SERVER_PID" XDG_STATE_HOME)"
  XDG_DATA="$(pid_env "$SERVER_PID" XDG_DATA_HOME)"
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
# 2b. Enumerate the full process subtree to hard-kill.
#     Roots: the herdr server pid, plus every pane pid of the tmux host session.
#     We BFS the ppid graph from a single ps snapshot (dynamic, no hardcoding).
# ---------------------------------------------------------------------------
PS_SNAP="$(ps -axo pid=,ppid=,rss=,comm= 2>/dev/null)"

# children_of PID -> prints immediate child pids from the snapshot.
children_of() { awk -v pp="$1" '$2==pp {print $1}' <<<"$PS_SNAP"; }

# collect_subtree PID... -> prints the given roots + all descendants (unique).
# bash 3.2 safe (macOS /bin/bash): no associative arrays — pids are positive
# integers, so a plain indexed array keyed by pid works as a membership set.
collect_subtree() {
  local queue=("$@") seen=() cur kid
  local mark=()
  while ((${#queue[@]})); do
    cur="${queue[0]}"; queue=("${queue[@]:1}")
    [[ -z "$cur" || -n "${mark[$cur]:-}" ]] && continue
    mark[$cur]=1
    seen+=("$cur")
    while read -r kid; do
      [[ -n "$kid" ]] && queue+=("$kid")
    done < <(children_of "$cur")
  done
  [[ ${#seen[@]} -gt 0 ]] && printf '%s\n' "${seen[@]}"
}

# Roots to reap: the server subtree, and every tmux-host pane subtree.
# PRESERVE the tmux pane pids themselves — those are the thin self-healing
# launcher-loop shells (a few MB). Killing them would tear down the tmux
# session and strand an attached terminal; instead we kill everything BELOW
# them (the herdr client, server, Claude Code, node — the actual RAM), then let
# each launcher shell relaunch herdr in place via the restart sentinel. The RAM
# hog is the agent stack, never the launcher shell or tmux.
ROOTS=()
[[ -n "$SERVER_PID" ]] && ROOTS+=("$SERVER_PID")
PRESERVE=()   # launcher-loop shells to keep alive so they self-heal in place
if [[ -n "$TMUX_HOST" ]]; then
  while read -r ppid; do
    [[ -n "$ppid" ]] && { ROOTS+=("$ppid"); PRESERVE+=("$ppid"); }
  done < <(tmux list-panes -t "$TMUX_HOST" -F '#{pane_pid}' 2>/dev/null)
fi

# Membership set for O(1) preserve lookups (bash 3.2 safe: pids are ints).
preserve_mark=()
for p in "${PRESERVE[@]:-}"; do [[ -n "$p" ]] && preserve_mark[$p]=1; done

VICTIMS=()
if ((${#ROOTS[@]})); then
  while read -r p; do
    [[ -z "$p" ]] && continue
    [[ -n "${preserve_mark[$p]:-}" ]] && continue   # keep the launcher shell
    VICTIMS+=("$p")
  done < <(collect_subtree "${ROOTS[@]}" | sort -un)
fi

# Report the BEFORE picture: pid / rss / comm, plus a rough reclaimable RSS sum.
BEFORE_RSS_KB=0
log "subtree to hard-kill (${#VICTIMS[@]} procs):"
for p in "${VICTIMS[@]}"; do
  line="$(awk -v pid="$p" '$1==pid {rss=$3; $1=$2=$3=""; sub(/^ +/,""); print rss" "$0}' <<<"$PS_SNAP")"
  [[ -z "$line" ]] && continue
  rss="${line%% *}"; comm="${line#* }"
  BEFORE_RSS_KB=$((BEFORE_RSS_KB + rss))
  log "  pid=$p rss=${rss}KB $comm"
done
log "reclaimable RSS (sum, before): $((BEFORE_RSS_KB/1024)) MB across ${#VICTIMS[@]} procs"

# Drop the restart sentinel BEFORE killing, so the preserved launcher shells
# relaunch herdr in place when their client dies (same mechanism as the soft
# restart). Shared path with restart-herdr.sh and the launcher.
SENTINEL="$XDG_STATE/herdr/.restart-request-$SESSION"
mkdir -p "$XDG_STATE/herdr" 2>/dev/null
: > "$SENTINEL" 2>/dev/null && log "sentinel dropped: $SENTINEL" || log "WARN: could not write sentinel $SENTINEL"

# ---------------------------------------------------------------------------
# 3. Hard kill the whole subtree (server + agent stack) — then verify gone.
#    The tmux host session and its launcher shells are intentionally KEPT so
#    they self-heal in place; we never kill the tmux session here.
# ---------------------------------------------------------------------------
# TERM the whole subtree first (children before roots is not required — a fresh
# reap of any survivors follows), give it a moment, then KILL survivors.
if ((${#VICTIMS[@]})); then
  kill -TERM "${VICTIMS[@]}" 2>/dev/null
fi

# Graceful window for the subtree to exit.
for _ in 1 2 3 4 5 6 7 8 9 10; do
  alive=0
  for p in "${VICTIMS[@]}"; do
    kill -0 "$p" 2>/dev/null && { alive=1; break; }
  done
  [[ "$alive" == 0 ]] && break
  sleep 0.2
done

# KILL any survivor of the subtree.
for p in "${VICTIMS[@]}"; do
  if kill -0 "$p" 2>/dev/null; then
    log "survivor after TERM: $p — sending KILL"
    kill -9 "$p" 2>/dev/null
  fi
done

# Sweep any straggler still bound to the socket — but ONLY known-old holders
# (the old server or a subtree victim). A preserved launcher shell may already
# have self-healed a FRESH server onto this socket; that new pid must NOT be
# killed. So we only re-kill pids that were part of the old world.
old_mark=()
for p in "${VICTIMS[@]:-}"; do [[ -n "$p" ]] && old_mark[$p]=1; done
[[ -n "${SERVER_PID:-}" ]] && old_mark[$SERVER_PID]=1
for _ in 1 2 3 4 5; do
  STRAG="$(lsof -t "$SOCK" 2>/dev/null)"
  [[ -z "$STRAG" ]] && break
  killed_any=0
  for sp in $STRAG; do
    if [[ -n "${old_mark[$sp]:-}" ]]; then
      log "old straggler on socket: $sp — killing"
      kill -9 "$sp" 2>/dev/null
      killed_any=1
    fi
  done
  [[ "$killed_any" == 0 ]] && break   # only a fresh self-healed server remains
  sleep 0.3
done

sleep 0.4

# VERIFY: every enumerated victim is gone (RAM reclaimed, no zombie/leak).
LEAKED=()
for p in "${VICTIMS[@]}"; do
  # A reaped process may momentarily be a zombie; kill -0 fails once reaped.
  if kill -0 "$p" 2>/dev/null; then
    # Distinguish a true survivor from a defunct/zombie awaiting reap.
    st="$(ps -o state= -p "$p" 2>/dev/null | tr -d ' ')"
    [[ "$st" == Z* ]] && continue
    LEAKED+=("$p")
  fi
done

if ((${#LEAKED[@]})); then
  log "ERROR: ${#LEAKED[@]} process(es) still alive after hard kill: ${LEAKED[*]}"
  exit 4
fi
log "verified: all ${#VICTIMS[@]} subtree procs gone — ~$((BEFORE_RSS_KB/1024)) MB reclaimed"

if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
  log "ERROR: old server pid $SERVER_PID still alive after kill"
  exit 4
fi
log "old instance is fully down"

# ---------------------------------------------------------------------------
# 4. Wait for a preserved launcher shell to relaunch the server IN PLACE.
# ---------------------------------------------------------------------------
# The sentinel is dropped and the agent stack is dead; each preserved launcher
# shell's `herdr` client exited, so its loop relaunches herdr in its own pty
# and the first one back brings the server up — the user's real terminal
# repaints itself, no detached tmux.
NEW_PID=""
for _ in $(seq 1 48); do   # ~12s: client exit + settle(0.5) + fresh server boot
  NEW_PID="$(lsof -t "$SOCK" 2>/dev/null | head -1)"
  if [[ -n "$NEW_PID" && "$NEW_PID" != "$SERVER_PID" ]]; then
    break
  fi
  sleep 0.25
done

if [[ -n "$NEW_PID" ]]; then
  # Do NOT remove the sentinel (TTL-aged): let every preserved launcher shell
  # relaunch, not just the race winner.
  log "FULL RESTART OK (self-healed in place): new server pid $NEW_PID (old was ${SERVER_PID:-none})"
  exit 0
fi

# ---------------------------------------------------------------------------
# 5. Last-resort fallback: nothing self-healed (e.g. no tmux host / all shells
#    were killed anyway). Relaunch into a detached tmux so the instance recovers.
# ---------------------------------------------------------------------------
log "no launcher shell self-healed within timeout — falling back to detached tmux relaunch"
LAUNCHER="$HOME/.local/bin/$SESSION"
if [[ -x "$LAUNCHER" ]]; then
  LAUNCH_CMD="$LAUNCHER"
  log "fallback relaunch via launcher: $LAUNCHER"
else
  LAUNCH_CMD="env XDG_CONFIG_HOME=$CFG XDG_STATE_HOME=$XDG_STATE XDG_DATA_HOME=$XDG_DATA HERDR_SESSION=$SESSION $BIN --session $SESSION"
  log "fallback relaunch via reconstructed exec line"
fi

tmux kill-session -t "$TMUX_HOST" 2>/dev/null
tmux new-session -d -s "$TMUX_HOST" -x "$GEOM_X" -y "$GEOM_Y" "$LAUNCH_CMD"

for _ in $(seq 1 40); do
  NEW_PID="$(lsof -t "$SOCK" 2>/dev/null | head -1)"
  if [[ -n "$NEW_PID" && "$NEW_PID" != "$SERVER_PID" ]]; then
    break
  fi
  sleep 0.25
done

if [[ -n "$NEW_PID" ]]; then
  log "FULL RESTART OK (detached-tmux fallback): new server pid $NEW_PID (old was ${SERVER_PID:-none})"
  exit 0
else
  log "ERROR: new server did not come up on $SOCK"
  exit 5
fi
