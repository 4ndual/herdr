#!/usr/bin/env bash
# relocate-conversation.sh — move the CURRENT pane's Claude conversation to the
# project it is ACTUALLY about, and continue it there.
#
# The "Relocate" button runs this. It fixes "I started Claude in the wrong
# directory": Claude Code has no mid-session `cd`, so the only reliable fix is to
# migrate the session on disk into the target directory's project slug and
# relaunch `--resume` there. That machinery already exists and is not
# reimplemented here — this script drives the `relocate` skill's
# `migrate-session.sh` (see ~/.claude/skills/relocate/).
#
# WHAT "THE CORRECT PROJECT" MEANS (the inference, stated):
#   The correct project is the git repository the conversation actually WORKED
#   IN, measured as the number of distinct existing paths under it that the
#   transcript mentions. A session launched in ~ that touched 22 distinct files
#   under ~/Projects/herdr is a herdr session that started in the wrong place.
#   The pane's cwd is deliberately NOT the answer — it is the thing that is
#   wrong. It is excluded from the candidates.
#
# The ranking is EVIDENCE, not a decision. Relocating is destructive-ish and the
# top candidate is often a near-tie (measured: 42/36/32/29 on a real transcript),
# so this script never picks. It opens a tab listing the candidates with their
# scores and lets the user choose — see relocate-pick.sh.
#
# Every failure path toasts. A silent no-op is how a broken button hides.

set -u

LOG="${HERDR_RELOCATE_LOG:-$HOME/.herdr-relocate.log}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=dash-common.sh
. "$SCRIPT_DIR/dash-common.sh" || { echo "relocate: dash-common.sh missing" >&2; exit 3; }

PICK_SCRIPT="$SCRIPT_DIR/relocate-pick.sh"

if [[ -z "$HERDR" ]]; then
  log "FATAL: no herdr binary found"
  desktop_notify "Relocate failed" "no herdr binary found"
  exit 3
fi

command -v jq >/dev/null 2>&1 || { toast "Relocate failed" "jq is not installed"; exit 3; }

# The migration itself belongs to the relocate skill. Overridable, absolute
# default — never a relative path escape. Fallback to plugin-bundled copy.
MIGRATE="${HERDR_RELOCATE_MIGRATE:-${CLAUDE_CONFIG_DIR:-$HOME/.claude}/skills/relocate/scripts/migrate-session.sh}"
if [[ ! -f "$MIGRATE" ]]; then
  PLUGIN_FALLBACK="$SCRIPT_DIR/migrate-session.sh"
  if [[ -f "$PLUGIN_FALLBACK" ]]; then
    MIGRATE="$PLUGIN_FALLBACK"
    log "using plugin-bundled migrate-session.sh: $MIGRATE"
  else
    toast "Relocate unavailable" \
      "the relocate skill is not installed — expected migrate-session.sh at $MIGRATE (override with HERDR_RELOCATE_MIGRATE)"
    exit 3
  fi
fi
[[ -x "$PICK_SCRIPT" ]] || { toast "Relocate failed" "relocate-pick.sh missing or not executable"; exit 3; }

# ---------------------------------------------------------------------------
# 1. Resolve the SOURCE pane (button: HERDR_PANE_ID; tab context: focused pane).
# ---------------------------------------------------------------------------
if ! PANE="$(current_pane)"; then
  toast "Relocate failed" "could not resolve the current pane"
  exit 4
fi

if ! PANE_JSON="$(hd pane get "$PANE" 2>/dev/null)" || [[ -z "$PANE_JSON" ]]; then
  toast "Relocate failed" "could not read the current pane"
  exit 4
fi
AGENT="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.agent // empty')"
SID="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.agent_session.value // empty')"
CWD="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.cwd // .result.pane.foreground_cwd // empty')"
# A pane reports BOTH cwd and foreground_cwd and they demonstrably disagree (the
# shell's dir vs the running process's). Either one can be "the wrong place" the
# user wants out of, so BOTH are excluded from the candidates rather than
# guessing which is authoritative.
FG_CWD="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.foreground_cwd // empty')"
WS="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.workspace_id // empty')"
[[ -z "$CWD" ]] && CWD="$HOME"
[[ -z "$WS" ]] && WS="${HERDR_WORKSPACE_ID:-}"
log "pane=$PANE agent=$AGENT sid=$SID cwd=$CWD"

# ---------------------------------------------------------------------------
# 2. Check if there is an agent. Relocation works across all runtimes now.
# ---------------------------------------------------------------------------
if [[ -z "$AGENT" ]]; then
  toast "Relocate: no agent here" "This pane has no conversation to relocate."
  exit 0
fi

# ---------------------------------------------------------------------------
# LAYER 1: HR-Reported Session ID (most trustworthy)
# Check all possible fields where HR might report the SID
# ---------------------------------------------------------------------------
if [[ -z "$SID" ]]; then
  # Try multiple possible field names that HR might use
  SID="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.agent_session.value // .result.pane.session_id // .result.pane.sid // .result.pane.agent_session // empty' 2>/dev/null)"
  [[ -n "$SID" && "$SID" != "null" ]] && log "LAYER 1 (HR-Reported): SID=$SID" && DISCOVERY_LAYER="layer1"
fi

# ---------------------------------------------------------------------------
# LAYER 2: Process Truth (second most trustworthy)
# Get foreground pid, read /proc/<pid>/environ, walk /proc/<pid>/fd
# ---------------------------------------------------------------------------
if [[ -z "$SID" || "$DISCOVERY_LAYER" != "layer1" ]]; then
  FG_PID="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.foreground_pid // empty' 2>/dev/null)"
  if [[ -n "$FG_PID" && "$FG_PID" =~ ^[0-9]+$ ]]; then
    log "LAYER 2: Checking foreground pid=$FG_PID"

    # Try to get session from /proc/<pid>/fd open files
    LAYER2_SID=""
    LAYER2_HOMES=""

    # Read environment to find home directories
    if [[ -r "/proc/$FG_PID/environ" ]]; then
      # Parse null-terminated environ carefully
      CODEX_HOME_FROM_PID="$(tr '\0' '\n' < "/proc/$FG_PID/environ" 2>/dev/null | grep '^CODEX_HOME=' | cut -d= -f2-)"
      JCODE_HOME_FROM_PID="$(tr '\0' '\n' < "/proc/$FG_PID/environ" 2>/dev/null | grep '^JCODE_HOME=' | cut -d= -f2-)"
      PI_HOME_FROM_PID="$(tr '\0' '\n' < "/proc/$FG_PID/environ" 2>/dev/null | grep '^PI_HOME=' | cut -d= -f2-)"

      [[ -n "$CODEX_HOME_FROM_PID" ]] && LAYER2_HOMES+="CODEX:$CODEX_HOME_FROM_PID "
      [[ -n "$JCODE_HOME_FROM_PID" ]] && LAYER2_HOMES+="JCODE:$JCODE_HOME_FROM_PID "
      [[ -n "$PI_HOME_FROM_PID" ]] && LAYER2_HOMES+="PI:$PI_HOME_FROM_PID "
    fi

    # Walk /proc/<pid>/fd to find open session files
    if [[ -d "/proc/$FG_PID/fd" ]]; then
      for fd in /proc/$FG_PID/fd/*; do
        [[ ! -e "$fd" ]] && continue
        session_path="$(readlink "$fd" 2>/dev/null)" || continue

        # Match codex session pattern
        if [[ "$session_path" =~ /codex/sessions/.*\.jsonl$ ]] && [[ -z "$LAYER2_SID" ]]; then
          LAYER2_SID="$(jq -r '.payload.session_id // .session_id // ""' "$session_path" 2>/dev/null)"
          [[ -n "$LAYER2_SID" ]] && log "LAYER 2 (Process Truth): Found codex session via fd: SID=$LAYER2_SID" && DISCOVERY_LAYER="layer2" && break
        fi

        # Match jcode session pattern
        if [[ "$session_path" =~ /jcode/sessions/.*\.json$ ]] && [[ -z "$LAYER2_SID" ]]; then
          LAYER2_SID="$(basename "$session_path" .json | rev | cut -d_ -f1 | rev)"
          [[ -n "$LAYER2_SID" ]] && log "LAYER 2 (Process Truth): Found jcode session via fd: SID=$LAYER2_SID" && DISCOVERY_LAYER="layer2" && break
        fi

        # Match pi session pattern
        if [[ "$session_path" =~ /\.pi/agent/sessions/.*\.jsonl$ ]] && [[ -z "$LAYER2_SID" ]]; then
          LAYER2_SID="$(jq -r '.id // ""' "$session_path" 2>/dev/null)"
          [[ -n "$LAYER2_SID" ]] && log "LAYER 2 (Process Truth): Found pi session via fd: SID=$LAYER2_SID" && DISCOVERY_LAYER="layer2" && break
        fi
      done
    fi

    # Check /proc/<pid>/children if direct pid is a shell
    if [[ -z "$LAYER2_SID" && -f "/proc/$FG_PID/comm" ]]; then
      comm="$(cat "/proc/$FG_PID/comm" 2>/dev/null)"
      if [[ "$comm" =~ ^(bash|sh|zsh|fish|tcsh)$ ]]; then
        # This is a shell, check children
        if [[ -f "/proc/$FG_PID/task/$FG_PID/children" ]]; then
          for child_pid in $(cat "/proc/$FG_PID/task/$FG_PID/children" 2>/dev/null); do
            [[ -d "/proc/$child_pid/fd" ]] || continue
            for fd in /proc/$child_pid/fd/*; do
              [[ ! -e "$fd" ]] && continue
              session_path="$(readlink "$fd" 2>/dev/null)" || continue

              if [[ "$session_path" =~ /codex/sessions/.*\.jsonl$ ]]; then
                LAYER2_SID="$(jq -r '.payload.session_id // .session_id // ""' "$session_path" 2>/dev/null)"
                [[ -n "$LAYER2_SID" ]] && log "LAYER 2 (Process Truth): Found codex session via child fd: SID=$LAYER2_SID" && DISCOVERY_LAYER="layer2" && break 2
              fi

              if [[ "$session_path" =~ /jcode/sessions/.*\.json$ ]]; then
                LAYER2_SID="$(basename "$session_path" .json | rev | cut -d_ -f1 | rev)"
                [[ -n "$LAYER2_SID" ]] && log "LAYER 2 (Process Truth): Found jcode session via child fd: SID=$LAYER2_SID" && DISCOVERY_LAYER="layer2" && break 2
              fi

              if [[ "$session_path" =~ /\.pi/agent/sessions/.*\.jsonl$ ]]; then
                LAYER2_SID="$(jq -r '.id // ""' "$session_path" 2>/dev/null)"
                [[ -n "$LAYER2_SID" ]] && log "LAYER 2 (Process Truth): Found pi session via child fd: SID=$LAYER2_SID" && DISCOVERY_LAYER="layer2" && break 2
              fi
            done
          done
        fi
      fi
    fi

    [[ -n "$LAYER2_SID" ]] && SID="$LAYER2_SID"
  fi
fi

# ---------------------------------------------------------------------------
# LAYER 3: Filesystem Heuristic (least trustworthy, requires picker confirmation)
# Newest session file whose recorded cwd matches pane cwd, or newest overall
# ---------------------------------------------------------------------------
if [[ -z "$SID" ]]; then
  log "LAYER 3: Starting filesystem heuristic for agent=$AGENT"
  GUESSED_SID=""

  case "$AGENT" in
    claude|cx)
      # For Claude, try all roots (both default and harness)
      while IFS= read -r CLAUDE_DIR; do
        [[ -n "$CLAUDE_DIR" ]] || continue
        # Try cwd match first
        while IFS= read -r session_file; do
          [[ -f "$session_file" ]] || continue
          session_cwd=$(jq -r '.[] | select(.payload.cwd or .cwd) | .payload.cwd // .cwd // ""' "$session_file" 2>/dev/null | head -1)
          if [[ "$session_cwd" == "$CWD" || "$session_cwd" == "$FG_CWD" ]]; then
            GUESSED_SID=$(jq -r '.[] | select(.payload.session_id or .session_id) | .payload.session_id // .session_id // ""' "$session_file" 2>/dev/null | head -1)
            [[ -n "$GUESSED_SID" ]] && log "LAYER 3 (Filesystem): Found claude session with cwd match: SID=$GUESSED_SID" && DISCOVERY_LAYER="layer3_cwd" && break
          fi
        done < <(find "$CLAUDE_DIR/projects" -maxdepth 2 -type f -name "*.jsonl" 2>/dev/null | xargs ls -t 2>/dev/null)

        [[ -n "$GUESSED_SID" ]] && break
      done < <(claude_roots)

      # If no cwd match, take the newest overall
      if [[ -z "$GUESSED_SID" ]]; then
        while IFS= read -r CLAUDE_DIR; do
          [[ -n "$CLAUDE_DIR" ]] || continue
          newest_session=$(find "$CLAUDE_DIR/projects" -maxdepth 2 -type f -name "*.jsonl" 2>/dev/null | xargs ls -t 2>/dev/null | head -1)
          if [[ -n "$newest_session" ]]; then
            GUESSED_SID=$(jq -r '.[] | select(.payload.session_id or .session_id) | .payload.session_id // .session_id // ""' "$newest_session" 2>/dev/null | head -1)
            [[ -n "$GUESSED_SID" ]] && log "LAYER 3 (Filesystem): Found claude session (newest): SID=$GUESSED_SID" && DISCOVERY_LAYER="layer3_newest" && break
          fi
        done < <(claude_roots)
      fi
      ;;
    codex)
      CODEX_HOME="${CODEX_HOME:-${CLAUDE_CONFIG_DIR:-$HOME/.claude}/../codex}"
      if [[ -d "$CODEX_HOME/sessions" ]]; then
        # Try cwd match first
        while IFS= read -r session_file; do
          [[ -f "$session_file" ]] || continue
          session_cwd=$(jq -r '.payload.cwd // .cwd // ""' "$session_file" 2>/dev/null)
          if [[ "$session_cwd" == "$CWD" || "$session_cwd" == "$FG_CWD" ]]; then
            GUESSED_SID=$(jq -r '.payload.session_id // .session_id // ""' "$session_file" 2>/dev/null)
            [[ -n "$GUESSED_SID" ]] && log "LAYER 3 (Filesystem): Found codex session with cwd match: SID=$GUESSED_SID" && DISCOVERY_LAYER="layer3_cwd" && break
          fi
        done < <(find "$CODEX_HOME/sessions" -type f -name "*.jsonl" 2>/dev/null | xargs ls -t 2>/dev/null)

        # If no cwd match, take the newest overall
        if [[ -z "$GUESSED_SID" ]]; then
          newest_session=$(find "$CODEX_HOME/sessions" -type f -name "*.jsonl" 2>/dev/null | xargs ls -t 2>/dev/null | head -1)
          if [[ -n "$newest_session" ]]; then
            GUESSED_SID=$(jq -r '.payload.session_id // .session_id // ""' "$newest_session" 2>/dev/null)
            log "LAYER 3 (Filesystem): Found codex session (newest): SID=$GUESSED_SID"
            DISCOVERY_LAYER="layer3_newest"
          fi
        fi
      fi
      ;;
    pi)
      PI_HOME="${PI_HOME:-$HOME/.pi}"
      if [[ -d "$PI_HOME/agent/sessions" ]]; then
        # Try cwd match first
        while IFS= read -r session_file; do
          [[ -f "$session_file" ]] || continue
          session_cwd=$(jq -r '.cwd // ""' "$session_file" 2>/dev/null)
          if [[ "$session_cwd" == "$CWD" || "$session_cwd" == "$FG_CWD" ]]; then
            GUESSED_SID=$(jq -r '.id // ""' "$session_file" 2>/dev/null)
            [[ -n "$GUESSED_SID" ]] && log "LAYER 3 (Filesystem): Found pi session with cwd match: SID=$GUESSED_SID" && DISCOVERY_LAYER="layer3_cwd" && break
          fi
        done < <(find "$PI_HOME/agent/sessions" -type f -name "*.jsonl" 2>/dev/null | xargs ls -t 2>/dev/null)

        # If no cwd match, take the newest overall
        if [[ -z "$GUESSED_SID" ]]; then
          newest_session=$(find "$PI_HOME/agent/sessions" -type f -name "*.jsonl" 2>/dev/null | xargs ls -t 2>/dev/null | head -1)
          if [[ -n "$newest_session" ]]; then
            GUESSED_SID=$(jq -r '.id // ""' "$newest_session" 2>/dev/null)
            log "LAYER 3 (Filesystem): Found pi session (newest): SID=$GUESSED_SID"
            DISCOVERY_LAYER="layer3_newest"
          fi
        fi
      fi
      ;;
    jcode)
      JCODE_HOME="${JCODE_HOME:-$HOME/.jcode}"
      if [[ -d "$JCODE_HOME/sessions" ]]; then
        # Try cwd match first
        while IFS= read -r session_file; do
          [[ -f "$session_file" ]] || continue
          session_cwd=$(jq -r '.cwd // ""' "$session_file" 2>/dev/null)
          if [[ "$session_cwd" == "$CWD" || "$session_cwd" == "$FG_CWD" ]]; then
            GUESSED_SID=$(basename "$session_file" .json | rev | cut -d_ -f1 | rev)
            [[ -n "$GUESSED_SID" ]] && log "LAYER 3 (Filesystem): Found jcode session with cwd match: SID=$GUESSED_SID" && DISCOVERY_LAYER="layer3_cwd" && break
          fi
        done < <(find "$JCODE_HOME/sessions" -type f -name "session_*.json" 2>/dev/null | xargs ls -t 2>/dev/null)

        # If no cwd match, take the newest overall
        if [[ -z "$GUESSED_SID" ]]; then
          newest_session=$(find "$JCODE_HOME/sessions" -type f -name "session_*.json" 2>/dev/null | xargs ls -t 2>/dev/null | head -1)
          if [[ -n "$newest_session" ]]; then
            GUESSED_SID=$(basename "$newest_session" .json | rev | cut -d_ -f1 | rev)
            log "LAYER 3 (Filesystem): Found jcode session (newest): SID=$GUESSED_SID"
            DISCOVERY_LAYER="layer3_newest"
          fi
        fi
      fi
      ;;
  esac

  if [[ -n "$GUESSED_SID" ]]; then
    SID="$GUESSED_SID"
  fi
fi

# ---------------------------------------------------------------------------
# Validate that we have an SID
# ---------------------------------------------------------------------------
if [[ -z "$SID" ]]; then
  case "$AGENT" in
    claude|cx)
      toast "Relocate: session not yet reported" "hr hasn't reported the session id for this pane yet. This usually happens after the agent completes a turn. Try again after the next message."
      log "FAILED: no sid found for claude pane via any layer"
      exit 0
      ;;
    *)
      toast "Relocate: session not found" "Could not discover a session ID for this pane."
      log "DISCOVERY FAILED: no session found via any layer for agent=$AGENT"
      exit 6
      ;;
  esac
fi

log "SID RESOLVED: sid=$SID (layer=$DISCOVERY_LAYER)"

# ---------------------------------------------------------------------------
# 3. Find the transcript under the single Claude Code projects root. This also
#    names the projects root the migration must write into, which the picker
#    passes through to migrate-session.sh as CLAUDE_PROJECTS_DIR.
# ---------------------------------------------------------------------------
# Every claude root, not just the default: a harness-owned session's transcript
# is under ~/harnesses/<name>/config/projects, and the single-root search this
# replaces refused the relocate with "no transcript on disk" for those panes.
# The root that OWNS the transcript is also the root the migration must write
# into, which is why PROJECTS_DIR is derived from the hit rather than assumed.
PROJECTS_DIR=""
TRANSCRIPT=""
while IFS= read -r CLAUDE_DIR; do
  [[ -n "$CLAUDE_DIR" ]] || continue
  found="$(find "$CLAUDE_DIR/projects" -maxdepth 2 -type f -name "$SID.jsonl" 2>/dev/null | head -1)"
  if [[ -n "$found" ]]; then
    PROJECTS_DIR="$CLAUDE_DIR/projects"
    TRANSCRIPT="$found"
    break
  fi
done < <(claude_roots)
if [[ -z "$TRANSCRIPT" ]]; then
  toast "Relocate failed" "no transcript on disk for session $SID"
  exit 6
fi
log "transcript=$TRANSCRIPT projects_dir=$PROJECTS_DIR"

# ---------------------------------------------------------------------------
# 4. Rank candidate projects by DISTINCT touched paths per project.
#    Distinct paths, not raw mentions: one file quoted 144 times must not
#    outrank a project the conversation actually moved around in. Build
#    artefacts are stripped so a wasm path under target/ credits its repo,
#    not itself. Projects are identified by:
#      1. Directory containing .git (git repositories), OR
#      2. Direct child of ~/Projects or ~/Documents (non-git projects)
#    Fork-free: walking up for .git using shell builtins, no `git` calls.
#    A 21 MB transcript still ranks in ~1s.
# ---------------------------------------------------------------------------
CANDIDATES="$(
  LC_ALL=C grep -o "$HOME/[A-Za-z0-9._/-]*" "$TRANSCRIPT" 2>/dev/null \
  | LC_ALL=C sed -E 's#/(target|node_modules|\.git)(/.*)?$##' \
  | sort -u \
  | while IFS= read -r p; do
      d="$p"
      [[ -d "$d" ]] || d="${p%/*}"
      found=0
      while [[ -n "$d" && "$d" != "/" && "$d" != "$HOME" ]]; do
        # Check for .git (git repository)
        if [[ -e "$d/.git" ]]; then
          printf '%s\n' "$d"
          found=1
          break
        fi
        # Check if d is a direct child of ~/Projects or ~/Documents (non-git project)
        parent="${d%/*}"
        if [[ ("$parent" == "$HOME/Projects" || "$parent" == "$HOME/Documents") && -d "$d" ]]; then
          printf '%s\n' "$d"
          found=1
          break
        fi
        d="${d%/*}"
      done
    done \
  | sort | uniq -c | sort -rn \
  | awk -v cur="$CWD" -v fg="$FG_CWD" 'NF==2 && $2 != cur && ($2 != fg || fg == "") {print $1"\t"$2}' \
  | head -8
)"

if [[ -z "$CANDIDATES" ]]; then
  toast "Relocate: nowhere to go" \
    "No other git project appears in this conversation — it may already be in the right place."
  exit 0
fi
log "candidates:"$'\n'"$CANDIDATES"

# ---------------------------------------------------------------------------
# 5. Hand the choice to the user in a new tab. Never pick here: the scores are
#    routinely a near-tie and relocating the wrong session is annoying to undo.
# ---------------------------------------------------------------------------
CREATE_JSON="$(hd tab create --workspace "$WS" --cwd "$CWD" --label "relocate?" --focus 2>>"$LOG")"
CREATE_RC=$?
ROOT_PANE="$(printf '%s' "$CREATE_JSON" | jq -r '.result.root_pane.pane_id // empty')"
if [[ $CREATE_RC -ne 0 || -z "$ROOT_PANE" || "$ROOT_PANE" == "null" ]]; then
  log "tab create rc=$CREATE_RC response: $CREATE_JSON"
  toast "Relocate failed" "could not create tab"
  exit 8
fi

# Hand the ranked list over on disk — it contains tabs and newlines, and a
# temp file keeps it out of the command line entirely.
PAYLOAD="$(mktemp "${TMPDIR:-/tmp}/herdr-relocate.XXXXXX")" || {
  toast "Relocate failed" "could not create the candidate list"; exit 8; }
{
  printf 'AGENT=%s\n' "$AGENT"
  printf 'SID=%s\n' "$SID"
  printf 'CWD=%s\n' "$CWD"
  printf 'PROJECTS_DIR=%s\n' "$PROJECTS_DIR"
  printf 'PANE=%s\n' "$PANE"
  printf 'DISCOVERY_LAYER=%s\n' "${DISCOVERY_LAYER:-unknown}"
  printf -- '--\n'
  printf '%s\n' "$CANDIDATES"
} >"$PAYLOAD"

sleep 0.4
if ! hd pane run "$ROOT_PANE" "exec $(printf '%q' "$PICK_SCRIPT") $(printf '%q' "$PAYLOAD")" >>"$LOG" 2>&1; then
  toast "Relocate failed" "could not start the picker in the new tab"
  exit 9
fi

log "RELOCATE OK: picker launched in $ROOT_PANE for session $SID"
toast "Relocate" "Pick the correct project in the new tab"
exit 0
