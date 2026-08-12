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
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=dash-common.sh
. "$SCRIPT_DIR/dash-common.sh" || { echo "resume-picker: dash-common.sh missing" >&2; exit 3; }

# $HERDR_BIN_PATH (resolved in dash-common.sh) is the RUNNING server's own
# executable, so it always carries the `resume-picker` subcommand this script
# needs — which a stale ~/.local/bin/herdr on PATH famously did not, failing
# every invocation with "unknown command: resume-picker".
if [[ -z "$HERDR" ]]; then
  log "FATAL: no herdr binary found (HERDR_BIN/HERDR_BIN_PATH unset and none on PATH)"
  desktop_notify "Resume search failed" "no herdr binary found"
  exit 3
fi

# Locate the overlay wrapper. resume-picker is NOT a herdr plugin -- it is a
# standalone bun tool -- so it does not ship in this repo. Resolution order,
# all configuration-free except the first:
#   1. $HERDR_RESUME_PICKER_WRAPPER  — explicit override for an odd install
#   2. beside this plugin ($HERDR_PLUGIN_ROOT/../resume-picker/…), which is
#      where a checkout that vendors it puts it
#   3. the same path derived from this script's own location, for a hand run
#      with no host env at all
#   4. anything named resume-picker-overlay.sh on PATH
# The old code hardcoded $HOME/Projects/herdr/plugins/resume-picker/… and so
# only ever worked on the machine it was written on.
WRAPPER=""
for cand in "${HERDR_RESUME_PICKER_WRAPPER:-}" \
  "${HERDR_PLUGIN_ROOT:+$HERDR_PLUGIN_ROOT/../resume-picker/bin/resume-picker-overlay.sh}" \
  "$SCRIPT_DIR/../../resume-picker/bin/resume-picker-overlay.sh" \
  "$(command -v resume-picker-overlay.sh 2>/dev/null || true)"; do
  if [[ -n "$cand" && -x "$cand" ]]; then WRAPPER="$cand"; break; fi
done
if [[ -z "$WRAPPER" ]]; then
  toast "Resume search unavailable" \
    "the resume-picker overlay is not installed — point HERDR_RESUME_PICKER_WRAPPER at its bin/resume-picker-overlay.sh"
  log "FATAL: no resume-picker overlay wrapper found"
  exit 3
fi

# Check the exit status. The old version logged a success line unconditionally,
# so a failing call still read as "OPEN" in the log.
if ! "$HERDR" resume-picker open --wrapper "$WRAPPER" >>"$LOG" 2>&1; then
  toast "Resume search failed" "herdr could not open the picker overlay"
  log "FATAL: resume-picker open failed (herdr=$HERDR wrapper=$WRAPPER)"
  exit 3
fi
log "RESUME-PICKER OVERLAY OPEN -> $WRAPPER (herdr=$HERDR)"
exit 0
