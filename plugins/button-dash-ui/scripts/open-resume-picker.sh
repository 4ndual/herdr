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

# Toast, don't just log. A failure only written to a log file is invisible to the
# user -- this script exited 3 silently for weeks. Mirrors remote-control.sh.
toast() {
  log "TOAST: $1 — ${2:-}"
  [[ -n "${HERDR:-}" ]] && "$HERDR" notification show "$1" ${2:+--body "$2"} >>"$LOG" 2>&1 || true
}

# Resolve the herdr client binary. ORDER MATTERS and PATH comes LAST: on this
# machine `command -v herdr` resolves to ~/.local/bin/herdr, a symlink to a
# June-19 pre-prod build with no `resume-picker` subcommand at all. Preferring it
# meant every invocation failed with "unknown command: resume-picker" and the
# picker's own logic was never once reached. Prefer the instance binary that
# actually carries the fork's subcommands.
HERDR=""
for cand in "${HERDR_BIN:-}" \
  "$HOME/.herdr-hr/bin/herdr" \
  "$HOME/Projects/herdr-worktrees/imgapi-127/target/release/herdr" \
  "$HOME/Projects/herdr/target/release/herdr" \
  "$(command -v herdr 2>/dev/null)"; do
  if [[ -n "$cand" && -x "$cand" ]]; then HERDR="$cand"; break; fi
done
if [[ -z "$HERDR" ]]; then
  log "FATAL: no herdr binary found"
  exit 3
fi

# Locate the overlay wrapper. resume-picker is NOT a herdr plugin -- it is a
# standalone bun tool -- so it does not live beside this repo, and the old
# "$HERE/../../resume-picker/bin" walk resolved to a path that does not exist
# once this plugin moved out of the herdr tree. Take an explicit override first,
# then known absolute locations.
WRAPPER=""
for cand in "${HERDR_RESUME_PICKER_WRAPPER:-}" \
  "$HOME/Projects/herdr/plugins/resume-picker/bin/resume-picker-overlay.sh" \
  "$HOME/Projects/resume-picker/bin/resume-picker-overlay.sh"; do
  if [[ -n "$cand" && -x "$cand" ]]; then WRAPPER="$cand"; break; fi
done
if [[ -z "$WRAPPER" ]]; then
  toast "Resume search unavailable" "overlay wrapper not found; set HERDR_RESUME_PICKER_WRAPPER"
  log "FATAL: no resume-picker overlay wrapper found"
  exit 3
fi

# Check the exit status. The old version logged a success line unconditionally,
# so a failing call still read as "OPEN" in the log.
if ! "$HERDR" resume-picker open --wrapper "$WRAPPER" >>"$LOG" 2>&1; then
  toast "Resume search failed" "$(basename "$HERDR") could not open the picker overlay"
  log "FATAL: resume-picker open failed (herdr=$HERDR wrapper=$WRAPPER)"
  exit 3
fi
log "RESUME-PICKER OVERLAY OPEN -> $WRAPPER (herdr=$HERDR)"
exit 0
