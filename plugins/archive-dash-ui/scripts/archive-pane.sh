#!/usr/bin/env bash
# archive-pane.sh — close the CURRENT (focused) pane and SAVE it into the archive
# store, then refresh the widget's input file.
#
# Invoked by the `archive` plugin action (detached via nohup). Steps:
#   1. Resolve the pane: HERDR_PANE_ID (set by the action env), else the focused
#      pane from `herdr pane list`.
#   2. Read agent / session / cwd / workspace / tab-label via `herdr pane get`.
#   3. Generate a short résumé (claude: first+last user message from the
#      session jsonl; other agents: "agent + cwd + date").
#   4. Append a record to ~/.herdr-archive/archive.json (array; created if
#      missing; full history kept — the widget shows the last 10).
#   5. Regenerate the widget input file (last 10, newest first).
#   6. Close the pane's TAB.
# Nothing dynamic is hardcoded: binary + socket + pane come from the env.
set -u

ARCHIVE_DIR="$HOME/.herdr-archive"
STORE="$ARCHIVE_DIR/archive.json"
LOG="$ARCHIVE_DIR/archive.log"
mkdir -p "$ARCHIVE_DIR" 2>/dev/null

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=dash-common.sh
. "$SCRIPT_DIR/dash-common.sh" || { echo "archive: dash-common.sh missing" >&2; exit 3; }

if [[ -z "$HERDR" ]]; then
  log "FATAL: no herdr binary"
  desktop_notify "Archive failed" "no herdr binary found"
  exit 3
fi
command -v jq >/dev/null 2>&1 || { toast "Archive failed" "jq is not installed"; exit 3; }

# ── source-of-truth for the widget input file ─────────────────────────────────
STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-}"
regen_input() {
  # Write the last-10 (newest first) into <state_dir>/archive_input.json so the
  # host surfaces it to the guest as HERDR_PLUGIN_UI_INPUT_JSON.
  [[ -z "$STATE_DIR" ]] && { log "WARN: no HERDR_PLUGIN_STATE_DIR; skip input regen"; return; }
  mkdir -p "$STATE_DIR" 2>/dev/null
  local out="$STATE_DIR/archive_input.json"
  local tmp="$STATE_DIR/.archive_input.json.$$.tmp"
  jq -c '
    (sort_by(.ts) | reverse)[0:10]
    | { items: [ range(0; length) as $i | {
        slot: ($i + 1),
        label: (.[$i].label // "session"),
        agent: (.[$i].agent // ""),
        resume: (.[$i].resume // "")
      } ] }
  ' "$STORE" >"$tmp" 2>>"$LOG" && mv -f "$tmp" "$out" \
    && log "input regen -> $out ($(jq '.items|length' "$out" 2>/dev/null) items)" \
    || log "WARN: input regen failed"
}

# ── 1. resolve pane ───────────────────────────────────────────────────────────
if ! PANE="$(current_pane)"; then
  toast "Archive failed" "could not resolve the current pane"
  exit 4
fi
log "archiving pane: $PANE"

# ── 2. read pane metadata ─────────────────────────────────────────────────────
PANE_JSON="$(hd pane get "$PANE" 2>/dev/null)"
AGENT="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.agent // empty')"
KIND="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.agent_session.kind // empty')"
VALUE="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.agent_session.value // empty')"
CWD="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.cwd // .result.pane.foreground_cwd // empty')"
WS="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.workspace_id // empty')"
TAB_ID="$(printf '%s' "$PANE_JSON" | jq -r '.result.pane.tab_id // empty')"
[[ -z "$CWD" ]] && CWD="$HOME"
[[ -z "$WS" ]] && WS="${HERDR_WORKSPACE_ID:-}"
log "agent=$AGENT kind=$KIND value=$VALUE cwd=$CWD ws=$WS tab=$TAB_ID"

# tab/pane label for the chip
LABEL=""
if [[ -n "$TAB_ID" && -n "$WS" ]]; then
  LABEL="$(hd tab list --workspace "$WS" 2>/dev/null \
    | jq -r --arg t "$TAB_ID" '.result.tabs[]? | select(.tab_id == $t) | .label // empty')"
fi
[[ -z "$LABEL" ]] && LABEL="$(basename "$CWD")"
[[ -z "$LABEL" ]] && LABEL="session"

# ── 2b. recover claude session id if not reported yet ─────────────────────────
CLAUDE_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
ENC="$(printf '%s' "$CWD" | LC_ALL=C sed 's/[^A-Za-z0-9]/-/g')"
if [[ -z "$VALUE" && ("$AGENT" == "claude" || "$AGENT" == "cx") ]]; then
  newest="$(ls -t "$CLAUDE_DIR/projects/$ENC/"*.jsonl 2>/dev/null | head -1)"
  if [[ -n "$newest" ]]; then
    VALUE="$(basename "$newest" .jsonl)"; KIND="id"
    log "recovered claude session $VALUE"
  fi
fi

# ── 3. résumé ─────────────────────────────────────────────────────────────────
DATE_NOW="$(date '+%Y-%m-%d %H:%M')"
RESUME=""
# Only Claude Code keeps a readable per-session jsonl at a path we can derive
# from the cwd, so only claude gets a real résumé; the others fall back to the
# generic one-liner below. (This used to loop over several transcript roots to
# cover a gateway profile dir; there is one root.)
if [[ ("$AGENT" == "claude" || "$AGENT" == "cx") && -n "$VALUE" ]]; then
  JSONL="$CLAUDE_DIR/projects/$ENC/$VALUE.jsonl"
  if [[ -f "$JSONL" ]]; then
    RESUME="$(jq -rs '
        [ .[]
          | select((.type? == "user") or (.message.role? == "user"))
          | (.message.content // .content)
          | if type == "string" then .
            elif type == "array" then ([ .[] | select(.type? == "text") | .text ] | join(" "))
            else empty end
          | select(. != null and (. | gsub("^\\s+|\\s+$"; "")) != "")
        ] as $m
        | if ($m | length) == 0 then ""
          elif ($m | length) == 1 then $m[0]
          else ($m[0] + " … " + $m[-1]) end
      ' "$JSONL" 2>>"$LOG")"
    [[ -n "$RESUME" ]] && log "résumé from $JSONL"
  fi
fi
if [[ -z "$RESUME" ]]; then
  # "${AGENT:-terminal}": an agent-less pane produced a leading-space
  # " session in …" chip that read like a truncated string.
  RESUME="${AGENT:-terminal} session in $(basename "$CWD") — archived $DATE_NOW"
fi
# collapse whitespace + cap length
# collapse whitespace + cap at 400 chars (jq slice is codepoint-safe, unlike cut -c)
RESUME="$(printf '%s' "$RESUME" | tr '\n\t' '  ' | jq -Rrs '.[:400]')"
log "résumé: $RESUME"

# ── 4. append record ──────────────────────────────────────────────────────────
[[ -f "$STORE" ]] || printf '[]' >"$STORE"
TS="$(date +%s)"
TMP="$ARCHIVE_DIR/.archive.json.$$.tmp"
jq \
  --arg ts "$TS" --arg label "$LABEL" --arg agent "$AGENT" \
  --arg kind "$KIND" --arg value "$VALUE" --arg cwd "$CWD" \
  --arg ws "$WS" --arg resume "$RESUME" \
  '. + [{
    ts: ($ts | tonumber), label: $label, agent: $agent,
    session_kind: $kind, session_value: $value, cwd: $cwd,
    workspace_id: $ws, resume: $resume
  }]' "$STORE" >"$TMP" 2>>"$LOG" && mv -f "$TMP" "$STORE" \
  || { toast "Archive failed" "could not write the archive store — the pane was left open"; exit 5; }
log "record appended (ts=$TS label=$LABEL)"

# Say so when the chip will not be restorable. The pane is still archived and
# closed (that is the button's job), but a chip that can only ever answer "no
# session to resume" should not be a surprise ten minutes from now.
if [[ -z "$VALUE" ]]; then
  toast "Archive saved without a session" \
    "'$LABEL' had no resumable agent session, so its chip records the cwd only."
fi

# ── 5. regenerate widget input ────────────────────────────────────────────────
regen_input

# ── 6. close the tab (fallback: close the pane) ───────────────────────────────
if [[ -n "$TAB_ID" ]]; then
  hd tab close "$TAB_ID" >>"$LOG" 2>&1 && log "tab closed: $TAB_ID" \
    || { log "tab close failed; trying pane close"; hd pane close "$PANE" >>"$LOG" 2>&1; }
else
  hd pane close "$PANE" >>"$LOG" 2>&1 && log "pane closed: $PANE"
fi
log "ARCHIVE OK: $AGENT $VALUE ($LABEL)"
exit 0
