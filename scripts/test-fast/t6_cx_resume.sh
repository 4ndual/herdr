#!/usr/bin/env bash
# T6 — cx-resume any-cwd fallback regression.
#
# Real bug: a cx (herdr:claude) session whose original cwd was deleted made
# `claude --resume` print "No conversation found". cx-resume must instead LOCATE
# the transcript anywhere under the CCR/daily roots, recover the original cwd
# from the transcript, recreate it, and re-dispatch — never printing the failure
# string.
#
# Fully sandboxed: a throwaway HOME, a real (synthetic) jsonl under a fake CCR
# projects dir keyed to a DELETED cwd, and a shim `claude`/`cx` on PATH that
# records the resume instead of launching the real agent. Touches neither the
# vault, ~/.claude, nor ~/.config. No mocking of the code under test — the real
# cx-resume script runs against real files; only the terminal exec target
# (claude) is a shim, which is the process boundary, not the logic being tested.
set -uo pipefail

CX_RESUME="$HOME/bin/cx-resume"
[ -x "$CX_RESUME" ] || { echo "cx-resume not found at $CX_RESUME"; exit 1; }

SB="$(mktemp -d)"
trap 'rm -rf "$SB"' EXIT

export HOME="$SB/home"
CCR_DIR="$HOME/.claude-code-router/profiles/default-claude-code/claude"
mkdir -p "$HOME/bin" "$CCR_DIR/projects"

# A cwd that will NOT exist when resume runs (simulates a cleaned-up tmp cwd).
ORIG_CWD="$SB/gone/project-x"
ENC="$(printf '%s' "$ORIG_CWD" | LC_ALL=C sed 's/[^A-Za-z0-9]/-/g')"
SID="11111111-2222-3333-4444-555555555555"

# Real transcript: lives under the CCR projects dir keyed to the original cwd,
# and records that cwd inside — exactly the shape cx-resume recovers from.
mkdir -p "$CCR_DIR/projects/$ENC"
printf '{"type":"user","cwd":"%s","sessionId":"%s","message":{"role":"user","content":"hi"}}\n' \
  "$ORIG_CWD" "$SID" > "$CCR_DIR/projects/$ENC/$SID.jsonl"

# Shim exec targets on PATH: record the resume and succeed. cx-resume execs one
# of these after locating the transcript.
for name in claude cx; do
  cat > "$HOME/bin/$name" <<EOF
#!/bin/sh
echo "RESUMED via $name in \$PWD args: \$*" > "$SB/resumed.txt"
exit 0
EOF
  chmod +x "$HOME/bin/$name"
done
export PATH="$HOME/bin:$PATH"

# Run from a cwd that does NOT match the transcript's encoded project dir, so the
# only way to succeed is the any-cwd fallback.
mkdir -p "$SB/elsewhere"; cd "$SB/elsewhere"

OUT="$("$CX_RESUME" "$SID" 2>&1)"; RC=$?

if printf '%s' "$OUT" | grep -qi 'No conversation found'; then
  echo "cx-resume printed 'No conversation found' — fallback broken"; echo "$OUT"; exit 1
fi
if printf '%s' "$OUT" | grep -qi 'could not be recovered'; then
  echo "cx-resume failed to recover the original cwd"; echo "$OUT"; exit 1
fi
if [ ! -f "$SB/resumed.txt" ]; then
  echo "cx-resume did not reach the resume exec (rc=$RC): $OUT"; exit 1
fi
if [ ! -d "$ORIG_CWD" ]; then
  echo "cx-resume did not recreate the deleted original cwd"; exit 1
fi
echo "ok: cx-resume located transcript, recreated cwd, resumed ($(cat "$SB/resumed.txt"))"
