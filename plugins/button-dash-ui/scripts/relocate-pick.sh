#!/usr/bin/env bash
# relocate-pick.sh — runs IN the tab that relocate-conversation.sh opens.
#
# Shows the ranked candidate projects, takes the user's pick, migrates the
# session on disk into that project's slug, and continues the conversation there
# with `claude --resume`.
#
# The choice is made HERE, by a human, on purpose: the ranking is evidence and
# the top two candidates are frequently a near-tie.
#
# The source session is COPIED, never destroyed — re-running is safe and the
# original pane keeps working.
#
# Usage: relocate-pick.sh <payload-file>

set -u

PAYLOAD="${1:?usage: relocate-pick.sh <payload-file>}"
LOG="${HERDR_RELOCATE_LOG:-$HOME/.herdr-relocate.log}"
log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG" 2>/dev/null; }

# Remote Control on the resumed session, like every other claude launch these
# plugins make. Sourced rather than re-implemented so the opt-out (HARNESS_RC=off)
# and the version guard stay in one place; this script runs IN a pane created by
# relocate-conversation.sh and so has the same scripts/ directory beside it.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ -r "$SCRIPT_DIR/dash-common.sh" ]]; then
  # shellcheck source=dash-common.sh
  . "$SCRIPT_DIR/dash-common.sh"
else
  # A relocate that still works beats a relocate that dies over a missing flag.
  rc_flags() { :; }
fi

hold() {
  printf '\n'
  # /bin/sh, not /bin/zsh: zsh is the macOS default login shell but is not
  # installed on a stock Linux box, so the old fallback exec'd a missing binary
  # and killed the pane on every error path this function exists to survive.
  exec "${SHELL:-/bin/sh}" -l
}

[[ -r "$PAYLOAD" ]] || { echo "relocate: candidate list missing ($PAYLOAD)"; log "FATAL(pick): payload unreadable"; hold; }

SID=""; CWD=""; PROJECTS_DIR=""
while IFS= read -r line; do
  case "$line" in
    SID=*) SID="${line#SID=}" ;;
    CWD=*) CWD="${line#CWD=}" ;;
    PROJECTS_DIR=*) PROJECTS_DIR="${line#PROJECTS_DIR=}" ;;
    --) break ;;
  esac
done <"$PAYLOAD"

# Candidate lines are everything after the `--` marker: "<score>\t<path>".
# Plain read loop, not `mapfile`: macOS ships bash 3.2, which has neither
# mapfile nor associative arrays, and this must stay runnable there too.
ROWS=()
while IFS= read -r row; do
  [[ -n "$row" ]] && ROWS+=("$row")
done < <(sed -n '/^--$/,$p' "$PAYLOAD" | tail -n +2)
rm -f "$PAYLOAD"

if [[ ${#ROWS[@]} -eq 0 || -z "$SID" ]]; then
  echo "relocate: nothing to choose from."
  log "FATAL(pick): empty candidate list or no sid"
  hold
fi

MIGRATE="${HERDR_RELOCATE_MIGRATE:-${CLAUDE_CONFIG_DIR:-$HOME/.claude}/skills/relocate/scripts/migrate-session.sh}"

printf 'Relocate this conversation\n'
printf '  session : %s\n' "$SID"
printf '  now in  : %s   <- the wrong place\n\n' "$CWD"
printf 'Projects this conversation actually worked in, by distinct files touched:\n\n'

PATHS=()
i=0
for row in "${ROWS[@]}"; do
  score="${row%%$'\t'*}"
  path="${row#*$'\t'}"
  [[ -n "$path" ]] || continue
  i=$((i + 1))
  PATHS+=("$path")
  printf '  %d) %-58s %s files\n' "$i" "$path" "$score"
done

printf '\n  q) cancel — change nothing\n\n'
printf 'Pick a number (the score is evidence, not a recommendation): '

IFS= read -r ANSWER || ANSWER="q"
case "$ANSWER" in
  ""|q|Q)
    echo "cancelled — nothing was moved"
    log "cancelled at picker (sid=$SID)"
    hold
    ;;
esac
if ! [[ "$ANSWER" =~ ^[0-9]+$ ]] || (( ANSWER < 1 || ANSWER > ${#PATHS[@]} )); then
  echo "not a listed choice — nothing was moved"
  log "invalid pick '$ANSWER' (sid=$SID)"
  hold
fi

TARGET="${PATHS[$((ANSWER - 1))]}"
if [[ ! -d "$TARGET" ]]; then
  echo "relocate: '$TARGET' is no longer a directory — nothing was moved."
  log "FATAL(pick): target vanished: $TARGET"
  hold
fi

printf '\nrelocating into %s …\n' "$TARGET"
log "migrating sid=$SID -> $TARGET (projects_dir=$PROJECTS_DIR)"

# The migration is the relocate skill's job. Point it at the projects root the
# transcript actually lives under.
if ! CLAUDE_PROJECTS_DIR="$PROJECTS_DIR" sh "$MIGRATE" "$TARGET" "$SID"; then
  echo
  echo "relocate: migration FAILED — nothing was moved. The original conversation is untouched."
  log "FATAL(pick): migrate-session.sh failed for sid=$SID -> $TARGET"
  hold
fi
log "migrated OK sid=$SID -> $TARGET"

cd "$TARGET" || { echo "relocate: cannot enter $TARGET"; hold; }

printf 'resuming in %s …\n\n' "$TARGET"
# The Remote Control prefix is the project the conversation was just relocated
# INTO — the whole point of this button is that the old name was wrong, so the
# new project is the only meaningful label the resumed session can carry.
RC="$(rc_flags "$(basename "$TARGET")")"
if command -v claude >/dev/null 2>&1; then
  # RC is a shell-quoted fragment from dash-common.sh (or empty); eval is what
  # turns it back into argv words, exactly as relaunch-skip.sh does.
  eval "exec claude --resume $(printf '%q' "$SID")$RC"
fi

echo "relocate: migrated, but claude is not on PATH."
echo "run:  cd '$TARGET' && claude --resume $SID$RC"
log "FATAL(pick): claude not on PATH"
hold
