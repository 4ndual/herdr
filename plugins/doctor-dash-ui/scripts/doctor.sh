#!/usr/bin/env bash
# doctor-dash runner — the only part of this plugin that executes anything.
#
# The wasm guest has no filesystem and no process spawn, so everything the panel
# knows arrives through one file:
#
#   fixers manifest  ──read──▶  doctor.sh  ──write──▶  $STATE_DIR/doctor_input.json
#                                                             │
#                                     host injects it as HERDR_PLUGIN_UI_INPUT_JSON
#                                                             ▼
#                                                        the guest renders
#
# Invoked ONLY through the plugin actions declared in herdr-plugin.toml, which
# the host runs on a detached thread — nothing here is on the render loop.
#
#   doctor.sh check-all     run every fixer's check + detail, rewrite the snapshot
#   doctor.sh fix           repair ONE fixer; its id arrives as
#                           HERDR_PLUGIN_PAYLOAD_JSON='{"id":"cx"}'
#
# Manifest contract — a BARE JSON ARRAY of entries:
#   [ { "id": "cx", "label": "cx / CCR",
#       "check":    "<cmd, exit 0 healthy, 1 broken, 2 unknown>",
#       "fix":      "<cmd, deterministic repair>",
#       "escalate": "<cmd handing off to an agent via cx>",
#       "detail":   "<cmd emitting one human-readable line>" }, … ]
#
# A `{"fixers":[…]}` wrapper is also accepted, and any additional key (`state`,
# and whatever later healers add) is ignored rather than fatal — entries are read
# field by field, never validated against a closed schema, because healers get
# appended by other people without touching this script.
#
# A NON-ZERO `fix` exit means "not fully resolved", NOT "nothing happened". Some
# fixers deliberately repair what they safely can, report the remainder and exit
# non-zero to hand the rest over. That is why the failure path below keeps the
# fix's own output as the row detail and reports "handed to an agent" — an
# escalation is a different outcome from a broken fix, and the panel says so.
#
# Every failure mode here ends in a snapshot the panel can render. The script
# never exits without writing one, so the UI never goes blank on an error.

set -uo pipefail

# ── the ONE manifest path constant ───────────────────────────────────────────
# Env-overridable so sandboxes and tests can point at a fixtures file without a
# second literal existing anywhere in the codebase.
FIXERS_MANIFEST="${HERDR_DOCTOR_MANIFEST:-$HOME/.config/herdr/fixers.json}"

# Where the guest reads from. Must match `ui_input_file` in herdr-plugin.toml.
SNAPSHOT_NAME="doctor_input.json"
STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-$HOME/.local/state/herdr/plugins/builtin.doctor-dash}"
SNAPSHOT="$STATE_DIR/$SNAPSHOT_NAME"
LOCK_DIR="$STATE_DIR/.doctor.lock"

# Per-command wall-clock budget. A check that hangs must not wedge the runner.
CHECK_TIMEOUT="${HERDR_DOCTOR_CHECK_TIMEOUT:-10}"
FIX_TIMEOUT="${HERDR_DOCTOR_FIX_TIMEOUT:-300}"

mkdir -p "$STATE_DIR" 2>/dev/null

log() { printf 'doctor: %s\n' "$*" >&2; }

now_ms() {
  # Portable enough: date +%s%3N is GNU-only, so build it from seconds.
  printf '%s000' "$(date +%s)"
}

# Atomic same-directory write, so the host's mtime-cached reader never sees a
# half-written snapshot.
write_snapshot() {
  local payload="$1" tmp
  tmp="$(mktemp "$STATE_DIR/.doctor_input.XXXXXX")" || return 1
  printf '%s\n' "$payload" >"$tmp" && mv -f "$tmp" "$SNAPSHOT"
}

# Emit a snapshot that carries only an error. Used whenever the manifest cannot
# be turned into rows at all — this is what makes the panel degrade to an
# informative empty state instead of crashing or going blank.
fail_snapshot() {
  local message="$1"
  log "$message"
  write_snapshot "$(jq -nc \
    --arg path "$FIXERS_MANIFEST" \
    --arg err "$message" \
    --argjson gen "$(now_ms)" \
    '{manifest_path:$path, error:$err, generation:$gen, rows:[]}' 2>/dev/null ||
    printf '{"manifest_path":"%s","error":"%s","generation":0,"rows":[]}' \
      "$FIXERS_MANIFEST" "runner failed and jq is unavailable")"
}

# Run `$1` under a wall-clock budget of `$2` seconds. Returns the command's exit
# status, or 124 on timeout (the conventional timeout(1) code). Implemented with
# a watchdog because macOS ships no timeout(1).
run_bounded() {
  local cmd="$1" budget="$2" pid watchdog status
  bash -c "$cmd" >/dev/null 2>&1 &
  pid=$!
  (
    sleep "$budget"
    kill -TERM "$pid" 2>/dev/null
    sleep 2
    kill -KILL "$pid" 2>/dev/null
  ) &
  watchdog=$!
  wait "$pid"
  status=$?
  kill "$watchdog" 2>/dev/null
  wait "$watchdog" 2>/dev/null
  return $status
}

# Run a repair under a budget, keeping its LAST output line in FIX_REPORT. A
# fixer that half-succeeds says so on stdout, and that sentence is worth more to
# the user than any exit code this script could paraphrase.
FIX_REPORT=""
run_fix_reporting() {
  local cmd="$1" budget="$2" pid watchdog status tmp
  tmp="$(mktemp "$STATE_DIR/.doctor_fix.XXXXXX")" || return 1
  bash -c "$cmd" >"$tmp" 2>&1 &
  pid=$!
  (
    sleep "$budget"
    kill -TERM "$pid" 2>/dev/null
    sleep 2
    kill -KILL "$pid" 2>/dev/null
  ) &
  watchdog=$!
  wait "$pid"
  status=$?
  kill "$watchdog" 2>/dev/null
  wait "$watchdog" 2>/dev/null
  FIX_REPORT="$(tail -n 1 "$tmp" 2>/dev/null | tr -d '\r')"
  rm -f "$tmp"
  return $status
}

# Same, but capture the first line of stdout (for `detail`).
run_bounded_capture() {
  local cmd="$1" budget="$2" out
  out="$(bash -c "
    bash -c $(printf '%q' "$cmd") 2>/dev/null &
    child=\$!
    ( sleep $budget; kill -KILL \$child 2>/dev/null ) &
    watchdog=\$!
    wait \$child
    kill \$watchdog 2>/dev/null
  " 2>/dev/null | head -n 1)"
  printf '%s' "$out"
}

# Map a check's exit status onto the panel's vocabulary.
state_for_status() {
  case "$1" in
    0) printf 'healthy' ;;
    1) printf 'broken' ;;
    2) printf 'unknown' ;;
    124) printf 'unknown' ;;
    *) printf 'unknown' ;;
  esac
}

# Guard the manifest before anything reads it. Echoes nothing on success; on
# failure it has already written the error snapshot and returns non-zero.
load_manifest() {
  if ! command -v jq >/dev/null 2>&1; then
    fail_snapshot "jq is required to read the fixers manifest but is not on PATH"
    return 1
  fi
  if [ ! -e "$FIXERS_MANIFEST" ]; then
    fail_snapshot "fixers manifest not found at $FIXERS_MANIFEST"
    return 1
  fi
  if [ ! -r "$FIXERS_MANIFEST" ]; then
    fail_snapshot "fixers manifest is not readable: $FIXERS_MANIFEST"
    return 1
  fi
  local parse_error
  if ! parse_error="$(jq -e 'type=="array" or (type=="object" and (.fixers|type)=="array")' \
    "$FIXERS_MANIFEST" 2>&1 >/dev/null)"; then
    if [ -n "$parse_error" ]; then
      fail_snapshot "fixers manifest is malformed: $(printf '%s' "$parse_error" | head -n 1)"
    else
      fail_snapshot "fixers manifest must be a json array of fixers (or {\"fixers\":[…]})"
    fi
    return 1
  fi
  return 0
}

# Normalised array of entries, whichever of the two accepted shapes is on disk.
manifest_entries() {
  jq -c 'if type=="array" then . else .fixers end
         | map(select(type=="object" and (.id|type)=="string" and (.id|length)>0))' \
    "$FIXERS_MANIFEST" 2>/dev/null
}

field() {
  # field <entry-json> <name> -> value or empty
  printf '%s' "$1" | jq -r --arg f "$2" '.[$f] // ""' 2>/dev/null
}

# ── check-all ────────────────────────────────────────────────────────────────

cmd_check_all() {
  load_manifest || return 0

  local entries count
  entries="$(manifest_entries)"
  if [ -z "$entries" ]; then
    fail_snapshot "fixers manifest could not be read as a list of fixers"
    return 0
  fi
  count="$(printf '%s' "$entries" | jq 'length' 2>/dev/null || printf '0')"

  local rows='[]' i entry id label check detail state status detail_text
  i=0
  while [ "$i" -lt "$count" ]; do
    entry="$(printf '%s' "$entries" | jq -c ".[$i]")"
    id="$(field "$entry" id)"
    label="$(field "$entry" label)"
    [ -n "$label" ] || label="$id"
    check="$(field "$entry" check)"
    detail="$(field "$entry" detail)"

    if [ -z "$check" ]; then
      state="unknown"
    else
      run_bounded "$check" "$CHECK_TIMEOUT"
      status=$?
      state="$(state_for_status "$status")"
    fi

    detail_text=""
    if [ -n "$detail" ]; then
      detail_text="$(run_bounded_capture "$detail" "$CHECK_TIMEOUT")"
    fi
    [ -n "$detail_text" ] || detail_text="no detail command"

    rows="$(printf '%s' "$rows" | jq -c \
      --arg id "$id" --arg label "$label" \
      --arg state "$state" --arg detail "$detail_text" \
      '. + [{id:$id, label:$label, state:$state, detail:$detail}]')"
    i=$((i + 1))
  done

  write_snapshot "$(jq -nc \
    --arg path "$FIXERS_MANIFEST" \
    --argjson gen "$(now_ms)" \
    --argjson rows "$rows" \
    '{manifest_path:$path, error:null, generation:$gen, rows:$rows}')"
}

# ── fix ──────────────────────────────────────────────────────────────────────

# Rewrite ONE row of the existing snapshot in place, leaving its peers alone, and
# bump the generation so the guest retires its optimistic "working" flag.
patch_row() {
  local id="$1" state="$2" detail="$3" note="$4" current
  current="$(cat "$SNAPSHOT" 2>/dev/null)"
  [ -n "$current" ] || current='{"rows":[]}'
  local patched
  patched="$(printf '%s' "$current" | jq -c \
    --arg id "$id" --arg state "$state" --arg detail "$detail" --arg note "$note" \
    --argjson gen "$(now_ms)" \
    '.generation = $gen
     | .rows = ((.rows // []) | map(
         if .id == $id then
           .state = $state
           | (if $detail != "" then .detail = $detail else . end)
           | .note = (if $note == "" then null else $note end)
         else . end))' 2>/dev/null)"
  [ -n "$patched" ] && write_snapshot "$patched"
}

cmd_fix() {
  local payload id
  payload="${HERDR_PLUGIN_PAYLOAD_JSON:-}"
  id="$(printf '%s' "$payload" | jq -r '.id // ""' 2>/dev/null)"
  if [ -z "$id" ]; then
    log "fix invoked without an id in HERDR_PLUGIN_PAYLOAD_JSON"
    return 0
  fi

  # One repair at a time. A second click while a fix runs is a no-op rather than
  # two concurrent repairs of the same thing.
  if ! mkdir "$LOCK_DIR" 2>/dev/null; then
    log "a fix is already running; ignoring $id"
    return 0
  fi
  trap 'rmdir "$LOCK_DIR" 2>/dev/null' EXIT

  load_manifest || return 0
  local entries entry fix_cmd escalate_cmd check_cmd detail_cmd detail_text status
  entries="$(manifest_entries)"
  entry="$(printf '%s' "$entries" | jq -c --arg id "$id" '.[] | select(.id == $id)' | head -n 1)"
  if [ -z "$entry" ]; then
    log "no fixer with id '$id' in $FIXERS_MANIFEST"
    return 0
  fi

  fix_cmd="$(field "$entry" fix)"
  escalate_cmd="$(field "$entry" escalate)"
  check_cmd="$(field "$entry" check)"
  detail_cmd="$(field "$entry" detail)"

  patch_row "$id" working "" "repairing…"

  if [ -z "$fix_cmd" ]; then
    patch_row "$id" unknown "this fixer declares no fix command" ""
    return 0
  fi

  run_fix_reporting "$fix_cmd" "$FIX_TIMEOUT"
  status=$?

  if [ "$status" -ne 0 ]; then
    # NOT FULLY RESOLVED. This is the normal, designed outcome for anything a
    # button cannot safely finish: the fixer repairs what it can, prints what it
    # left, and exits non-zero so an agent picks up the rest. Keep the fixer's own
    # sentence as the detail, and distinguish the three endings clearly —
    # "handed to an agent" is not the same news as "the fix broke".
    local report="$FIX_REPORT"
    [ -n "$report" ] || report="not fully resolved (exit $status)"
    if [ -n "$escalate_cmd" ]; then
      run_bounded "$escalate_cmd" "$CHECK_TIMEOUT"
      local escalate_status=$?
      if [ "$escalate_status" -eq 0 ]; then
        # Row stays `working` on purpose: an agent IS working on it. The user
        # watches the panel, not a terminal.
        patch_row "$id" working "$report" "handed to an agent"
      else
        patch_row "$id" broken "$report" \
          "escalation failed too (exit $escalate_status) — needs a human"
      fi
    else
      patch_row "$id" broken "$report" "no escalate command declared"
    fi
    return 0
  fi

  # The fix claims success — re-check rather than take its word for it.
  local state="unknown"
  if [ -n "$check_cmd" ]; then
    run_bounded "$check_cmd" "$CHECK_TIMEOUT"
    state="$(state_for_status $?)"
  fi
  detail_text=""
  if [ -n "$detail_cmd" ]; then
    detail_text="$(run_bounded_capture "$detail_cmd" "$CHECK_TIMEOUT")"
  fi
  patch_row "$id" "$state" "$detail_text" ""
}

case "${1:-}" in
  check-all) cmd_check_all ;;
  fix) cmd_fix ;;
  *)
    printf 'usage: doctor.sh {check-all|fix}\n' >&2
    exit 2
    ;;
esac
