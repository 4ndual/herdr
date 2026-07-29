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
log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG" 2>/dev/null; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PICK_SCRIPT="$SCRIPT_DIR/relocate-pick.sh"

# ---------------------------------------------------------------------------
# 0. Resolve the herdr client binary (prefer the fork build; never assume PATH).
#    PATH LAST, deliberately: `command -v herdr` resolves to ~/.local/bin/herdr,
#    a symlink to a June-19 pre-prod build missing this fork's subcommands.
# ---------------------------------------------------------------------------
HERDR=""
for cand in "${HERDR_BIN:-}" \
  "$HOME/.herdr-hr/bin/herdr" \
  "$HOME/Projects/herdr-worktrees/imgapi-127/target/release/herdr" \
  "$HOME/Projects/herdr/target/release/herdr" \
  "$(command -v herdr 2>/dev/null)"; do
  if [[ -n "$cand" && -x "$cand" ]]; then
    HERDR="$cand"
    break
  fi
done

hd() { "$HERDR" "$@"; }

toast() {
  local title="$1" body="${2:-}"
  log "TOAST: $title — $body"
  [[ -n "$HERDR" ]] || return 0
  hd notification show "$title" ${body:+--body "$body"} >>"$LOG" 2>&1 || true
}

if [[ -z "$HERDR" ]]; then
  log "FATAL: no herdr binary found"
  exit 3
fi

if ! command -v jq >/dev/null 2>&1; then
  for j in /opt/homebrew/bin/jq /usr/local/bin/jq /usr/bin/jq; do
    [[ -x "$j" ]] && { export PATH="$(dirname "$j"):$PATH"; break; }
  done
fi
command -v jq >/dev/null 2>&1 || { toast "Relocate failed" "jq not found"; exit 3; }

# The migration itself belongs to the relocate skill. Overridable, absolute
# default — never a relative path escape.
MIGRATE="${HERDR_RELOCATE_MIGRATE:-$HOME/.claude/skills/relocate/scripts/migrate-session.sh}"
if [[ ! -f "$MIGRATE" ]]; then
  toast "Relocate failed" "relocate skill not installed (migrate-session.sh missing)"
  exit 3
fi
[[ -x "$PICK_SCRIPT" ]] || { toast "Relocate failed" "relocate-pick.sh missing or not executable"; exit 3; }

# ---------------------------------------------------------------------------
# 1. Resolve the SOURCE pane (button: HERDR_PANE_ID; hotkey: focused pane).
# ---------------------------------------------------------------------------
PANE="${HERDR_PANE_ID:-}"
if [[ -z "$PANE" ]]; then
  PANE="$(hd pane list 2>/dev/null | jq -r '.result.panes[] | select(.focused) | .pane_id' | head -1)"
fi
if [[ -z "$PANE" || "$PANE" == "null" ]]; then
  toast "Relocate failed" "could not resolve the current pane"
  exit 4
fi

PANE_JSON="$(hd pane get "$PANE" 2>/dev/null)"
if [[ $? -ne 0 || -z "$PANE_JSON" ]]; then
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
# 2. Gate on the agent kind. Relocation is a Claude Code mechanism (transcript
#    slug + `--resume`); codex/pi store sessions differently.
# ---------------------------------------------------------------------------
case "$AGENT" in
  claude) ;;
  "") toast "Relocate: no agent here" "This pane has no Claude conversation to relocate."; exit 0 ;;
  *)  toast "Relocate not available" "Relocation is a Claude Code mechanism — this pane runs '$AGENT'."; exit 0 ;;
esac

if [[ -z "$SID" ]]; then
  toast "Relocate failed" "no session id reported for this pane yet"
  exit 5
fi

# ---------------------------------------------------------------------------
# 3. Find the transcript. A cx (GPT-via-CCR) session's transcript lives ONLY
#    under the CCR profile dir; herdr labels cx and daily claude identically, so
#    discriminate by WHERE the file is — the same signal cx-resume uses. This
#    also selects the projects root the migration must write into.
# ---------------------------------------------------------------------------
CCR_DIR="${CCR_DIR:-$HOME/.claude-code-router/profiles/default-claude-code/claude}"
PROJECTS_DIR=""
TRANSCRIPT=""
for base in "$HOME/.claude" "$CCR_DIR"; do
  found="$(find "$base/projects" -maxdepth 2 -type f -name "$SID.jsonl" 2>/dev/null | head -1)"
  if [[ -n "$found" ]]; then
    PROJECTS_DIR="$base/projects"
    TRANSCRIPT="$found"
    break
  fi
done
if [[ -z "$TRANSCRIPT" ]]; then
  toast "Relocate failed" "no transcript on disk for session $SID"
  exit 6
fi
log "transcript=$TRANSCRIPT projects_dir=$PROJECTS_DIR"

# ---------------------------------------------------------------------------
# 4. Rank candidate projects by DISTINCT touched paths per git repo.
#    Distinct paths, not raw mentions: one file quoted 144 times must not
#    outrank a repo the conversation actually moved around in. Build artefacts
#    are stripped so a wasm path under target/ credits its repo, not itself.
#    Fork-free: walking up for a .git entry with a shell builtin instead of
#    calling `git` per path took a 21 MB transcript from 10s to 1s.
# ---------------------------------------------------------------------------
CANDIDATES="$(
  LC_ALL=C grep -o "$HOME/[A-Za-z0-9._/-]*" "$TRANSCRIPT" 2>/dev/null \
  | LC_ALL=C sed -E 's#/(target|node_modules|\.git)(/.*)?$##' \
  | sort -u \
  | while IFS= read -r p; do
      d="$p"
      [[ -d "$d" ]] || d="${p%/*}"
      while [[ -n "$d" && "$d" != "/" && "$d" != "$HOME" ]]; do
        if [[ -e "$d/.git" ]]; then printf '%s\n' "$d"; break; fi
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
  printf 'SID=%s\n' "$SID"
  printf 'CWD=%s\n' "$CWD"
  printf 'PROJECTS_DIR=%s\n' "$PROJECTS_DIR"
  printf 'PANE=%s\n' "$PANE"
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
