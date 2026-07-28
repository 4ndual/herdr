#!/usr/bin/env bash
# open-resume-picker.sh — open the resume-search picker as an IN-APP OVERLAY.
#
# The picker used to open in a new tab / in-place pane; it now opens as a
# floating bordered overlay window (like the settings modal, but bigger) painted
# over the pane area with the rest of the UI still visible. This script only
# resolves the absolute path to the overlay wrapper (which the host cannot know)
# and asks herdr to open the overlay; the host spawns the picker's hidden PTY.
#
# Wired to the button-dash "Resume search" button and the ⌃⌥S hotkey.
set -u

LOG="${HERDR_RESUME_PICKER_LOG:-$HOME/.herdr-resume-picker.log}"
log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG" 2>/dev/null; }

# Locate the overlay wrapper relative to this script (button-dash-ui/scripts ->
# repo root -> plugins/resume-picker/bin).
HERE="$(cd "$(dirname "$0")" && pwd)"
WRAPPER="$(cd "$HERE/../../resume-picker/bin" 2>/dev/null && pwd)/resume-picker-overlay.sh"
if [[ ! -x "$WRAPPER" ]]; then
  log "FATAL: overlay wrapper not found/executable at $WRAPPER"
  exit 3
fi

# Resolve the herdr client binary (same discovery as resume-in-cx.sh).
HERDR=""
for cand in "${HERDR_BIN:-}" "$(command -v herdr 2>/dev/null)" \
  "$HOME/Projects/herdr/target/release/herdr" "$HOME/.local/bin/herdr"; do
  if [[ -n "$cand" && -x "$cand" ]]; then HERDR="$cand"; break; fi
done
[[ -n "$HERDR" ]] || { log "FATAL: no herdr binary"; exit 3; }

"$HERDR" resume-picker open --wrapper "$WRAPPER" >>"$LOG" 2>&1
log "RESUME-PICKER OVERLAY OPEN -> $WRAPPER"
exit 0
