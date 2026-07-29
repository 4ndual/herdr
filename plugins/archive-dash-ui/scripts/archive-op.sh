#!/usr/bin/env bash
# archive-op.sh <restore|fork|delete> [slot] — act on a stored archive record.
#
# WHERE THE SLOT COMES FROM: the widget emits `InvokePluginAction` with
# `payload_json = {"slot":N}`, and the host exports that string to us as
# HERDR_PLUGIN_PAYLOAD_JSON. The trailing positional arg is still accepted so the
# script stays runnable by hand (and for the ten `<verb>-slot-N` actions this
# replaced, which encoded the integer in the action id back when host actions
# were param-less).
#
# Either way the slot is 1-based, newest first. We re-derive the record by
# sorting ~/.herdr-archive/archive.json by ts desc and taking the Nth — the exact
# same ordering the widget input file uses, so slot N here == chip N there.
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

ARCHIVE_DIR="$HOME/.herdr-archive"
STORE="$ARCHIVE_DIR/archive.json"
LOG="$ARCHIVE_DIR/archive.log"
mkdir -p "$ARCHIVE_DIR" 2>/dev/null
log() { printf '%s [%s#%s] %s\n' "$(date '+%H:%M:%S')" "$VERB" "${SLOT:-?}" "$*" >>"$LOG" 2>/dev/null; }

# ── binary + jq ───────────────────────────────────────────────────────────────
# PATH LAST, deliberately: `command -v herdr` resolves to ~/.local/bin/herdr, a
# symlink to a June-19 pre-prod build missing this fork's subcommands. This loop
# used to try PATH third and could silently run the wrong binary — the same root
# cause that left the resume-search button broken for weeks. Order matches the
# other plugins' scripts verbatim.
HERDR=""
for cand in "${HERDR_BIN:-}" "${HERDR_BIN_PATH:-}" \
  "$HOME/.herdr-hr/bin/herdr" \
  "$HOME/Projects/herdr-worktrees/imgapi-127/target/release/herdr" \
  "$HOME/Projects/herdr/target/release/herdr" \
  "$(command -v herdr 2>/dev/null)"; do
  if [[ -n "$cand" && -x "$cand" ]]; then HERDR="$cand"; break; fi
done
hd() { "$HERDR" "$@"; }

# Surface every failure to the user. The widget fires and forgets, so a silent
# exit here is indistinguishable from a dead button.
toast() {
  local title="$1" body="${2:-}"
  log "TOAST: $title — $body"
  [[ -n "$HERDR" ]] || return 0
  hd notification show "$title" ${body:+--body "$body"} >>"$LOG" 2>&1 || true
}
die() { toast "Archive: $VERB failed" "$1"; exit "$2"; }

[[ -z "$HERDR" ]] && { log "FATAL: no herdr binary"; exit 3; }

if ! command -v jq >/dev/null 2>&1; then
  for j in /opt/homebrew/bin/jq /usr/local/bin/jq /usr/bin/jq; do
    [[ -x "$j" ]] && { export PATH="$(dirname "$j"):$PATH"; break; }
  done
fi
command -v jq >/dev/null 2>&1 || die "jq not found" 3

case "$VERB" in restore | fork | delete) ;; *) die "unknown verb '$VERB'" 2 ;; esac

# Slot: positional arg wins (hand-run), else the payload the widget sent.
if [[ -z "$SLOT" && -n "${HERDR_PLUGIN_PAYLOAD_JSON:-}" ]]; then
  SLOT="$(printf '%s' "$HERDR_PLUGIN_PAYLOAD_JSON" | jq -r '.slot // empty' 2>/dev/null)"
fi
if [[ -z "$SLOT" ]]; then
  die "no slot given (empty HERDR_PLUGIN_PAYLOAD_JSON — is the plugin_actions capability declared?)" 2
fi
[[ "$SLOT" =~ ^[0-9]+$ ]] || die "slot '$SLOT' is not a number" 2

[[ -f "$STORE" ]] || die "no archive store at $STORE" 4

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
[[ -z "$REC" || "$REC" == "null" ]] && die "no archived record at slot $SLOT" 5

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
    && mv -f "$TMP" "$STORE" || die "could not rewrite the archive store" 6
  log "deleted record ts=$TS"
  regen_input
  exit 0
fi

# ── restore / fork: build the per-agent command ───────────────────────────────
[[ -z "$AGENT" || -z "$VALUE" ]] && die "archived record has no agent/session to resume" 7
CMD=""
case "$VERB:$AGENT" in
  restore:claude) CMD="cx-resume $(printf '%q' "$VALUE")" ;;
  restore:codex)  CMD="codex resume $(printf '%q' "$VALUE")" ;;
  restore:pi | restore:omp) CMD="pi --session $(printf '%q' "$VALUE")" ;;
  fork:claude)    CMD="cx-resume $(printf '%q' "$VALUE") --fork-session" ;;
  fork:codex)     CMD="codex fork $(printf '%q' "$VALUE")" ;;
  fork:pi | fork:omp) CMD="pi --fork $(printf '%q' "$VALUE")" ;;
  *) die "$VERB is not supported for agent '$AGENT'" 8 ;;
esac
log "cmd: $CMD"

# Label the new tab after the archived session.
DEST_LABEL="$LABEL"
[[ "$VERB" == "fork" ]] && DEST_LABEL="${LABEL% fork} fork"

CREATE_JSON="$(hd tab create --workspace "$WS" --cwd "$CWD" --label "$DEST_LABEL" --focus 2>>"$LOG")"
ROOT_PANE="$(printf '%s' "$CREATE_JSON" | jq -r '.result.root_pane.pane_id // empty')"
[[ -z "$ROOT_PANE" || "$ROOT_PANE" == "null" ]] && { log "tab create response: $CREATE_JSON"; die "could not create the tab" 9; }
log "dest root pane: $ROOT_PANE (tab label=$DEST_LABEL)"

# Settle so the root pane's login shell is at its prompt before we type.
sleep 0.4
# `exec` replaces the shell so closing the agent closes the pane.
hd pane run "$ROOT_PANE" "exec $CMD" >>"$LOG" 2>&1 \
  || die "created the tab but could not start '$AGENT' in it" 10
log "$VERB OK: $AGENT $VALUE -> $ROOT_PANE"
exit 0
