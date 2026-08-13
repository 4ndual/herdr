#!/usr/bin/env bash
# resume-picker-overlay.sh — the wrapper herdr's resume-search overlay spawns.
#
# `herdr resume-picker open --wrapper <this>` runs it as a hidden PTY with
# $HERDR_RESUME_RESULT_FILE in the env (src/app/resume_picker.rs). Two jobs, and
# only two:
#
#   1. send stdout to the result file, so the picker's chosen command line lands
#      where the host reads it and never leaks onto the screen. The TUI itself
#      draws on stderr, which stays attached to the PTY.
#   2. exec the picker, so the PTY's process IS the picker and the host's
#      PaneDied fires the moment it exits.
#
# Runtime: bun. `bun run <file>` is used rather than a committed compiled binary
# — a checked-in 50 MB per-platform executable is worse than a dependency the
# whole toolchain already has (the agent-launcher, pi and the rest of this box
# all run on it). A prebuilt `bin/resume-picker` (from `bun build --compile`) is
# preferred when present, for machines without bun on PATH.
set -u

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENTRY="$SCRIPT_DIR/../src/picker.ts"
COMPILED="$SCRIPT_DIR/resume-picker"

# A plugin action inherits the SERVER's env, which may carry a desktop session's
# minimal PATH with no ~/.bun/bin — the same trap dash-common.sh documents.
for d in "$HOME/.bun/bin" "$HOME/.local/bin" /opt/homebrew/bin /usr/local/bin; do
  case ":$PATH:" in
    *":$d:"*) ;;
    *) [ -d "$d" ] && PATH="$PATH:$d" ;;
  esac
done
export PATH

: "${HERDR_RESUME_RESULT_FILE:=/dev/null}"

if [ -x "$COMPILED" ]; then
  exec "$COMPILED" >"$HERDR_RESUME_RESULT_FILE"
fi

BUN="$(command -v bun 2>/dev/null || true)"
if [ -z "$BUN" ]; then
  # stderr is the visible surface; the overlay shows this until Esc.
  echo "resume-picker: bun is not installed (needed to run src/picker.ts)" >&2
  echo "install it from https://bun.sh, or build bin/resume-picker with 'bun build --compile'" >&2
  sleep 6
  exit 3
fi

exec "$BUN" run "$ENTRY" >"$HERDR_RESUME_RESULT_FILE"
