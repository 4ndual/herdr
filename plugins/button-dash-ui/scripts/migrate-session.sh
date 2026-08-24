#!/usr/bin/env bash
# migrate-session.sh — move a conversation's transcript and cache from
# one project to another, handling multiple runtime types (claude, codex, pi, jcode).
#
# Called by relocate-pick.sh after the user chooses a target project. Moves the
# session from wherever it currently lives (determined by CLAUDE_PROJECTS_DIR or
# runtime-specific env) into the chosen target project, preserving the transcript
# and cache lineage so the session can be resumed.
#
# Usage: migrate-session.sh <target-git-path> <session-id>
# Environment:
#   AGENT_TYPE (required; one of: claude, codex, pi, jcode)
#   CLAUDE_PROJECTS_DIR (for claude runtime)
#
# Exit codes:
#   0 — success
#   1 — invalid arguments or missing env
#   2 — transcript not found or already moved
#   3 — target path is not a directory
#   4 — permission denied or I/O error
#   5 — unsupported runtime or runtime-specific error

set -u

TARGET_DIR="${1:?usage: migrate-session.sh <target-git-path> <session-id>}"
SID="${2:?usage: migrate-session.sh <target-git-path> <session-id>}"
AGENT_TYPE="${AGENT_TYPE:-claude}"
LOG="${HERDR_RELOCATE_LOG:-$HOME/.herdr-relocate.log}"

log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG" 2>/dev/null; }

log "migrate: starting TARGET_DIR=$TARGET_DIR SID=$SID AGENT_TYPE=$AGENT_TYPE"

# ─────────────────────────────────────────────────────────────────────────────
# Validate arguments and environment
# ─────────────────────────────────────────────────────────────────────────────

if [[ -z "$TARGET_DIR" || -z "$SID" ]]; then
  log "migrate: FATAL missing arguments"
  exit 1
fi

if [[ ! -d "$TARGET_DIR" ]]; then
  log "migrate: FATAL target is not a directory: $TARGET_DIR"
  exit 3
fi

# ─────────────────────────────────────────────────────────────────────────────
# Helper: compute slug from path
# ─────────────────────────────────────────────────────────────────────────────
slug_from_path() {
  local p="$1"
  # Normalize to absolute path
  p="$(cd "$p" 2>/dev/null && pwd)" || return 1
  # Replace / with - and strip leading -
  printf '%s' "$p" | sed 's#/#-#g' | sed 's/^-//'
}

# ─────────────────────────────────────────────────────────────────────────────
# Claude runtime migration
# ─────────────────────────────────────────────────────────────────────────────
migrate_claude() {
  local target_dir="$1"
  local sid="$2"

  local source_projects_dir="${CLAUDE_PROJECTS_DIR:-}"
  if [[ -z "$source_projects_dir" ]]; then
    log "migrate: FATAL CLAUDE_PROJECTS_DIR not set"
    exit 1
  fi

  if [[ ! -d "$source_projects_dir" ]]; then
    log "migrate: FATAL source projects dir does not exist: $source_projects_dir"
    exit 2
  fi

  # Find source slug and transcript
  local source_slug=""
  local source_transcript=""
  for slug_dir in "$source_projects_dir"/*; do
    if [[ -d "$slug_dir" ]]; then
      local slug="$(basename "$slug_dir")"
      if [[ -f "$slug_dir/$sid.jsonl" ]]; then
        source_slug="$slug"
        source_transcript="$slug_dir/$sid.jsonl"
        break
      fi
    fi
  done

  if [[ -z "$source_slug" || -z "$source_transcript" ]]; then
    log "migrate: FATAL could not find source transcript for SID $sid in $source_projects_dir"
    exit 2
  fi

  local target_slug
  if ! target_slug="$(slug_from_path "$target_dir")"; then
    log "migrate: FATAL could not compute target slug"
    exit 1
  fi

  log "migrate: claude source_slug=$source_slug target_slug=$target_slug"

  # Create target slug directory if needed
  local target_projects_dir="$source_projects_dir"
  local target_slug_dir="$target_projects_dir/$target_slug"

  if [[ ! -d "$target_slug_dir" ]]; then
    log "migrate: creating target slug directory: $target_slug_dir"
    if ! mkdir -p "$target_slug_dir"; then
      log "migrate: FATAL could not create target slug directory"
      exit 4
    fi
  fi

  # Prepare paths
  local source_slug_dir="$source_projects_dir/$source_slug"
  local source_cache_dir="$source_slug_dir/$sid"
  local target_transcript="$target_slug_dir/$sid.jsonl"
  local target_cache_dir="$target_slug_dir/$sid"

  log "migrate: source_transcript=$source_transcript"
  log "migrate: source_cache_dir=$source_cache_dir"
  log "migrate: target_transcript=$target_transcript"
  log "migrate: target_cache_dir=$target_cache_dir"

  # Check if already in target location
  if [[ "$source_slug_dir" == "$target_slug_dir" ]]; then
    log "migrate: session already in target directory (no-op success)"
    exit 0
  fi

  # Remove existing target if present
  if [[ -f "$target_transcript" ]]; then
    log "migrate: removing existing target transcript: $target_transcript"
    if ! rm -f "$target_transcript"; then
      log "migrate: FATAL could not remove existing target transcript"
      exit 4
    fi
  fi

  if [[ -d "$target_cache_dir" ]]; then
    log "migrate: removing existing target cache dir: $target_cache_dir"
    if ! rm -rf "$target_cache_dir"; then
      log "migrate: FATAL could not remove existing target cache dir"
      exit 4
    fi
  fi

  # Move transcript
  if ! mv "$source_transcript" "$target_transcript"; then
    log "migrate: FATAL could not move transcript to $target_transcript"
    exit 4
  fi
  log "migrate: moved transcript OK"

  # Move cache directory (if it exists)
  if [[ -d "$source_cache_dir" ]]; then
    if ! mv "$source_cache_dir" "$target_cache_dir"; then
      # Try to recover
      if ! mv "$target_transcript" "$source_transcript" 2>/dev/null; then
        log "migrate: FATAL could not move cache AND could not recover transcript"
      else
        log "migrate: FATAL could not move cache (recovered transcript)"
      fi
      exit 4
    fi
    log "migrate: moved cache dir OK"
  else
    log "migrate: no cache dir to move (empty session)"
  fi

  log "migrate: SUCCESS (claude) sid=$sid moved from $source_slug to $target_slug"
  exit 0
}

# ─────────────────────────────────────────────────────────────────────────────
# Codex runtime migration
# ─────────────────────────────────────────────────────────────────────────────
migrate_codex() {
  local target_dir="$1"
  local sid="$2"

  local codex_home="${CODEX_HOME:-$HOME/.codex}"
  local sessions_dir="$codex_home/sessions"

  if [[ ! -d "$sessions_dir" ]]; then
    log "migrate: FATAL codex sessions directory not found: $sessions_dir"
    exit 2
  fi

  # Find the source session file (YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl format)
  # Use find instead of globs for reliability across different shell configurations
  local source_transcript=""
  source_transcript="$(find "$sessions_dir" -type f -name "*$sid*.jsonl" 2>/dev/null | head -1)"

  # If not found with .jsonl extension, try without
  if [[ -z "$source_transcript" ]]; then
    source_transcript="$(find "$sessions_dir" -type f -name "*$sid*" ! -name "*.jsonl" 2>/dev/null | head -1)"
  fi

  if [[ -z "$source_transcript" ]]; then
    log "migrate: FATAL could not find codex session file for SID $sid in $sessions_dir"
    exit 2
  fi

  log "migrate: codex source_transcript=$source_transcript"

  # For Codex, we need to create the same directory structure under the new cwd
  # Codex stores sessions with cwd encoded in the session_meta (first line)
  # We'll update the cwd in the first line and move the file

  # Read the first line (session metadata)
  local first_line=""
  local rest_of_file=""
  first_line="$(head -n1 "$source_transcript" 2>/dev/null)"
  rest_of_file="$(tail -n+2 "$source_transcript" 2>/dev/null)"

  if [[ -z "$first_line" ]]; then
    log "migrate: FATAL codex session file is empty: $source_transcript"
    exit 4
  fi

  # Update the cwd in the first line (session_meta JSON)
  # Assuming it's JSON format with "cwd" field
  local updated_first_line=""
  if command -v jq >/dev/null 2>&1; then
    # Use jq if available (with -c for compact output to preserve original format)
    updated_first_line="$(printf '%s' "$first_line" | jq -c --arg cwd "$target_dir" '.cwd = $cwd' 2>/dev/null)"
    if [[ -z "$updated_first_line" ]]; then
      # jq failed, just use original
      log "migrate: jq failed to update cwd, using original"
      updated_first_line="$first_line"
    fi
  else
    # Manual JSON update (simple sed approach for "cwd" field)
    updated_first_line="$(printf '%s' "$first_line" | sed "s|\"cwd\":\"[^\"]*\"|\"cwd\":\"$target_dir\"|")"
  fi

  # Determine target session file path: codex uses YYYY/MM/DD structure based on date
  local today_yyyy_mm_dd="$(date '+%Y/%m/%d')"
  local target_session_dir="$codex_home/sessions/$today_yyyy_mm_dd"

  if [[ ! -d "$target_session_dir" ]]; then
    log "migrate: creating codex session directory: $target_session_dir"
    if ! mkdir -p "$target_session_dir"; then
      log "migrate: FATAL could not create codex session directory"
      exit 4
    fi
  fi

  # Use same filename format (we'll keep the session id naming)
  local target_transcript="$target_session_dir/$sid.jsonl"

  # If file exists at target, remove it
  if [[ -f "$target_transcript" ]]; then
    log "migrate: removing existing target codex session: $target_transcript"
    if ! rm -f "$target_transcript"; then
      log "migrate: FATAL could not remove existing target codex session"
      exit 4
    fi
  fi

  # Write the updated session to the target location
  {
    printf '%s\n' "$updated_first_line"
    if [[ -n "$rest_of_file" ]]; then
      printf '%s' "$rest_of_file"
    fi
  } > "$target_transcript" 2>/dev/null

  if [[ ! -f "$target_transcript" ]]; then
    log "migrate: FATAL could not write to target codex session: $target_transcript"
    exit 4
  fi

  # Remove source transcript (we've already moved it by rewriting)
  if ! rm -f "$source_transcript"; then
    log "migrate: warning could not remove source codex session: $source_transcript (but target was created)"
  fi

  log "migrate: SUCCESS (codex) sid=$sid updated cwd to $target_dir and moved to $target_transcript"
  exit 0
}

# ─────────────────────────────────────────────────────────────────────────────
# PI runtime migration
# ─────────────────────────────────────────────────────────────────────────────
migrate_pi() {
  local target_dir="$1"
  local sid="$2"

  local pi_home="${PI_HOME:-$HOME/.pi}"
  local sessions_dir="$pi_home/agent/sessions"

  if [[ ! -d "$sessions_dir" ]]; then
    log "migrate: FATAL pi sessions directory not found: $sessions_dir"
    exit 2
  fi

  # PI stores sessions in <cwd-slug>/<timestamp>_<uuid>.jsonl
  # Find the source session using find for reliability
  local source_transcript=""
  source_transcript="$(find "$sessions_dir" -type f -name "*$sid*" 2>/dev/null | head -1)"

  if [[ -z "$source_transcript" ]]; then
    log "migrate: FATAL could not find pi session file for SID $sid in $sessions_dir"
    exit 2
  fi

  # Get the source slug from the directory name
  local source_slug_dir="$(dirname "$source_transcript")"
  local source_slug="$(basename "$source_slug_dir")"

  log "migrate: pi source_slug=$source_slug source_transcript=$source_transcript"

  # Compute target slug
  local target_slug
  if ! target_slug="$(slug_from_path "$target_dir")"; then
    log "migrate: FATAL could not compute target slug"
    exit 1
  fi

  # Create target slug directory if needed
  local target_slug_dir="$sessions_dir/$target_slug"

  if [[ ! -d "$target_slug_dir" ]]; then
    log "migrate: creating pi target slug directory: $target_slug_dir"
    if ! mkdir -p "$target_slug_dir"; then
      log "migrate: FATAL could not create pi target slug directory"
      exit 4
    fi
  fi

  # Read the session file and update the cwd field
  local first_line=""
  local rest_of_file=""
  first_line="$(head -n1 "$source_transcript" 2>/dev/null)"
  rest_of_file="$(tail -n+2 "$source_transcript" 2>/dev/null)"

  if [[ -z "$first_line" ]]; then
    log "migrate: FATAL pi session file is empty: $source_transcript"
    exit 4
  fi

  # Update cwd in metadata
  local updated_first_line=""
  if command -v jq >/dev/null 2>&1; then
    updated_first_line="$(printf '%s' "$first_line" | jq -c --arg cwd "$target_dir" '.cwd = $cwd' 2>/dev/null)"
    if [[ -z "$updated_first_line" ]]; then
      log "migrate: jq failed to update cwd, using original"
      updated_first_line="$first_line"
    fi
  else
    updated_first_line="$(printf '%s' "$first_line" | sed "s|\"cwd\":\"[^\"]*\"|\"cwd\":\"$target_dir\"|")"
  fi

  local target_transcript="$target_slug_dir/$sid.jsonl"

  # If file exists at target, remove it
  if [[ -f "$target_transcript" ]]; then
    log "migrate: removing existing target pi session: $target_transcript"
    if ! rm -f "$target_transcript"; then
      log "migrate: FATAL could not remove existing target pi session"
      exit 4
    fi
  fi

  # Write the updated session to the target location
  {
    printf '%s\n' "$updated_first_line"
    if [[ -n "$rest_of_file" ]]; then
      printf '%s' "$rest_of_file"
    fi
  } > "$target_transcript" 2>/dev/null

  if [[ ! -f "$target_transcript" ]]; then
    log "migrate: FATAL could not write to target pi session: $target_transcript"
    exit 4
  fi

  # Remove source session
  if ! rm -f "$source_transcript"; then
    log "migrate: warning could not remove source pi session: $source_transcript (but target was created)"
  fi

  log "migrate: SUCCESS (pi) sid=$sid updated cwd to $target_dir, renamed slug $source_slug -> $target_slug"
  exit 0
}

# ─────────────────────────────────────────────────────────────────────────────
# JCODE runtime migration
# ─────────────────────────────────────────────────────────────────────────────
migrate_jcode() {
  local target_dir="$1"
  local sid="$2"

  local jcode_home="${JCODE_HOME:-$HOME/.jcode}"
  local sessions_dir="$jcode_home/sessions"

  if [[ ! -d "$sessions_dir" ]]; then
    log "migrate: FATAL jcode sessions directory not found: $sessions_dir"
    exit 2
  fi

  # JCODE stores sessions as session_<name>_<ts>_<id>.json or similar
  # The SID is typically the identifier used
  local source_transcript=""
  source_transcript="$(find "$sessions_dir" -type f -name "*$sid*.json" 2>/dev/null | head -1)"

  if [[ -z "$source_transcript" ]]; then
    log "migrate: FATAL could not find jcode session file for SID $sid in $sessions_dir"
    exit 2
  fi

  log "migrate: jcode source_transcript=$source_transcript"

  # Similar to Codex, JCODE uses JSON metadata with cwd field
  local first_line=""
  local rest_of_file=""
  first_line="$(head -n1 "$source_transcript" 2>/dev/null)"
  rest_of_file="$(tail -n+2 "$source_transcript" 2>/dev/null)"

  if [[ -z "$first_line" ]]; then
    log "migrate: FATAL jcode session file is empty: $source_transcript"
    exit 4
  fi

  # Update cwd in metadata
  local updated_first_line=""
  if command -v jq >/dev/null 2>&1; then
    updated_first_line="$(printf '%s' "$first_line" | jq -c --arg cwd "$target_dir" '.cwd = $cwd' 2>/dev/null)"
    if [[ -z "$updated_first_line" ]]; then
      log "migrate: jq failed to update cwd, using original"
      updated_first_line="$first_line"
    fi
  else
    updated_first_line="$(printf '%s' "$first_line" | sed "s|\"cwd\":\"[^\"]*\"|\"cwd\":\"$target_dir\"|")"
  fi

  # Use the same filename (JCODE doesn't seem to use cwd-based slug in path like PI)
  local target_transcript="$sessions_dir/$(basename "$source_transcript")"

  # Check if source and target are the same (they usually are for JCODE)
  if [[ "$source_transcript" == "$target_transcript" ]]; then
    # For JCODE, sessions stay in the same directory, we just update the cwd in-place
    log "migrate: jcode session in same directory, updating in-place"
    {
      printf '%s\n' "$updated_first_line"
      if [[ -n "$rest_of_file" ]]; then
        printf '%s' "$rest_of_file"
      fi
    } > "$target_transcript" 2>/dev/null

    if [[ ! -f "$target_transcript" ]]; then
      log "migrate: FATAL could not write to target jcode session: $target_transcript"
      exit 4
    fi
  else
    # If file exists at target, remove it
    if [[ -f "$target_transcript" ]]; then
      log "migrate: removing existing target jcode session: $target_transcript"
      if ! rm -f "$target_transcript"; then
        log "migrate: FATAL could not remove existing target jcode session"
        exit 4
      fi
    fi

    # Write the updated session
    {
      printf '%s\n' "$updated_first_line"
      if [[ -n "$rest_of_file" ]]; then
        printf '%s' "$rest_of_file"
      fi
    } > "$target_transcript" 2>/dev/null

    if [[ ! -f "$target_transcript" ]]; then
      log "migrate: FATAL could not write to target jcode session: $target_transcript"
      exit 4
    fi

    # Remove source session
    if ! rm -f "$source_transcript"; then
      log "migrate: warning could not remove source jcode session: $source_transcript (but target was created)"
    fi
  fi

  log "migrate: SUCCESS (jcode) sid=$sid updated cwd to $target_dir"
  exit 0
}

# ─────────────────────────────────────────────────────────────────────────────
# Dispatch to runtime-specific handler
# ─────────────────────────────────────────────────────────────────────────────
case "$AGENT_TYPE" in
  claude)
    migrate_claude "$TARGET_DIR" "$SID"
    ;;
  codex)
    migrate_codex "$TARGET_DIR" "$SID"
    ;;
  pi)
    migrate_pi "$TARGET_DIR" "$SID"
    ;;
  jcode)
    migrate_jcode "$TARGET_DIR" "$SID"
    ;;
  cx)
    # "cx" is a display label for Claude Code, treat as claude
    migrate_claude "$TARGET_DIR" "$SID"
    ;;
  *)
    log "migrate: FATAL unsupported runtime: $AGENT_TYPE"
    echo "Unsupported runtime: $AGENT_TYPE" >&2
    exit 5
    ;;
esac
