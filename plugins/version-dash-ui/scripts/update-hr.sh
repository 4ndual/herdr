#!/bin/bash
# update-hr.sh — promote the EXACT build currently running in herdrtest into the
# hr instance (right-click "hr" in the version-dash strip).
#
# Staged promotion, enforced: hr may ONLY update to the exact build that is
# running in herdrtest. The SOURCE is always the herdrtest binary — there is no
# path from arbitrary builds to hr. (This is the second, more conservative stage
# of the pipeline: build → herdrtest → hr.)
#
# hr's live exec path is RESOLVED DYNAMICALLY, not assumed: the running hr server
# may exec `~/Projects/herdr/target/release/herdr` directly (an old launcher loop
# can keep winning the restart race) rather than the `~/.herdr-hr/bin` snapshot.
# So we resolve the live server's actual executable (hr socket → pid → exe) and
# swap THAT, and also swap the snapshot path when it exists, so hr runs the
# promoted build regardless of which path a future restart picks up. Each swap is
# atomic same-volume staging → mv (never rewrite a running inode → no SIGKILL).
#
# Pipeline:
#   1. resolve the herdrtest SOURCE binary + verify it answers `--version`;
#   2. resolve every hr exec target (live server exe + snapshot), dedup;
#   3. atomic-swap the source into each target on its own volume;
#   4. restart hr via its self-heal sentinel (only when VD_DO_RESTART=1).
#
# SAFETY: swaps are atomic (old inode stays alive for the running process). The
# RESTART is gated behind VD_DO_RESTART=1; live wiring gated on team-lead ack.
#
# Env overrides (all optional — sandbox E2E points these at throwaway clones):
#   VD_TEST_BIN    herdrtest SOURCE binary       (default ~/.local/bin/herdrtest)
#   VD_HR_ROOT     hr XDG root                   (default ~/.herdr-hr)
#   VD_HR_SESSION  hr session name               (default hr)
#   VD_HR_SOCK     hr server socket for live-exe (default ~/.herdr-hr/config/herdr/sessions/hr/herdr.sock)
#   VD_HR_TARGETS  explicit space-separated exec paths to swap (skips resolution; for sandbox)
#   VD_RESTART_SCRIPT  sanctioned hr restart script (default button-dash restart-herdr.sh)
#   VD_DO_RESTART  "1" to actually restart       (default off: swap only)
set -euo pipefail

VD_TEST_BIN="${VD_TEST_BIN:-$HOME/.local/bin/herdrtest}"
VD_HR_ROOT="${VD_HR_ROOT:-$HOME/.herdr-hr}"
VD_HR_SESSION="${VD_HR_SESSION:-hr}"
VD_HR_SOCK="${VD_HR_SOCK:-$VD_HR_ROOT/config/herdr/sessions/$VD_HR_SESSION/herdr.sock}"
VD_DO_RESTART="${VD_DO_RESTART:-0}"

# The ONLY sanctioned hr restart is button-dash's restart-herdr.sh (sentinel
# self-heal so every attached client relaunches in place). Resolve it relative to
# this plugin's dir; override for sandbox.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
VD_RESTART_SCRIPT="${VD_RESTART_SCRIPT:-$SCRIPT_DIR/../../button-dash-ui/scripts/restart-herdr.sh}"

SNAPSHOT_BIN="$VD_HR_ROOT/bin/herdr"

log() { printf '» %s\n' "$*"; }
die() { printf '✗ %s\n' "$*" >&2; exit 1; }

# ENFORCEMENT: source is always the herdrtest binary — hr can take no other build.
[[ -x "$VD_TEST_BIN" ]] || die "herdrtest source binary not found/executable: $VD_TEST_BIN"

log "resolving herdrtest source build"
SRC_VER="$("$VD_TEST_BIN" --version 2>/dev/null)" || die "herdrtest binary won't report --version — refusing to promote a broken build"
log "herdrtest build: $SRC_VER"

# Resolve the LIVE hr executable from the server holding the hr socket open.
live_hr_exe() {
  local pid exe
  pid="$(lsof -t "$VD_HR_SOCK" 2>/dev/null | head -1 || true)"
  [[ -n "$pid" ]] || return 0
  exe="$(ps -p "$pid" -o comm= 2>/dev/null | sed 's/^ *//;s/ *$//')"
  [[ -n "$exe" && -f "$exe" && "$(basename "$exe")" == "herdr" ]] && printf '%s\n' "$exe"
}

# Build the set of exec paths to swap: explicit override, else live exe + snapshot.
TARGETS=()
if [[ -n "${VD_HR_TARGETS:-}" ]]; then
  read -r -a TARGETS <<<"$VD_HR_TARGETS"
else
  LIVE="$(live_hr_exe || true)"
  [[ -n "$LIVE" ]] && TARGETS+=("$LIVE")
  [[ -f "$SNAPSHOT_BIN" || -d "$(dirname "$SNAPSHOT_BIN")" ]] && TARGETS+=("$SNAPSHOT_BIN")
fi
[[ ${#TARGETS[@]} -gt 0 ]] || die "no hr exec target resolved (live server exe + snapshot both absent)"

# Dedup while preserving order.
DEDUP=()
for t in "${TARGETS[@]}"; do
  skip=0
  for d in "${DEDUP[@]:-}"; do [[ "$d" == "$t" ]] && skip=1 && break; done
  [[ $skip == 0 ]] && DEDUP+=("$t")
done

swap_into() {
  local dest="$1" dir stage
  dir="$(dirname "$dest")"
  mkdir -p "$dir"
  # Stage on the SAME volume as the destination, then atomic mv.
  stage="$dir/.herdr.new.$$"
  cp "$VD_TEST_BIN" "$stage"
  chmod +x "$stage"
  touch "$stage"   # newest mtime so a snapshot_bin re-copy keeps our build
  mv -f "$stage" "$dest"
  log "promoted → $dest ($("$dest" --version 2>/dev/null))"
}

for dest in "${DEDUP[@]}"; do
  swap_into "$dest"
done

if [[ "$VD_DO_RESTART" == "1" ]]; then
  # Do NOT kill hr directly — defer to the sanctioned sentinel self-heal script
  # so every attached client (incl. Andres's visible terminal) relaunches in
  # place on the new binary. It self-detaches, so this returns immediately.
  [[ -x "$VD_RESTART_SCRIPT" ]] || die "sanctioned hr restart script not found: $VD_RESTART_SCRIPT"
  log "invoking sanctioned hr restart: $VD_RESTART_SCRIPT $VD_HR_SESSION"
  "$VD_RESTART_SCRIPT" "$VD_HR_SESSION"
  log "hr restart requested"
else
  log "binary swapped; restart skipped (set VD_DO_RESTART=1 to restart hr)"
fi
log "done"
