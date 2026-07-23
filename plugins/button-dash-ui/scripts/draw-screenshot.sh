#!/usr/bin/env bash
# draw-screenshot.sh — ZERO-DELAY draw-on-screen + screenshot toggle.
#
# DrawPen stays RESIDENT but FULLY INVISIBLE between sessions — canvas AND
# floating toolbar — appearing ONLY during the draw/shot moment:
#   Press 1: show canvas (⌘⇧A) + toolbar (⌘T) → draw on the live screen. ~0.2s.
#   Press 2: capture (clipboard + ~/Desktop/drawshot-<ts>.png) → hide both. ~0.5s.
#
# Guarantees (backed by DrawPen's config, ~/Library/Application Support/DrawPen/
# config.json — set up previously and relied on here):
#   starts_hidden=true            → a (re)launch never flashes the overlay
#   clear_drawings_on_hide=true   → hiding wipes the canvas → ALWAYS blank next time
#
# DrawPen facts this script encodes (extracted from its app.asar config schema +
# verified live via the window list):
#   Shown  = one fullscreen "DrawPen" window; the toolbar is drawn INSIDE it
#            (⌘T toggles it in-canvas). Hidden = ZERO windows.
#   EXCEPT: a toolbar left in COLLAPSED state becomes a separate floating
#           "Extended Toolbar" window (the pencil widget) that SURVIVES an
#           app-hide. ⌘T can NOT remove it while hidden; re-showing the app
#           reabsorbs it — so the recovery is a fast show→hide bounce.
# ⌘⇧A is a blind toggle, so instead of firing it blind we query the canvas
# window's REAL on-screen state (`keypost visible <App> <Window>`) and toggle
# only when wrong. Externally-toggled state can never drift this script.
#
# Keystrokes are posted by the compiled `keypost` helper (CGEvent, ~40ms) —
# NOT cliclick (~450ms/invocation), which was the old latency hog.
#
# Invoked identically by the herdr button-dash "draw-screenshot" action AND the
# ⌃⌥S Karabiner rule, so button and shortcut behave the same.
set -u

export PATH="/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin:${PATH:-}"
APP="DrawPen"
HERE="$(cd "$(dirname "$0")" && pwd)"
KP="$HERE/keypost"
MARKER="/tmp/drawshot-session"
SHOTDIR="$HOME/Desktop"
LOG="$HOME/.drawshot.log"

log() { printf '%s %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*" >>"$LOG"; }

canvas_visible()  { "$KP" visible "$APP" "$APP"; }
toolbar_visible() { "$KP" visible "$APP" "Extended Toolbar"; }
# DrawPen global ⌘⇧A. keypost exits 3 when the CALLING context lacks
# Accessibility (CGEvent would be silently dropped) — e.g. Karabiner's
# karabiner_console_user_server before it is granted the permission. Log it
# loudly so a "shortcut does nothing" report is diagnosable from ~/.drawshot.log.
toggle_canvas() {
  "$KP" cmd,shift a
  rc=$?
  [ "$rc" -eq 3 ] && log "ERROR: no Accessibility permission in this context — grant it to karabiner_console_user_server (System Settings → Privacy & Security → Accessibility)"
  return "$rc"
}

ensure_running() {
  pgrep -x "$APP" >/dev/null 2>&1 && return 0
  open -g -a "$APP"    # starts_hidden=true → no flash
  for _ in $(seq 1 40); do pgrep -x "$APP" >/dev/null 2>&1 && break; sleep 0.1; done
  sleep 0.8            # one-time: let the global-shortcut listener register
}

# Optional arg1 "region": finish with an interactive drag-selection instead of
# the instant fullscreen shot. The system selection UI draws ABOVE the DrawPen
# overlay, so the drawings are included in the selected area; Esc cancels the
# capture but still hides the overlay. Both modes start a session identically.
MODE="${1:-full}"

if canvas_visible; then
  # ── CAPTURE → HIDE EVERYTHING (hide auto-clears the canvas) ────────────────
  ts="$(date +%Y%m%d-%H%M%S)"
  out="$SHOTDIR/drawshot-$ts.png"
  if [ "$MODE" = "region" ]; then
    # -i: one interactive selection to a file, then mirror it to the clipboard
    # via keypost (screencapture can't target both in a single -i pass).
    screencapture -i -t png "$out"
    [ -s "$out" ] && "$KP" clip "$out"
  else
    screencapture -x -c
    screencapture -x -t png "$out"
  fi
  toggle_canvas
  # If the toolbar was left COLLAPSED it survives the app-hide as a floating
  # "Extended Toolbar" widget. ⌘T can't remove it while hidden — reabsorb it
  # with a fast show→hide bounce (verified live: re-show removes the widget,
  # and the following hide leaves ZERO windows).
  sleep 0.4
  if toolbar_visible; then
    toggle_canvas; sleep 0.4; toggle_canvas
    log "recovered collapsed-toolbar widget via show/hide bounce"
  fi
  rm -f "$MARKER"
  log "CAPTURE -> clipboard + $out ; fully hidden (auto-cleared)"
else
  # ── SHOW canvas (toolbar rides inside it; blank by config) ─────────────────
  ensure_running
  toggle_canvas
  : >"$MARKER"
  log "START drawing (blank canvas, resident overlay)"
fi
