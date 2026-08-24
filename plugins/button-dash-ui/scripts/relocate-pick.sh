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

AGENT=""; SID=""; CWD=""; PROJECTS_DIR=""; DISCOVERY_LAYER=""
while IFS= read -r line; do
  case "$line" in
    AGENT=*) AGENT="${line#AGENT=}" ;;
    SID=*) SID="${line#SID=}" ;;
    CWD=*) CWD="${line#CWD=}" ;;
    PROJECTS_DIR=*) PROJECTS_DIR="${line#PROJECTS_DIR=}" ;;
    DISCOVERY_LAYER=*) DISCOVERY_LAYER="${line#DISCOVERY_LAYER=}" ;;
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

# Fallback: if the skill doesn't exist, check if it's bundled in the plugin
if [[ ! -f "$MIGRATE" ]]; then
  PLUGIN_FALLBACK="$SCRIPT_DIR/migrate-session.sh"
  if [[ -f "$PLUGIN_FALLBACK" ]]; then
    MIGRATE="$PLUGIN_FALLBACK"
    log "using plugin-bundled migrate-session.sh: $MIGRATE"
  fi
fi

# ---------------------------------------------------------------------------
# LAYER 3 Confirmation: When session was guessed via filesystem heuristic,
# show session details and require explicit confirmation before proceeding
# ---------------------------------------------------------------------------
if [[ "$DISCOVERY_LAYER" =~ ^layer3 ]]; then
  printf '\n%s\n' "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  printf 'SESSION GUESSED (Layer 3: Filesystem Heuristic)\n'
  printf '%s\n' "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  printf '\nThe session ID was not reported by herdr and was guessed from:\n'
  if [[ "$DISCOVERY_LAYER" == "layer3_cwd" ]]; then
    printf '  ✓ Newest session file whose recorded cwd matches the pane cwd\n'
  else
    printf '  ✓ Newest overall session file (cwd did not match)\n'
  fi
  printf '\n'

  # Find and display the session file details
  SESSION_FILE=""
  SESSION_MTIME=""
  SESSION_FIRST_MSG=""

  case "$AGENT" in
    claude|cx)
      while IFS= read -r CLAUDE_DIR; do
        [[ -n "$CLAUDE_DIR" ]] || continue
        for slug_dir in "$CLAUDE_DIR/projects"/*; do
          [[ -d "$slug_dir" ]] || continue
          if [[ -f "$slug_dir/$SID.jsonl" ]]; then
            SESSION_FILE="$slug_dir/$SID.jsonl"
            break 2
          fi
        done
      done < <(
        # Inline claude_roots for this subshell
        _cr_default="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
        [[ -d "$_cr_default" ]] && printf '%s\n' "$_cr_default"
        [[ -d "$HOME/harnesses" ]] || return 0
        for _cr_dir in "$HOME"/harnesses/*/; do
          [[ -r "$_cr_dir/harness.json" ]] || continue
          _cr_rt="$(jq -r '.runtime // empty' <"$_cr_dir/harness.json" 2>/dev/null)"
          _cr_profile="$(jq -r '.profile // false' <"$_cr_dir/harness.json" 2>/dev/null)"
          [[ "$_cr_profile" == "true" ]] && continue
          [[ "$_cr_rt" == "claude" ]] || continue
          printf '%s\n' "$_cr_dir"
        done
      )
      ;;
    codex)
      CODEX_HOME="${CODEX_HOME:-$HOME/.codex}"
      SESSION_FILE="$(find "$CODEX_HOME/sessions" -type f -name "*.jsonl" 2>/dev/null | xargs ls -t 2>/dev/null | grep -m1 .)"
      ;;
    pi)
      PI_HOME="${PI_HOME:-$HOME/.pi}"
      SESSION_FILE="$(find "$PI_HOME/agent/sessions" -type f -name "*.jsonl" 2>/dev/null | xargs ls -t 2>/dev/null | grep -m1 .)"
      ;;
    jcode)
      JCODE_HOME="${JCODE_HOME:-$HOME/.jcode}"
      SESSION_FILE="$(find "$JCODE_HOME/sessions" -type f -name "session_*.json" 2>/dev/null | xargs ls -t 2>/dev/null | grep -m1 .)"
      ;;
  esac

  if [[ -f "$SESSION_FILE" ]]; then
    SESSION_MTIME="$(stat -c '%y' "$SESSION_FILE" 2>/dev/null | cut -d' ' -f1-2)" || SESSION_MTIME="$(ls -lT "$SESSION_FILE" 2>/dev/null | awk '{print $6, $7, $8}')"
    printf 'Session file: %s\n' "$(basename "$SESSION_FILE")"
    printf 'Modified    : %s\n' "$SESSION_MTIME"

    # Try to extract first user message snippet
    if [[ "$SESSION_FILE" == *.jsonl ]]; then
      # JSONL format — read first 2 lines (usually meta + first message)
      SESSION_FIRST_MSG="$(head -2 "$SESSION_FILE" 2>/dev/null | tail -1 | jq -r '.content // .text // empty' 2>/dev/null | cut -c1-60)"
    elif [[ "$SESSION_FILE" == *.json ]]; then
      # JSON format
      SESSION_FIRST_MSG="$(jq -r '.[0].content // .[0].text // .content // .text // empty' "$SESSION_FILE" 2>/dev/null | cut -c1-60)"
    fi

    if [[ -n "$SESSION_FIRST_MSG" ]]; then
      printf 'First msg   : %s...\n' "$SESSION_FIRST_MSG"
    fi
  fi

  printf '%s\n' "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
  printf '\nIs this the session you want to relocate? [y/n]: '
  IFS= read -r CONFIRM || CONFIRM="n"

  case "$CONFIRM" in
    y|Y|yes|YES)
      log "confirmed layer 3 guessed sid=$SID"
      printf '\nProceeding with relocation...\n'
      ;;
    *)
      echo "cancelled — nothing was moved"
      log "layer 3 guess rejected (sid=$SID)"
      hold
      ;;
  esac
  printf '\n'
fi

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

printf '\n  c) specify another folder…\n'
printf '  q) cancel — change nothing\n\n'
printf 'Pick a number or c/q: '

IFS= read -r ANSWER || ANSWER="q"
case "$ANSWER" in
  ""|q|Q)
    echo "cancelled — nothing was moved"
    log "cancelled at picker (sid=$SID)"
    hold
    ;;
  c|C)
    # Custom folder path entry
    printf '\nEnter folder path (use ~ for home, or path under ~/Projects):\n'
    printf 'path: '
    IFS= read -r CUSTOM_PATH || CUSTOM_PATH=""

    if [[ -z "$CUSTOM_PATH" ]]; then
      echo "cancelled — nothing was moved"
      log "cancelled custom path entry (sid=$SID)"
      hold
    fi

    # Expand ~ to home directory
    CUSTOM_PATH="${CUSTOM_PATH/#\~/$HOME}"

    # If relative path, treat as relative to ~/Projects
    if [[ "$CUSTOM_PATH" != /* ]]; then
      CUSTOM_PATH="$HOME/Projects/$CUSTOM_PATH"
    fi

    # Normalize the path (remove trailing slashes)
    CUSTOM_PATH="${CUSTOM_PATH%/}"

    log "custom folder entry: $CUSTOM_PATH (sid=$SID)"

    # Check if path exists
    if [[ ! -d "$CUSTOM_PATH" ]]; then
      printf '\nFolder does not exist: %s\n' "$CUSTOM_PATH"
      printf 'Create it? [y/N]: '
      IFS= read -r CREATE_CONFIRM || CREATE_CONFIRM="n"

      case "$CREATE_CONFIRM" in
        y|Y)
          printf 'creating folder…\n'
          if ! mkdir -p "$CUSTOM_PATH"; then
            echo "failed to create folder — nothing was moved"
            log "FATAL(pick): could not create custom folder: $CUSTOM_PATH"
            hold
          fi
          log "created custom folder: $CUSTOM_PATH"
          printf 'relocating into %s …\n' "$CUSTOM_PATH"
          ;;
        *)
          echo "cancelled — nothing was moved"
          log "cancelled custom folder creation (sid=$SID)"
          hold
          ;;
      esac
    else
      printf '\nrelocating into %s …\n' "$CUSTOM_PATH"
    fi

    TARGET="$CUSTOM_PATH"
    log "migrating sid=$SID -> $TARGET (projects_dir=$PROJECTS_DIR, custom path)"
    ;;
esac

# Handle numbered choices (custom path case already handled above)
if [[ "$ANSWER" =~ ^[0-9]+$ ]]; then
  if (( ANSWER < 1 || ANSWER > ${#PATHS[@]} )); then
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
fi

# The migration is the relocate skill's job. Point it at the projects root the
# transcript actually lives under, and pass the agent type for runtime-specific handling.
if ! CLAUDE_PROJECTS_DIR="$PROJECTS_DIR" AGENT_TYPE="$AGENT" sh "$MIGRATE" "$TARGET" "$SID"; then
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
