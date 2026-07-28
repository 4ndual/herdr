#!/usr/bin/env bash
# draw-screenshot.sh — draw-on-screen annotation + screenshot, via our OWN
# overlay helper (scripts/draw-overlay), NOT the old third-party DrawPen.
#
# WHY WE OWN THE OVERLAY: DrawPen's overlay windows were plain, non-sticky
# NSWindows bound to their birth Space, so triggering them from a native
# -fullscreen hr window (its own Space) or any second desktop force-switched the
# display to DrawPen's Space and drew on the wrong screen — the "green contour on
# an empty desktop" bug. A window's Space/level attributes can only be set by the
# process that owns it (verified: cross-process CGS space/tag calls are no-ops
# without WindowServer injection), so we stopped driving DrawPen and ship our own
# helper whose window declares exactly what macOS needs to float over ANY Space:
#   collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]  + screenSaver level
# (verify with spacediag: the window reports sticky=true across every Space id).
# DrawPen stays installed but is no longer used by this feature.
#
# TWO-PRESS TOGGLE (unchanged UX): the herdr button and the ⌥⇧5 / ⌥⇧4 Karabiner
# rules invoke this script. First invocation launches the BLANK overlay to draw
# on; a second invocation captures + closes it (via SIGUSR1). While the overlay
# is up, RETURN also captures and ESC cancels — so a single press + Enter works
# too (Andres's requested native-style confirm).
#
# Each launch is a fresh one-shot process → the canvas is ALWAYS blank, with no
# residency and no stale drawings.
#
#   draw-screenshot.sh          full-screen capture on confirm
#   draw-screenshot.sh region   interactive rectangle crop on confirm
set -u

export PATH="/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin:${PATH:-}"
HERE="$(cd "$(dirname "$0")" && pwd)"
HELPER="$HERE/draw-overlay"
APP="draw-overlay"
MODE="${1:-full}"
LOG="$HOME/.drawshot.log"
log() { printf '%s %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*" >>"$LOG"; }

if pgrep -x "$APP" >/dev/null 2>&1; then
  # Overlay already up → this press CONFIRMS the capture.
  pkill -USR1 -x "$APP"
  log "CONFIRM capture (second press) mode=$MODE"
else
  # Launch the blank overlay to draw on. Detached so the button/shortcut returns
  # immediately; the helper owns its own run loop until capture/cancel.
  nohup "$HELPER" "$MODE" >>"$LOG" 2>&1 &
  log "START overlay mode=$MODE"
fi
