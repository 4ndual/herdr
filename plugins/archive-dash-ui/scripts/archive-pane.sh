#!/usr/bin/env bash
# archive-pane.sh — close the CURRENT (focused) pane and SAVE it into the archive
# store, then refresh the widget's input file.
#
# Invoked by the `archive` plugin action (detached via nohup). Steps:
#   1. Resolve the pane: HERDR_PANE_ID (set by the action env), else the focused
#      pane from `herdr pane list`.
#   2. Read agent / session / cwd / workspace / tab-label via `herdr pane get`.
#   3. Generate a short résumé (claude/cx: first+last user message from the
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
log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG" 2>/dev/null; }

# ── resolve binary + jq ───────────────────────────────────────────────────────
HERDR=""
for cand in "${HERDR_BIN:-}" "${HERDR_BIN_PATH:-}" "$(command -v herdr 2>/dev/null)" \
  "$HOME/Projects/herdr/target/release/herdr" "$HOME/.local/bin/herdr"; do
  if [[ -n "$cand" && -x "$cand" ]]; then HERDR="$cand"; break; fi
done
[[ -z "$HERDR" ]] && { log "FATAL: no herdr binary"; exit 3; }
if ! command -v jq >/dev/null 2>&1; then
  for j in /opt/homebrew/bin/jq /usr/local/bin/jq /usr/bin/jq; do
    [[ -x "$j" ]] && { export PATH="$(dirname "$j"):$PATH"; break; }
  done
fi
command -v jq >/dev/null 2>&1 || { log "FATAL: jq not found"; exit 3; }
hd() { "$HERDR" "$@"; }

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
PANE="${HERDR_PANE_ID:-}"
if [[ -z "$PANE" || "$PANE" == "null" ]]; then
  PANE="$(hd pane list 2>/dev/null | jq -r '.result.panes[] | select(.focused) | .pane_id' | head -1)"
fi
[[ -z "$PANE" || "$PANE" == "null" ]] && { log "FATAL: no pane to archive"; exit 4; }
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

# ── 2b. recover claude/cx session id if not reported yet ──────────────────────
CLAUDE_BASES=(
  "$HOME/.claude-code-router/profiles/default-claude-code/claude"
  "$HOME/.claude"
)
ENC="$(printf '%s' "$CWD" | LC_ALL=C sed 's/[^A-Za-z0-9]/-/g')"
if [[ -z "$VALUE" && "$AGENT" == "claude" ]]; then
  for base in "${CLAUDE_BASES[@]}"; do
    newest="$(ls -t "$base/projects/$ENC/"*.jsonl 2>/dev/null | head -1)"
    if [[ -n "$newest" ]]; then
      VALUE="$(basename "$newest" .jsonl)"; KIND="id"
      log "recovered claude session $VALUE from $base"; break
    fi
  done
fi

# ── 3. résumé ─────────────────────────────────────────────────────────────────
DATE_NOW="$(date '+%Y-%m-%d %H:%M')"
RESUME=""
if [[ "$AGENT" == "claude" && -n "$VALUE" ]]; then
  for base in "${CLAUDE_BASES[@]}"; do
    JSONL="$base/projects/$ENC/$VALUE.jsonl"
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
      [[ -n "$RESUME" ]] && { log "résumé from $JSONL"; break; }
    fi
  done
fi
if [[ -z "$RESUME" ]]; then
  RESUME="$AGENT session in $(basename "$CWD") — archived $DATE_NOW"
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
  || { log "FATAL: could not append record"; exit 5; }
log "record appended (ts=$TS label=$LABEL)"

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
