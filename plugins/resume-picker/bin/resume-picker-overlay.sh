#!/usr/bin/env bash
# resume-picker-overlay — the in-app overlay entrypoint for the resume picker.
#
# Runs the SAME resume-picker.ts TUI, byte-for-byte, but instead of the fzf
# same-pane `exec` model (resume-picker.sh) it redirects the picker's STDOUT —
# the single resolved `exec cx…` line — to $HERDR_RESUME_RESULT_FILE. The TUI
# still renders to STDERR (the overlay PTY) and reads keys from STDIN (also the
# PTY), so the picker paints into the herdr overlay window. herdr reads the
# result file when this process exits and opens the resolved command in a new
# tab. Esc/Ctrl-C leave the file empty (a cancel).
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"

: "${HERDR_RESUME_RESULT_FILE:?resume-picker-overlay: HERDR_RESUME_RESULT_FILE not set}"

BUN="${BUN:-}"
if [[ -z "$BUN" ]]; then
  for cand in "$(command -v bun 2>/dev/null)" "$HOME/.pkg-guard/shim/bun" "$HOME/.bun/bin/bun"; do
    [[ -n "$cand" && -x "$cand" ]] && { BUN="$cand"; break; }
  done
fi
[[ -n "$BUN" ]] || { printf 'resume-picker-overlay: bun not found\n' >&2; exit 3; }

# stdout -> result file (the resolved command); stderr/stdin stay on the PTY.
exec "$BUN" "$HERE/resume-picker.ts" >"$HERDR_RESUME_RESULT_FILE"
