#!/usr/bin/env bash
# test-fast.sh — FAST regression suite for the herdr fork + its plugins.
#
# Encodes REAL failure cases (see the inventory in
# .local/prd/hr-instances-methodology.md) as cross-cutting contract checks that
# run on every change. Hard budgets:
#   --scope host            ≤5 min (host binary changed)
#   --scope plugin:<name>   ≤1 min (only that plugin changed)
#   --scope all             everything
#
# Usage:
#   scripts/test-fast/test-fast.sh --scope host
#   scripts/test-fast/test-fast.sh --scope plugin:version-dash-ui
#   scripts/test-fast/test-fast.sh --scope all
#   scripts/test-fast/test-fast.sh --update-golden   # reseed placement golden
#
# NEVER uses the default cargo target dir in the repo (an in-place relink would
# SIGKILL the live hr server). All cargo work goes to CARGO_TARGET_DIR below.
set -uo pipefail

# ── env safety preamble: never let an inherited hr/test env leak in ───────────
unset HERDR_ENV HERDR_SOCKET_PATH HERDR_CLIENT_SOCKET_PATH HERDR_SESSION \
      HERDR_WORKSPACE_ID HERDR_TAB_ID HERDR_PANE_ID 2>/dev/null || true

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PLUGINS="$REPO/plugins"
HERE="$REPO/scripts/test-fast"
CHECKS="$HERE/checks.py"
GOLDEN="$HERE/placement.golden.json"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/Projects/herdr-target-testsuite}"

SCOPE="all"
PLUGIN=""
for arg in "$@"; do
  case "$arg" in
    --scope) : ;;
    --scope=*) SCOPE="${arg#--scope=}" ;;
    --update-golden)
      python3 "$CHECKS" placement-write "$PLUGINS" "$GOLDEN"; exit $? ;;
    host|all) SCOPE="$arg" ;;
    plugin:*) SCOPE="plugin"; PLUGIN="${arg#plugin:}" ;;
    *) echo "unknown arg: $arg" >&2; exit 2 ;;
  esac
done
# support `--scope host` (two tokens)
[ "$SCOPE" = "all" ] && for i in "$@"; do case "$i" in host) SCOPE=host;; plugin:*) SCOPE=plugin; PLUGIN="${i#plugin:}";; esac; done

[ "$SCOPE" = "plugin" ] && [ -z "$PLUGIN" ] && { echo "plugin scope needs plugin:<name>" >&2; exit 2; }
[ "$SCOPE" = "plugin" ] && export HERDR_TESTFAST_PLUGIN="$PLUGIN"

PASS=0; FAIL=0; SKIP=0
declare -a SUMMARY

# run <name> <cmd...> — time + record a check
run() {
  local name="$1"; shift
  local start end dur
  start=$(python3 -c 'import time;print(time.time())')
  if "$@" >/tmp/testfast.$$.out 2>&1; then
    end=$(python3 -c 'import time;print(time.time())')
    dur=$(python3 -c "print(f'{$end-$start:5.1f}s')")
    tail -1 /tmp/testfast.$$.out | sed 's/^/    /'
    echo "  PASS $name ($dur)"
    SUMMARY+=("PASS  $dur  $name"); PASS=$((PASS+1))
  else
    end=$(python3 -c 'import time;print(time.time())')
    dur=$(python3 -c "print(f'{$end-$start:5.1f}s')")
    sed 's/^/    /' /tmp/testfast.$$.out
    echo "  FAIL $name ($dur)"
    SUMMARY+=("FAIL  $dur  $name"); FAIL=$((FAIL+1))
  fi
  rm -f /tmp/testfast.$$.out
}
skip() { echo "  SKIP $1 ($2)"; SUMMARY+=("SKIP    -    $1 ($2)"); SKIP=$((SKIP+1)); }

# ── T1/T2/T3: static contract checks (run in every scope) ─────────────────────
run "T1 manifest-parse"      python3 "$CHECKS" manifests "$PLUGINS"
run "T2 placement-contract"  python3 "$CHECKS" placement "$PLUGINS" "$GOLDEN"
run "T3 wasm-load"           python3 "$CHECKS" wasm "$PLUGINS"

# collector + script contracts are per-plugin; gate them by scope/plugin.
want_plugin() { [ "$SCOPE" != "plugin" ] || [ "$PLUGIN" = "$1" ]; }

# ── T4: version-dash collector output schema ─────────────────────────────────
check_version_collector() {
  local bin sd out
  bin="$REPO/target-versiondash/release/version-dash-collector"
  [ -x "$bin" ] || bin="$REPO/target/release/version-dash-collector"
  [ -x "$bin" ] || bin="$CARGO_TARGET_DIR/release/version-dash-collector"
  if [ ! -x "$bin" ]; then echo "collector binary not built"; return 1; fi
  sd="$(mktemp -d)"; out="$sd/version_input.json"
  # network-independent: even a failed fetch carries forward + writes the file.
  HERDR_VERSION_DASH_OUTPUT="$out" "$bin" >/dev/null 2>&1 || true
  [ -f "$out" ] || { echo "collector wrote no output"; rm -rf "$sd"; return 1; }
  python3 "$CHECKS" version-input "$out"; local rc=$?; rm -rf "$sd"; return $rc
}
if want_plugin version-dash-ui; then run "T4 version-collector-schema" check_version_collector; fi

# ── T5: usage-dash collector protocol line ───────────────────────────────────
check_usage_collector() {
  local bin td tx focus line
  bin="$REPO/target/release/usage-dash-collector"
  [ -x "$bin" ] || bin="$CARGO_TARGET_DIR/release/usage-dash-collector"
  if [ ! -x "$bin" ]; then echo "collector binary not built"; return 1; fi
  td="$(mktemp -d)"; tx="$td/session.jsonl"
  # A minimal-but-real claude transcript record (schema-shaped, generated here).
  printf '%s\n' \
    '{"type":"user","cwd":"'"$td"'","message":{"role":"user","content":"hi"}}' \
    '{"type":"assistant","message":{"role":"assistant","usage":{"input_tokens":10,"output_tokens":5},"content":[{"type":"text","text":"ok"}]}}' \
    > "$tx"
  # AgentKind serializes PascalCase (no serde rename) → "Claude", not "claude".
  focus=$(python3 -c "import json;print(json.dumps({'session_id':'session','session_ref':'$tx','cwd':'$td','agent':'Claude','is_self_focus':False,'subagent':None}))")
  line=$(HERDR_FOCUS_JSON="$focus" "$bin" 2>/dev/null); local rc=$?
  rm -rf "$td"
  [ $rc -eq 0 ] || { echo "collector exited $rc"; return 1; }
  python3 "$CHECKS" usage-line "$line"
}
if want_plugin usage-dash-ui; then run "T5 usage-collector-schema" check_usage_collector; fi

# ── T6: cx-resume any-cwd fallback (button-dash resume dispatch) ──────────────
check_cx_resume() {
  bash "$HERE/t6_cx_resume.sh"
}
if want_plugin button-dash-ui; then run "T6 cx-resume-anycwd" check_cx_resume; fi

# ── T6b: cx-resume never-prompted session → fresh start (not a dead shell) ────
check_cx_resume_never_prompted() {
  bash "$HERE/t6b_cx_resume_never_prompted.sh"
}
if want_plugin button-dash-ui; then run "T6b cx-resume-never-prompted" check_cx_resume_never_prompted; fi

# ── T7: update-herdrtest.sh pinned-env relaunch contract ─────────────────────
check_update_env() {
  local f="$PLUGINS/version-dash-ui/scripts/update-herdrtest.sh"
  [ -f "$f" ] || { echo "update-herdrtest.sh missing"; return 1; }
  grep -q 'XDG_CONFIG_HOME=' "$f" && grep -q 'XDG_STATE_HOME=' "$f" \
    && grep -q -- '-u HERDR_SOCKET_PATH' "$f" && grep -q -- '-u HERDR_SESSION' "$f" \
    || { echo "relaunch does not pin XDG + unset HERDR_* nesting vars"; return 1; }
  echo "ok: herdrtest relaunch pins XDG root + unsets HERDR_* nesting"
}
if want_plugin version-dash-ui; then run "T7 update-herdrtest-pinned-env" check_update_env; fi

# ── T8: restorable-agents (host-only; existing rust tests, isolated target) ──
check_restorable() {
  ( cd "$REPO" && cargo test --release \
      --bin herdr -p herdr_bin 2>/dev/null \
      restorable 2>&1 ) || \
  ( cd "$REPO" && bash "$HERE/t8_restorable.sh" )
}
if [ "$SCOPE" = "host" ] || [ "$SCOPE" = "all" ]; then
  run "T8 restorable-agents" bash "$HERE/t8_restorable.sh"
else
  skip "T8 restorable-agents" "host-only"
fi

# ── T9: sandbox boot smoke (host-only; guards relaunch env bug at runtime) ───
if [ "$SCOPE" = "host" ] || [ "$SCOPE" = "all" ]; then
  run "T9 sandbox-boot-smoke" bash "$HERE/t9_boot_smoke.sh"
else
  skip "T9 sandbox-boot-smoke" "host-only"
fi

# ── T10: known-failing quarantine (host-only; asserts 3 upstream tests stay red)
if [ "$SCOPE" = "host" ] || [ "$SCOPE" = "all" ]; then
  run "T10 known-failing-quarantine" bash "$HERE/t10_known_failing.sh"
else
  skip "T10 known-failing-quarantine" "host-only"
fi

# ── summary ──────────────────────────────────────────────────────────────────
echo
echo "── test-fast summary (scope=$SCOPE${PLUGIN:+:$PLUGIN}) ─────────────────────"
for line in "${SUMMARY[@]}"; do echo "  $line"; done
echo "  ── $PASS passed, $FAIL failed, $SKIP skipped ──"
[ "$FAIL" -eq 0 ]
