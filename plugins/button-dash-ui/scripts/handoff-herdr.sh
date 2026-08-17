#!/usr/bin/env bash
# handoff-herdr.sh — NON-DESTRUCTIVE server swap for a herdr instance.
#
# The "Handoff hr" button and the global ⌃⌥H (ctrl+option+h) hotkey run this.
# Unlike restart-herdr.sh (⌃⌥X) and full-restart-herdr.sh (⌃⌥⇧X), this kills
# NOTHING. It asks the running server to spawn a replacement from the binary on
# disk and transfer the live pane PTY master fds over a unix socket (SCM_RIGHTS),
# so every `claude`/`cx` pane keeps the SAME pid and every in-flight subagent,
# team and Task survives. Only the server process is swapped underneath them.
#
# Intended flow (herdr-dev §7): build + E2E on herdrtest, `mv` the SAME binary
# into place (never `cp` — cp over a running binary SIGKILLs on macOS), then
# press ⌃⌥H instead of ⌃⌥X.
#
# DESIGN: fail loud, never fall back to a kill. Not killing the agents is the
# entire point of this button; silently degrading to a restart would do exactly
# what the user pressed it to avoid, at the moment they least expect it. On ANY
# failure this script toasts the reason, logs it, changes nothing and exits
# non-zero. `--fallback-restart` is deliberately NOT implemented here.
#
# Target resolution, prod refusal and server detection are copied verbatim from
# restart-herdr.sh; instance identity must not diverge across the restart family.
#
# Usage:
#   handoff-herdr.sh                 # in-app button: instance from inherited env
#   handoff-herdr.sh hr              # explicit instance name (hotkey path)
#   handoff-herdr.sh hr <exe>        # $2 overrides the binary to hand off to
#   HERDR_HANDOFF_IMPORT_EXE=<path>  # same override, via env (button path, since
#                                    # the button passes no args at all)
#
# ($1 is the instance name, per the restart-family convention, so the import-exe
# override is $2 — the spec's "$1 or the env var" predates that collision.)

set -u

# ---------------------------------------------------------------------------
# 0. Self-detach: the action runner is a child of the server being replaced.
# ---------------------------------------------------------------------------
LOG="${HERDR_HANDOFF_LOG:-$HOME/.herdr-handoff.log}"
if [[ "${_HERDR_HANDOFF_DETACHED:-}" != "1" ]]; then
  _HERDR_HANDOFF_DETACHED=1 nohup "$0" "$@" >>"$LOG" 2>&1 < /dev/null &
  exit 0
fi

log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*"; }

# This script was written on macOS and is now also run on Linux (the box). The
# four places where the two genuinely differ are routed through the helpers
# below rather than sprinkled inline, so the divergence stays in one block.
UNAME_S="$(uname -s 2>/dev/null || echo unknown)"

# Inode of a file. BSD/macOS stat is `-f %i`; GNU/Linux stat is `-c %i`.
file_inode() {
  case "$UNAME_S" in
    Darwin) stat -f %i "$1" 2>/dev/null ;;
    *)      stat -c %i "$1" 2>/dev/null ;;
  esac
}

# ABSOLUTE path of the executable a pid is running.
#
# The original used `ps -o comm=`, which is correct ONLY on macOS: there `comm`
# is the full executable path. On Linux `comm` is the bare 15-char process name
# (`herdr-bridge`), so every downstream use — the -f/-x gates and the exec at
# the end — was handed a name that resolves to nothing. That was the bug that
# made this button dead on this box.
#
# On Linux /proc/<pid>/exe is the ground truth. When the binary has been
# replaced underneath the running server — which is the ENTIRE intended flow of
# this button (`mv` a fresh build into place, then press it) — the kernel
# appends " (deleted)" to that link. Stripping the suffix yields the path of the
# NEW on-disk file, which is exactly the image we want to hand off to.
exe_path() {
  local pid="$1" p=""
  if [[ -r "/proc/$pid/exe" ]]; then
    p="$(readlink "/proc/$pid/exe" 2>/dev/null)"
    p="${p% (deleted)}"
  fi
  [[ -n "$p" ]] || p="$(ps -o comm= -p "$pid" 2>/dev/null | sed 's/ server$//')"
  printf '%s' "$p"
}

# Pid of the server LISTENING on a unix socket path.
#
# `lsof -t <sock> | head -1` is not safe on Linux: lsof also reports processes
# holding OTHER unix sockets, so the -t list is polluted and the first entry is
# whichever unrelated process sorts first (measured here: gvfsd-trash, pid
# 163293, whose socket is /run/user/1000/gvfsd/socket-*). Picking that pid would
# sail through the "server alive" gate and then hand off against a random
# process. Match the NAME field against the socket path instead, and prefer the
# LISTEN row — the listener is the server; the CONNECTED rows are its clients.
socket_pid() {
  local sock="$1" out=""
  out="$(lsof -- "$sock" 2>/dev/null)" || true
  [[ -n "$out" ]] || return 0
  printf '%s\n' "$out" \
    | awk -v s="$sock" '
        { for (i = 1; i <= NF; i++) if ($i == s) {
            if (index($0, "LISTEN")) { listen = $2 } else if (!any) { any = $2 }
            break
          } }
        END { print (listen ? listen : any) }'
}

# Resolve a herdr CLI for toasts/status. PATH LAST: `herdr` on PATH may be a
# different instance's shim, and the promoted per-instance binaries are the
# authoritative ones.
HERDR=""
for cand in \
  "${HERDR_BIN:-}" \
  "$HOME/.herdr-hr/bin/herdr" \
  "$HOME/Projects/herdr-worktrees/imgapi-127/target/release/herdr" \
  "$HOME/Projects/herdr/target/release/herdr" \
  "$(command -v herdr 2>/dev/null)"
do
  [[ -n "$cand" && -x "$cand" ]] && { HERDR="$cand"; break; }
done

# Toast, with a macOS fallback when herdr swallows it.
#
# `herdr notification show` answers {"shown":false,"reason":"disabled"} whenever
# `[ui.toast] delivery` is "off" — which is exactly what hr's own config.toml
# sets. A fail-loud button whose failure message is silently dropped is the
# cardinal silent-failure sin, and this button's whole contract is that the user
# LEARNS the handoff did not happen. So: check `shown`, and if herdr did not
# display it, raise it through Notification Center instead. We deliberately do
# not touch the user's herdr config to fix this.
toast() {
  local title="$1" body="${2:-}" out=""
  log "TOAST: $title — $body"
  if [[ -n "$HERDR" ]]; then
    out="$("$HERDR" notification show "$title" ${body:+--body "$body"} 2>&1)"
    printf '%s\n' "$out" >>"$LOG"
    if printf '%s' "$out" | grep -q '"shown":[[:space:]]*true'; then
      return 0
    fi
    log "herdr toast not shown (delivery off/busy) — falling back to Notification Center"
  fi
  # Desktop-notification fallback. This is not cosmetic: hr's config sets
  # [ui.toast] delivery = "off", so EVERY message this button emits — including
  # "Handoff FAILED" — reaches the user only through this path. On Linux the
  # osascript call could never run, which is why the button read as "does
  # nothing" instead of "failed for this reason".
  case "$UNAME_S" in
    Darwin)
      # AppleScript string literals: escape backslashes first, then double quotes.
      local t b
      t="$(printf '%s' "$title" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')"
      b="$(printf '%s' "$body"  | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')"
      osascript -e "display notification \"$b\" with title \"$t\"" >>"$LOG" 2>&1 || \
        log "WARN: osascript fallback also failed — this message reached the log ONLY"
      ;;
    *)
      # notify-send needs a display to talk to; the action runner inherits the
      # server's env, which has one. Failures are logged, never fatal.
      if command -v notify-send >/dev/null 2>&1; then
        notify-send -a herdr -u critical -- "$title" "$body" >>"$LOG" 2>&1 || \
          log "WARN: notify-send fallback failed — this message reached the log ONLY"
      else
        log "WARN: no notify-send on PATH — this message reached the log ONLY"
      fi
      ;;
  esac
}

# Every failure toast states that the agents are still alive — that is the fact
# the user most needs at that moment.
die() {
  toast "Handoff FAILED — nothing changed" \
        "$1 Your agents are still running. Press ⌃⌥X only if you accept losing them."
  exit "$2"
}

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
  die "Could not resolve which herdr instance to hand off." 2
fi

# Safety: never touch prod herdr (~/.config/herdr under the real HOME).
if [[ "$CFG" == "$HOME/.config" || "$CFG" == "$HOME/.config/" ]]; then
  log "REFUSING to hand off prod herdr ($CFG). Aborting."
  toast "Handoff refused" "Target is prod herdr ($CFG). Refusing. Nothing was changed."
  exit 3
fi

SOCK="$CFG/herdr/sessions/$SESSION/herdr.sock"

# Escape hatch for instances that are NOT on the session-socket convention.
# Env-only and deliberately unreachable from the button, which passes no env.
# It exists because a herdr launched with another instance's XDG vars inherited
# (e.g. started from inside an hr pane) ends up holding that instance's GLOBAL
# socket, $CFG/herdr/herdr.sock, with an empty HERDR_SESSION — so neither
# resolution path above can name it. The prod refusal above still applies, and
# the resolved socket is re-checked against prod below, so this cannot be used
# to reach ~/.config.
if [[ -n "${HERDR_HANDOFF_SOCKET:-}" ]]; then
  SOCK="$HERDR_HANDOFF_SOCKET"
  log "SOCKET OVERRIDE in effect: $SOCK"
  case "$SOCK" in
    "$HOME/.config"/*)
      log "REFUSING: override points into prod herdr. Aborting."
      toast "Handoff refused" "Socket override points at prod herdr. Refusing. Nothing was changed."
      exit 3
      ;;
  esac
fi

log "target: session=$SESSION config=$CFG"
log "socket: $SOCK"

# Pin the socket for EVERY herdr call below. This is not belt-and-braces: the
# bare CLI defaults to the GLOBAL socket ($CFG/herdr/herdr.sock), which on this
# machine is a DIFFERENT live server (herdrtest) from the session socket. An
# unpinned call would hand off the wrong instance while the user believes their
# agents are being preserved.
export HERDR_SOCKET_PATH="$SOCK"
unset HERDR_CLIENT_SOCKET_PATH 2>/dev/null || true

# ---------------------------------------------------------------------------
# 2. Detect the live server: pid, binary.
# ---------------------------------------------------------------------------
SERVER_PID="$(socket_pid "$SOCK")"

BIN=""
pid_env() { ps eww "$1" 2>/dev/null | tr ' ' '\n' | grep -m1 "^$2=" | cut -d= -f2-; }

# --- Gate 1: server alive -------------------------------------------------
if [[ -z "$SERVER_PID" ]]; then
  log "no live server on socket $SOCK"
  toast "Handoff refused" \
        "No live server on $SESSION (stale socket) — there is nothing to hand off. Nothing was changed. Use ⌃⌥X to start it."
  exit 4
fi
log "live server pid: $SERVER_PID"
BIN="$(exe_path "$SERVER_PID")"
[[ -n "$BIN" && -x "$BIN" ]] || BIN="$HOME/Projects/herdr/target/release/herdr"
log "running server binary: $BIN"

# --- Gate 2: handoff supported -------------------------------------------
STATUS_JSON="$("$HERDR" status server --json 2>>"$LOG")"
if [[ -z "$STATUS_JSON" ]]; then
  die "Could not read server status on $SESSION." 5
fi
SUPPORTED="$(printf '%s' "$STATUS_JSON" | jq -r '.. | .live_handoff? // empty' 2>/dev/null | head -1)"
STATUS_SOCK="$(printf '%s' "$STATUS_JSON" | jq -r '.. | .socket? // empty' 2>/dev/null | head -1)"
log "status: live_handoff=$SUPPORTED socket=$STATUS_SOCK"
# Paranoia: prove the status we just read came from the instance we targeted.
if [[ -n "$STATUS_SOCK" && "$STATUS_SOCK" != "$SOCK" ]]; then
  die "Server status came from $STATUS_SOCK, not $SOCK — refusing to hand off the wrong instance." 6
fi
if [[ "$SUPPORTED" != "true" ]]; then
  toast "Handoff unavailable" \
        "This server does not advertise live_handoff. Nothing was changed and your agents are still running. Use ⌃⌥X only if you accept losing them."
  exit 7
fi

# --- Gate 3: import binary resolvable ------------------------------------
IMPORT_EXE="${HERDR_HANDOFF_IMPORT_EXE:-}"
[[ -n "${2:-}" ]] && IMPORT_EXE="$2"
[[ -n "$IMPORT_EXE" ]] || IMPORT_EXE="$BIN"
if [[ ! -f "$IMPORT_EXE" ]]; then
  die "Import binary $IMPORT_EXE does not exist (or is not a regular file)." 8
fi
if [[ ! -x "$IMPORT_EXE" ]]; then
  die "Import binary $IMPORT_EXE is not executable." 9
fi
log "import exe: $IMPORT_EXE"

# --- Gate 3.5: refuse a no-op handoff --------------------------------------
# A handoff to the image already executing succeeds mechanically and toasts
# success, but changes nothing — measured 2026-08-14: the button was pressed
# before a new build was promoted to disk, the swap "worked", and the missing
# fix read as "the handoff button is broken". The running image is
# /proc/<pid>/exe (possibly an unlinked inode after a mv); compare inodes.
RUNNING_INO=""
if [[ -r "/proc/$SERVER_PID/exe" ]]; then
  RUNNING_INO="$(stat -L -c %i "/proc/$SERVER_PID/exe" 2>/dev/null)"
elif [[ "$UNAME_S" == "Darwin" ]]; then
  RUNNING_INO="$(lsof -p "$SERVER_PID" -a -d txt 2>/dev/null | awk 'NR>1{print $(NF-1)}' | head -1)"
fi
IMPORT_INO="$(file_inode "$IMPORT_EXE")"
log "no-op check: running_ino=$RUNNING_INO import_ino=$IMPORT_INO"
if [[ -n "$RUNNING_INO" && -n "$IMPORT_INO" && "$RUNNING_INO" == "$IMPORT_INO" ]]; then
  toast "Handoff skipped — already live on this binary" \
        "$IMPORT_EXE is the image the server is already running. Promote a new build to disk first, then press again. Nothing was changed."
  exit 16
fi

# --- Gate 4: pane count within the FD ceiling ----------------------------
# MAX_FDS_PER_HANDOFF = 64 (src/server/handoff.rs). Refuse up front rather than
# discovering it mid-transfer with live agents attached.
PANE_JSON="$("$HERDR" pane list 2>>"$LOG")"
PANES="$(printf '%s' "$PANE_JSON" | jq -r '.result.panes | length' 2>/dev/null)"
if [[ -z "$PANES" || "$PANES" == "null" ]]; then
  die "Could not count panes on $SESSION." 10
fi
log "pane count: $PANES"
if (( PANES > 64 )); then
  toast "Handoff refused" \
        "$PANES panes exceeds the 64-fd handoff ceiling. Nothing was changed and your agents are still running. Use ⌃⌥X only if you accept losing them."
  exit 11
fi

# ---------------------------------------------------------------------------
# 2.5 Drop the restart sentinel BEFORE the handoff, so attached CLIENTS reattach.
# ---------------------------------------------------------------------------
# Measured, not assumed: the attached client does NOT survive a live handoff. It
# exits with "server shut down: live update in progress; reconnect after handoff
# completes". The pane PROCESSES survive (that is the whole point and it is
# proven), but the human's terminal falls out of herdr.
#
# The launcher (~/.local/bin/<session>) already solves exactly this for the
# restart buttons: on client exit it relaunches herdr IN THE SAME PTY iff it
# finds a sentinel touched within RESTART_TTL (20s). A handoff drops no sentinel,
# so without this the user would be left staring at a dead shell — the precise
# bug the restart flow exists to avoid. Same file, same convention as
# restart-herdr.sh §2.5.
#
# This kills nothing; it only tells an exiting client to come back.
XDG_STATE="$(pid_env "$SERVER_PID" XDG_STATE_HOME)"
[[ -n "$XDG_STATE" ]] || XDG_STATE="$HOME/.herdr-${SESSION}/state"
SENTINEL="$XDG_STATE/herdr/.restart-request-$SESSION"
mkdir -p "$XDG_STATE/herdr" 2>/dev/null
if : > "$SENTINEL" 2>/dev/null; then
  log "sentinel dropped: $SENTINEL"
else
  SENTINEL=""
  log "WARN: could not write sentinel under $XDG_STATE/herdr — an attached client may not reattach"
fi

# ---------------------------------------------------------------------------
# 3. The handoff itself.
# ---------------------------------------------------------------------------
# READY_TIMEOUT and COMMIT_TIMEOUT are both 30s server-side; do NOT impose a
# shorter script timeout and do not race the server's own rollback
# (cleanup_failed_import_child kills the half-started import server and keeps
# the old one running, so a failed handoff is a no-op).
log "handing off: $BIN server live-handoff --import-exe $IMPORT_EXE"
OUT="$("$BIN" server live-handoff --import-exe "$IMPORT_EXE" 2>&1)"
RC=$?
log "live-handoff rc=$RC output: $OUT"

if [[ $RC -ne 0 ]]; then
  # Nothing was swapped, so no client will exit and none should relaunch. Clear
  # the sentinel rather than leaving a live one armed for its 20s TTL.
  # (On SUCCESS we deliberately leave it, exactly as restart-herdr.sh does: with
  # several clients attached, one wins the race and the losers still need to find
  # it. Let it age out via TTL so EVERY attached client comes back.)
  [[ -n "$SENTINEL" ]] && rm -f "$SENTINEL" 2>/dev/null
  # The CLI reports failures as JSON ({"error":{"code":...,"message":...}}).
  # Show the message, not the envelope — a toast is read in one glance.
  REASON="$(printf '%s' "$OUT" | jq -r '.error.message // empty' 2>/dev/null | head -1)"
  [[ -n "$REASON" ]] || REASON="$(printf '%s' "$OUT" | tail -3 | tr '\n' ' ')"
  die "${REASON:-live-handoff exited $RC}." 12
fi

# Confirm a new server actually owns the socket, and report the version applied.
NEW_PID=""
for _ in $(seq 1 40); do
  NEW_PID="$(socket_pid "$SOCK")"
  [[ -n "$NEW_PID" && "$NEW_PID" != "$SERVER_PID" ]] && break
  sleep 0.25
done

if [[ -z "$NEW_PID" ]]; then
  die "live-handoff reported success but no server owns $SOCK." 13
fi
if [[ "$NEW_PID" == "$SERVER_PID" ]]; then
  die "live-handoff reported success but the server pid is unchanged ($SERVER_PID)." 14
fi

# --- Did the swap actually take? Compare INODES, not versions. -------------
# `herdr --version` and version-dash are both FALSE GREENS here: --version
# executes the ON-DISK binary and reports the promoted build no matter what is
# actually running, and version-dash's +N is derived from the git source, so it
# shows the same number whichever image is executing. A handoff that silently
# kept the old image is indistinguishable from a real one by either.
#
# The executing image is the only ground truth: the inode the new server has
# mapped as its text segment must equal the inode of the file we imported.
DISK_INO="$(file_inode "$IMPORT_EXE")"
MAPPED_INO="$(lsof -p "$NEW_PID" -a -d txt 2>/dev/null | awk 'NR>1{print $(NF-1)}' | head -1)"
# Linux: /proc is authoritative and needs no lsof. Note the running image may be
# an unlinked inode after a `mv`, which is precisely the mismatch this gate is
# built to catch, so resolve it via the pid's own view, not the path.
if [[ -z "$MAPPED_INO" && -r "/proc/$NEW_PID/exe" ]]; then
  MAPPED_INO="$(stat -L -c %i "/proc/$NEW_PID/exe" 2>/dev/null)"
fi
log "inode check: import_exe=$DISK_INO mapped_by_$NEW_PID=$MAPPED_INO"
if [[ -z "$MAPPED_INO" || -z "$DISK_INO" ]]; then
  log "WARN: could not read both inodes — swap NOT verified"
elif [[ "$MAPPED_INO" != "$DISK_INO" ]]; then
  # Panes are fine, but the point of the button was not achieved.
  toast "Handoff applied the WRONG image" \
        "New server $NEW_PID is running inode $MAPPED_INO, not $DISK_INO from $IMPORT_EXE. Your panes and agents are still running, but the new binary is NOT live."
  exit 15
fi

# --- Defense-in-depth: nudge the wasm UI plugin layers. --------------------
# HISTORY: a live handoff used to drop the layer set (measured on herdrtest,
# `render_ui_plugin_layers layers=N` fell from 8 to 3 — the dash lost
# button-dash, version-dash and image-view while usage-dash survived, with no
# error in any log). That was a HOST defect: the imported server rebuilt its
# plugin registry but never rebuilt the UiPluginManager from it.
#
# FIXED IN THE HOST by herdr 4aaf75bc ("fix: rebuild the ui plugin layer set on
# handoff import"), verified on herdrtest: 84 consecutive post-import frames at
# the normal layers=10 with this cycling never running, and every plugin visible
# in the band. So on a current binary this block is redundant.
#
# It stays because the script has to survive a handoff onto an OLDER image too —
# import_exe is whatever the user points at, and downgrading to a pre-4aaf75bc
# binary brings the defect back. Cycling any one plugin's enabled state rebuilds
# the whole layer set, and it is a no-op when the layers are already correct.
#
# Best-effort only — never fail the handoff over this, the swap already worked.
CYCLE_ID="$(printf '%s' "$("$HERDR" plugin list 2>>"$LOG")" \
  | awk '/^- .* enabled /{print $2; exit}')"
if [[ -n "$CYCLE_ID" ]]; then
  log "nudging plugin layers by cycling $CYCLE_ID (redundant on a host with 4aaf75bc)"
  "$HERDR" plugin disable "$CYCLE_ID" >>"$LOG" 2>&1 \
    && sleep 1 \
    && "$HERDR" plugin enable "$CYCLE_ID" >>"$LOG" 2>&1 \
    && log "plugin layers nudged" \
    || log "WARN: could not cycle $CYCLE_ID — harmless on a current host, but on a pre-4aaf75bc image the dash may be missing plugins until a restart"
else
  log "WARN: no enabled plugin found to cycle — harmless on a current host, but on a pre-4aaf75bc image the dash may be missing plugins until a restart"
fi

NEW_VER="$("$HERDR" status server --json 2>>"$LOG" | jq -r '.. | .version? // empty' 2>/dev/null | head -1)"
log "HANDOFF OK: server pid $SERVER_PID -> $NEW_PID, version ${NEW_VER:-unknown}, image inode $MAPPED_INO"
toast "hr handed off" \
      "New server live on ${NEW_VER:-the new binary} (pid $NEW_PID). Panes and agents kept running — nothing was restarted."
exit 0
