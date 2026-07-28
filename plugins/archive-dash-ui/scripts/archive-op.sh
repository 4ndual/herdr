#!/usr/bin/env bash
# archive-op.sh <restore|fork|delete> <slot> — act on a stored archive record.
#
# WHY slot numbers: host plugin actions are param-less and the guest wasm has no
# filesystem, so the widget can't hand the host an arbitrary record id. Instead
# the widget renders the 10 newest records as chips in a fixed slot order
# (slot 1 = newest) and a click invokes the matching `<verb>-slot-N` action,
# which runs THIS script with (verb, N). We re-derive the record by sorting
# ~/.herdr-archive/archive.json by ts desc and taking the Nth — the exact same
# ordering the widget input file uses, so slot N here == chip N there.
#
#   restore : resume the SAME session (no fork flag) in a new tab
#             claude/cx -> cx-resume <id> ; codex -> codex resume <id> ;
#             pi -> pi --session <value>
#   fork    : resume with each agent's fork flag (fresh session id) in a new tab
#             claude/cx -> cx-resume <id> --fork-session ; codex -> codex fork <id> ;
#             pi -> pi --fork <value>
#   delete  : drop the record from the store (no tab), regenerate the input file
#
# Restore/fork DO NOT remove the record (you can restore the same session again);
# delete is the only mutation. Every path regenerates the widget input file.
set -u

VERB="${1:-}"
SLOT="${2:-}"
case "$VERB" in restore | fork | delete) ;; *) echo "usage: $0 <restore|fork|delete> <slot>" >&2; exit 2 ;; esac
[[ "$SLOT" =~ ^[0-9]+$ ]] || { echo "slot must be a number" >&2; exit 2; }

ARCHIVE_DIR="$HOME/.herdr-archive"
STORE="$ARCHIVE_DIR/archive.json"
LOG="$ARCHIVE_DIR/archive.log"
mkdir -p "$ARCHIVE_DIR" 2>/dev/null
log() { printf '%s [%s#%s] %s\n' "$(date '+%H:%M:%S')" "$VERB" "$SLOT" "$*" >>"$LOG" 2>/dev/null; }

# ── binary + jq ───────────────────────────────────────────────────────────────
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

[[ -f "$STORE" ]] || { log "FATAL: no store at $STORE"; exit 4; }

STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-}"
regen_input() {
  [[ -z "$STATE_DIR" ]] && { log "WARN: no HERDR_PLUGIN_STATE_DIR; skip input regen"; return; }
  mkdir -p "$STATE_DIR" 2>/dev/null
  local out="$STATE_DIR/archive_input.json" tmp="$STATE_DIR/.archive_input.json.$$.tmp"
  jq -c '
    (sort_by(.ts) | reverse)[0:10]
    | { items: [ range(0; length) as $i | {
        slot: ($i + 1),
        label: (.[$i].label // "session"),
        agent: (.[$i].agent // ""),
        resume: (.[$i].resume // "")
      } ] }
  ' "$STORE" >"$tmp" 2>>"$LOG" && mv -f "$tmp" "$out" \
    && log "input regen ($(jq '.items|length' "$out" 2>/dev/null) items)" || log "WARN: input regen failed"
}

# ── select the slot's record (newest first, 1-based) ──────────────────────────
IDX=$((SLOT - 1))
REC="$(jq -c --argjson i "$IDX" '(sort_by(.ts) | reverse) | .[$i] // empty' "$STORE" 2>>"$LOG")"
[[ -z "$REC" || "$REC" == "null" ]] && { log "FATAL: no record at slot $SLOT"; exit 5; }

AGENT="$(printf '%s' "$REC" | jq -r '.agent // empty')"
VALUE="$(printf '%s' "$REC" | jq -r '.session_value // empty')"
CWD="$(printf '%s' "$REC" | jq -r '.cwd // empty')"
WS="$(printf '%s' "$REC" | jq -r '.workspace_id // empty')"
LABEL="$(printf '%s' "$REC" | jq -r '.label // "session"')"
TS="$(printf '%s' "$REC" | jq -r '.ts')"
[[ -z "$CWD" ]] && CWD="$HOME"
[[ -z "$WS" ]] && WS="${HERDR_WORKSPACE_ID:-}"
log "record: agent=$AGENT value=$VALUE cwd=$CWD ws=$WS label=$LABEL ts=$TS"

# ── delete ────────────────────────────────────────────────────────────────────
if [[ "$VERB" == "delete" ]]; then
  TMP="$ARCHIVE_DIR/.archive.json.$$.tmp"
  jq --argjson ts "$TS" 'map(select(.ts != $ts))' "$STORE" >"$TMP" 2>>"$LOG" \
    && mv -f "$TMP" "$STORE" || { log "FATAL: delete failed"; exit 6; }
  log "deleted record ts=$TS"
  regen_input
  exit 0
fi

# ── restore / fork: build the per-agent command ───────────────────────────────
[[ -z "$AGENT" || -z "$VALUE" ]] && { log "FATAL: record missing agent/session"; exit 7; }
CMD=""
case "$VERB:$AGENT" in
  restore:claude) CMD="cx-resume $(printf '%q' "$VALUE")" ;;
  restore:codex)  CMD="codex resume $(printf '%q' "$VALUE")" ;;
  restore:pi | restore:omp) CMD="pi --session $(printf '%q' "$VALUE")" ;;
  fork:claude)    CMD="cx-resume $(printf '%q' "$VALUE") --fork-session" ;;
  fork:codex)     CMD="codex fork $(printf '%q' "$VALUE")" ;;
  fork:pi | fork:omp) CMD="pi --fork $(printf '%q' "$VALUE")" ;;
  *) log "FATAL: $VERB not supported for agent '$AGENT'"; exit 8 ;;
esac
log "cmd: $CMD"

# Label the new tab after the archived session.
DEST_LABEL="$LABEL"
[[ "$VERB" == "fork" ]] && DEST_LABEL="${LABEL% fork} fork"

CREATE_JSON="$(hd tab create --workspace "$WS" --cwd "$CWD" --label "$DEST_LABEL" --focus 2>>"$LOG")"
ROOT_PANE="$(printf '%s' "$CREATE_JSON" | jq -r '.result.root_pane.pane_id // empty')"
[[ -z "$ROOT_PANE" || "$ROOT_PANE" == "null" ]] && { log "FATAL: tab create failed: $CREATE_JSON"; exit 9; }
log "dest root pane: $ROOT_PANE (tab label=$DEST_LABEL)"

# Settle so the root pane's login shell is at its prompt before we type.
sleep 0.4
# `exec` replaces the shell so closing the agent closes the pane.
hd pane run "$ROOT_PANE" "exec $CMD" >>"$LOG" 2>&1
log "$VERB OK: $AGENT $VALUE -> $ROOT_PANE"
exit 0
