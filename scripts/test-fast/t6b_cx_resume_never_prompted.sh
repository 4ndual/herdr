#!/usr/bin/env bash
# T6b — cx-resume never-prompted-session regression.
#
# Real bug: herdr restored a claude pane whose session was minted but NEVER given
# a first prompt. claude only writes the transcript jsonl on prompt #1, so no
# transcript exists under EITHER root (CCR profile or ~/.claude). `cx-resume`'s
# any-cwd fallback found nothing and fell through to `claude --resume <sid>`,
# which printed the red "No conversation found with session ID" into a dead shell.
#
# cx-resume must instead detect the no-transcript-anywhere case and start a FRESH
# claude session (same args, minus --resume/<sid>) with a one-line stderr note,
# leaving the pane a working claude the user can simply re-prompt.
#
# Fully sandboxed: a throwaway HOME with EMPTY CCR + ~/.claude projects trees (no
# transcript at all) and a shim `claude` on PATH that fails on --resume (like the
# real bug) but records a fresh launch otherwise. Touches neither the vault,
# ~/.claude, ~/.config, nor the real ~/bin. No mocking of the code under test —
# the real cx-resume runs against real (empty) dirs; only the terminal exec target
# (claude) is a shim, the process boundary, not the logic being tested.
set -uo pipefail

CX_RESUME="$HOME/bin/cx-resume"
[ -x "$CX_RESUME" ] || { echo "cx-resume not found at $CX_RESUME"; exit 1; }

SB="$(mktemp -d)"
trap 'rm -rf "$SB"' EXIT

# Own HOME so cx-resume reads EMPTY project roots and our shim bin — never the
# real ~/bin (writing a shim through the real ~/bin/cx symlink would clobber the
# real launcher). Everything below stays inside $SB.
export HOME="$SB/home"
CCR_DIR="$HOME/.claude-code-router/profiles/default-claude-code/claude"
mkdir -p "$HOME/bin" "$CCR_DIR/projects" "$HOME/.claude/projects"

SID="c2447fe3-1111-2222-3333-444455556666"

# Shim `claude`: mimic the real failure on --resume of a missing sid; on a fresh
# (no --resume) launch, record the argv so we can assert a working session booted.
cat > "$HOME/bin/claude" <<EOF
#!/bin/sh
for a in "\$@"; do [ "\$a" = "--resume" ] && R=1; done
if [ "\${R:-0}" = 1 ]; then
  echo "No conversation found with session ID: \$*" >&2
  exit 1
fi
echo "FRESH claude in \$PWD args: \$*" > "$SB/fresh.txt"
exit 0
EOF
chmod +x "$HOME/bin/claude"
export PATH="$HOME/bin:$PATH"

# Resume from a cwd with NO transcript (there is none anywhere — never prompted).
mkdir -p "$SB/elsewhere"; cd "$SB/elsewhere"

OUT="$("$CX_RESUME" "$SID" 2>&1)"; RC=$?

if printf '%s' "$OUT" | grep -qi 'No conversation found'; then
  echo "cx-resume printed 'No conversation found' — never-prompted fallthrough not fixed"; echo "$OUT"; exit 1
fi
if ! printf '%s' "$OUT" | grep -qi 'starting a fresh session'; then
  echo "cx-resume did not print the fresh-start note"; echo "$OUT"; exit 1
fi
if [ ! -f "$SB/fresh.txt" ]; then
  echo "cx-resume did not launch a fresh claude (rc=$RC): $OUT"; exit 1
fi
if grep -q -- '--resume' "$SB/fresh.txt"; then
  echo "cx-resume leaked --resume into the fresh launch: $(cat "$SB/fresh.txt")"; exit 1
fi
echo "ok: never-prompted sid → fresh working claude, no red error ($(cat "$SB/fresh.txt"))"
