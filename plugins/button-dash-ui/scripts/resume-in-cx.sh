#!/usr/bin/env bash
# resume-in-cx.sh — fork the CURRENT claude pane's conversation into a NEW TAB
# running UNDER cx (GPT-via-CCR gateway), leaving the original conversation
# untouched.
#
# The "Resume in cx" button and the ⌃⌥C Karabiner hotkey run this. It is the
# cx-forcing sibling of fork-conversation.sh: where Fork reuses whatever launcher
# the pane's agent already had, Resume-in-cx always brings the conversation up on
# the cx gateway so the GPT/Codex models appear in `/model`.
#
# Two source cases (herdr labels BOTH cx and daily claude as agent="claude"):
#   * cx-origin pane  — transcript lives under the CCR profile dir. `cx-resume`
#     already routes it to cx, so we just fork with cx-resume.
#   * daily claude pane — transcript lives under ~/.claude. `cx-resume` would
#     route it back to PLAIN claude (its dispatch is by transcript location), so
#     to force cx we make the transcript visible to cx (a symlink into the CCR
#     projects dir — non-destructive, the original jsonl is read-only) and launch
#     the cx launcher directly. `--fork-session` gives the fork a fresh id, so
#     the original daily conversation is never mutated.
#
# Non-claude panes (codex / pi / terminal) are not supported and get a toast.
#
# Usage:  resume-in-cx.sh
#   Reads the source pane from the inherited HERDR_PANE_ID (button path) or the
#   focused pane (Karabiner path). Nothing is hardcoded: agent/session/cwd come
#   live from `herdr pane get`.

set -u

LOG="${HERDR_RESUME_CX_LOG:-$HOME/.herdr-resume-cx.log}"
log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG" 2>/dev/null; }

# ---------------------------------------------------------------------------
# 0. Resolve the herdr client binary + jq.
# ---------------------------------------------------------------------------
HERDR=""
# PATH LAST, deliberately: `command -v herdr` resolves to ~/.local/bin/herdr,
# a symlink to a June-19 pre-prod build missing this fork's subcommands. Putting
# it first silently ran the wrong binary. Prefer the live instance binary.
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
if [[ -z "$HERDR" ]]; then
  log "FATAL: no herdr binary found"
  exit 3
fi

if ! command -v jq >/dev/null 2>&1; then
  for j in /opt/homebrew/bin/jq /usr/local/bin/jq /usr/bin/jq; do
    [[ -x "$j" ]] && { export PATH="$(dirname "$j"):$PATH"; break; }
  done
fi
command -v jq >/dev/null 2>&1 || { log "FATAL: jq not found"; exit 3; }

hd() { "$HERDR" "$@"; }

# ---------------------------------------------------------------------------
# INCIDENT FIX (F6): sweep stale bridge symlinks out of the CCR projects tree.
# A bridge is only needed transiently while a fork boots; a lingering symlink
# whose target lives under ~/.claude/projects makes cx-resume mis-dispatch the
# ORIGINAL daily pane as cx on the next hr restart. Remove every such symlink
# (optionally keeping $1) on every run.
# ---------------------------------------------------------------------------
sweep_stale_bridges() {
  local keep="${1:-}" root daily p tgt
  root="${CCR_DIR:-$HOME/.claude-code-router/profiles/default-claude-code/claude}/projects"
  daily="$HOME/.claude/projects"
  [[ -d "$root" ]] || return 0
  while IFS= read -r p; do
    [[ -z "$p" || "$p" == "$keep" ]] && continue
    if [[ -L "$p" ]]; then
      tgt="$(readlink -f "$p" 2>/dev/null || true)"
      if [[ -z "$tgt" || "$tgt" == "$daily"/* ]]; then
        rm -f "$p" 2>/dev/null && log "swept stale bridge: $p"
      fi
    fi
  done < <(find "$root" -maxdepth 2 -type l 2>/dev/null)
}

# Detached reaper: remove bridge symlink $1 once the fork has booted (a real,
# non-symlink transcript appears there) or after $2 seconds grace.
reap_bridge() {
  local bridge="$1" grace="${2:-60}" i
  ( for ((i=0;i<grace;i++)); do
      sleep 1
      [[ -e "$bridge" && ! -L "$bridge" ]] && break
    done
    [[ -L "$bridge" ]] && rm -f "$bridge" 2>/dev/null && log "reaped bridge: $bridge"
  ) >/dev/null 2>&1 &
  disown 2>/dev/null || true
}

toast() {
  local title="$1" body="${2:-}"
  log "TOAST: $title — $body"
  hd notification show "$title" ${body:+--body "$body"} >>"$LOG" 2>&1 || true
}

# ---------------------------------------------------------------------------
# 1. Resolve the SOURCE pane.
# ---------------------------------------------------------------------------
PANE="${HERDR_PANE_ID:-}"
if [[ -z "$PANE" ]]; then
  PANE="$(hd pane list 2>/dev/null | jq -r '.result.panes[] | select(.focused) | .pane_id' | head -1)"
fi
if [[ -z "$PANE" || "$PANE" == "null" ]]; then
  log "FATAL: could not resolve source pane"
  exit 4
fi
log "source pane: $PANE"

# ---------------------------------------------------------------------------
# 2. Read agent, session ref, cwd, workspace.
# ---------------------------------------------------------------------------
PANE_JSON="$(hd pane get "$PANE" 2>/dev/null)"
AGENT="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.agent // empty')"
VALUE="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.agent_session.value // empty')"
CWD="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.cwd // .result.pane.foreground_cwd // empty')"
WS="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.workspace_id // empty')"
TAB_ID="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.tab_id // empty')"
[[ -z "$CWD" ]] && CWD="$HOME"
log "agent=$AGENT value=$VALUE cwd=$CWD ws=$WS"

# Only claude-family panes can be resumed in cx.
if [[ "$AGENT" != "claude" ]]; then
  toast "Resume in cx: not supported" \
    "This pane runs '${AGENT:-no agent}'. Resume-in-cx only works for a claude/cx conversation."
  exit 0
fi

# ---------------------------------------------------------------------------
# 3. Resolve the session id, recovering the newest transcript for this cwd if the
#    pane hasn't reported one yet (check the CCR dir first so a cx-origin session
#    is recognised as such, then the daily dir).
# ---------------------------------------------------------------------------
CCR_DIR="${CCR_DIR:-$HOME/.claude-code-router/profiles/default-claude-code/claude}"
ENC="$(printf '%s' "$CWD" | LC_ALL=C sed 's/[^A-Za-z0-9]/-/g')"

if [[ -z "$VALUE" ]]; then
  for base in "$CCR_DIR" "$HOME/.claude"; do
    newest="$(ls -t "$base/projects/$ENC/"*.jsonl 2>/dev/null | head -1)"
    if [[ -n "$newest" ]]; then
      VALUE="$(basename "$newest" .jsonl)"
      log "recovered claude session $VALUE from $base"
      break
    fi
  done
fi
if [[ -z "$VALUE" ]]; then
  toast "Resume in cx failed" "No session transcript found for this pane's conversation."
  log "FATAL: no session ref for pane $PANE"
  exit 6
fi

CX_JSONL="$CCR_DIR/projects/$ENC/$VALUE.jsonl"
DAILY_JSONL="$HOME/.claude/projects/$ENC/$VALUE.jsonl"

# ---------------------------------------------------------------------------
# 4. Build the fork command that FORCES cx.
#    * cx-origin: cx-resume already routes to cx.
#    * daily: bridge the transcript into the CCR projects dir (symlink; the
#      original stays read-only) and launch the cx launcher directly.
# ---------------------------------------------------------------------------
# De-pollute the CCR tree before every fork (removes prior runs' stale bridges).
sweep_stale_bridges
DAILY_BRIDGE=""
if [[ -f "$CX_JSONL" ]]; then
  FORK_CMD="cx-resume $(printf '%q' "$VALUE") --fork-session"
  log "cx-origin session; forking via cx-resume"
else
  # Daily session: make its transcript visible to cx, then launch cx directly.
  mkdir -p "$CCR_DIR/projects/$ENC" 2>/dev/null || true
  if [[ ! -e "$CX_JSONL" && -f "$DAILY_JSONL" ]]; then
    ln -s "$DAILY_JSONL" "$CX_JSONL" 2>/dev/null \
      && { DAILY_BRIDGE="$CX_JSONL"; log "bridged daily transcript into CCR dir: $CX_JSONL -> $DAILY_JSONL"; } \
      || log "WARN: could not symlink daily transcript into CCR dir"
  fi
  FORK_CMD="cx --resume $(printf '%q' "$VALUE") --fork-session"
  log "daily session; forcing cx via cx launcher"
fi
log "fork cmd: $FORK_CMD"

# ---------------------------------------------------------------------------
# 5. Derive the fork's tab label from the SOURCE tab: "<name> cx" (mirrors the
#    Fork button's "<name> fork"). Re-running keeps a single " cx" suffix.
# ---------------------------------------------------------------------------
SRC_NAME=""
if [[ -n "$TAB_ID" ]]; then
  SRC_NAME="$(hd tab list --workspace "$WS" 2>/dev/null \
    | jq -r --arg t "$TAB_ID" '.result.tabs[]? | select(.tab_id == $t) | .label // empty')"
fi
[[ -z "$SRC_NAME" ]] && SRC_NAME="$(basename "$CWD")"
SRC_NAME="${SRC_NAME% cx}"
if [[ -n "$SRC_NAME" ]]; then
  LABEL="$SRC_NAME cx"
else
  LABEL="cx"
fi
log "fork label: $LABEL"

# ---------------------------------------------------------------------------
# 6. Create the new tab and run the fork in its root pane.
# ---------------------------------------------------------------------------
[[ -z "$WS" ]] && WS="${HERDR_WORKSPACE_ID:-}"
CREATE_JSON="$(hd tab create --workspace "$WS" --cwd "$CWD" --label "$LABEL" --focus 2>/dev/null)"
ROOT_PANE="$(printf '%s' "$CREATE_JSON" | jq -r '.result.root_pane.pane_id // empty')"
if [[ -z "$ROOT_PANE" || "$ROOT_PANE" == "null" ]]; then
  toast "Resume in cx failed" "Could not create a new tab."
  log "FATAL: could not create tab (response: $CREATE_JSON)"
  exit 8
fi
log "destination root pane: $ROOT_PANE"

sleep 0.4
hd pane run "$ROOT_PANE" "exec $FORK_CMD" >>"$LOG" 2>&1
# Fork boots in $ROOT_PANE writing a NEW session id; the daily bridge symlink was
# only needed to let cx FIND the source transcript. Reap it once the fork is up so
# it never lingers to pollute a later hr restart (F6).
if [[ -n "$DAILY_BRIDGE" ]]; then
  reap_bridge "$DAILY_BRIDGE" "${RESUME_PICKER_BRIDGE_GRACE_S:-60}"
fi
log "RESUME-IN-CX OK: $VALUE -> $ROOT_PANE"
exit 0
