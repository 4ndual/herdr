#!/usr/bin/env bash
# resume-picker — thin launcher that runs the Bun TUI and then execs the resume
# command it prints, replacing this process IN THE SAME PANE (fzf model).
#
# The TUI renders to the tty via stderr and prints exactly one line to stdout on
# Enter/click: the resolved `exec cx…` / `exec cx-resume…` command. On Esc it
# prints nothing and we fall back to the shell.
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"

BUN="${BUN:-}"
if [[ -z "$BUN" ]]; then
  for cand in "$(command -v bun 2>/dev/null)" "$HOME/.pkg-guard/shim/bun" "$HOME/.bun/bin/bun"; do
    [[ -n "$cand" && -x "$cand" ]] && { BUN="$cand"; break; }
  done
fi
[[ -n "$BUN" ]] || { printf 'resume-picker: bun not found\n' >&2; exit 3; }

CMD="$("$BUN" "$HERE/resume-picker.ts")"
if [[ -n "${CMD:-}" ]]; then
  exec sh -c "$CMD"
fi
# Esc / Ctrl-C: nothing selected — return to the shell that launched us.
exit 0
