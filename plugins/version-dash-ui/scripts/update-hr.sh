#!/bin/bash
# update-hr.sh — button 2: promote the VERIFIED herdrtest build into hr.
#
# ── WHAT CHANGED AND WHY ──────────────────────────────────────────────────
# The previous version of this script gated on exactly one thing: "does the
# source binary answer `--version`". An upstream stable binary answers that
# perfectly. At the time of writing, `~/.local/bin/herdrtest` WAS a bridge-less
# upstream 0.7.5 — so right-clicking `hr` would have promoted it into the live
# instance and destroyed the entire WASM plugin bridge. That is the exact failure
# `.local/prd/fork-update-safety-audit.md` was written to prevent, arriving
# through the update button instead of through `herdr update`.
#
# This script now refuses to promote anything that has not proven itself, on
# THREE independent grounds, any one of which would have stopped that binary:
#
#   1. BRIDGE GATE      `herdr-bridge-check.sh` must accept the source binary —
#                       fork build, wasm ABI symbols, ui_modules schema, wasmtime.
#   2. VERIFIED RECEIPT the sha256-bound receipt from `verify-instance.sh` must
#                       exist, say ok=true, and match the source binary BYTE FOR
#                       BYTE. "herdrtest looks fine" is not sufficient; the gate
#                       must have actually run against this exact build, in the
#                       running process.
#   3. REGISTRY GUARD   hr's plugins.json is snapshotted before the swap, because
#                       the Finding-3 damage survives a binary rollback and a
#                       snapshot is the only recovery.
#
# ── THIS SCRIPT CANNOT RESTART hr ─────────────────────────────────────────
# There is no restart branch. Not disabled, not behind a flag, not behind an env
# var — ABSENT, so no future edit can re-enable it by flipping a default. `hr` is
# the terminal multiplexer hosting the user's session and every agent in it, so
# restarting it kills the process tree that would report what happened: there is
# no recovering AND reporting. The script swaps the binary atomically, verifies
# what can be verified without restarting, then puts the strip into
# `awaiting_restart` and leaves the keystroke to the user.
#
# Atomic swap only: `cp` over a running binary SIGKILLs it on macOS, so every
# install stages on the SAME volume as the destination and then `mv`s.
#
# EXIT CODES: 0 promoted (awaiting the user's restart) · 50 refused — source not
#             verified/bridged · 51 no target · 52 swap failed
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=vd-common.sh
source "$SCRIPT_DIR/vd-common.sh"

BRIDGE_CHECK="$SCRIPT_DIR/herdr-bridge-check.sh"
REGISTRY_GUARD="$SCRIPT_DIR/herdr-registry-guard.sh"

VD_TEST_BIN="${VD_TEST_BIN:-$HOME/.local/bin/herdrtest}"
VD_HR_ROOT="${VD_HR_ROOT:-$HOME/.herdr-hr}"
VD_HR_SESSION="${VD_HR_SESSION:-hr}"
VD_HR_SOCK="${VD_HR_SOCK:-$VD_HR_ROOT/config/herdr/sessions/$VD_HR_SESSION/herdr.sock}"
SNAPSHOT_BIN="$VD_HR_ROOT/bin/herdr"
STAGE=hr

json_field() {
  VD_J="$1" VD_K="$2" python3 -c 'import json,os
try: print(json.loads(os.environ["VD_J"]).get(os.environ["VD_K"], ""))
except Exception: print("")' 2>/dev/null
}

vd_phase "$STAGE" running "checking the herdrtest build is safe to promote"

# ── gate 1: the source binary must exist and carry the bridge ──────────────
if [[ ! -x "$VD_TEST_BIN" ]]; then
  vd_phase "$STAGE" failed "herdrtest binary missing" 50 "not executable: $VD_TEST_BIN"
  exit 50
fi

BRIDGE_JSON="$("$BRIDGE_CHECK" "$VD_TEST_BIN" --json --quiet 2>/dev/null)"
BRIDGE_RC=$?
if [[ $BRIDGE_RC -ne 0 ]]; then
  REASON="$(json_field "$BRIDGE_JSON" reason)"
  vd_log "REFUSED: $VD_TEST_BIN did not pass the bridge gate (exit $BRIDGE_RC): $REASON"
  vd_phase "$STAGE" failed "refused to promote an unbridged binary into hr" "$BRIDGE_RC" \
    "${REASON:-the source binary is not a fork build with an intact plugin bridge}"
  exit 50
fi
SRC_SHA="$(json_field "$BRIDGE_JSON" sha256)"
SRC_VER="$(json_field "$BRIDGE_JSON" version)"
vd_log "bridge gate: accepted herdrtest $SRC_VER ($SRC_SHA)"

# ── gate 2: a matching, passing verification receipt must exist ────────────
# This is the same rule the guest uses to grey out the hr token, enforced again
# here and independently. Hiding the action in the UI is what makes the UI
# honest; THIS is what makes the promotion safe.
RECEIPT="$VD_STATE_DIR/verify-test.json"
VERDICT="$(VD_R="$RECEIPT" VD_S="$SRC_SHA" python3 - <<'PY' 2>/dev/null
import json, os
try:
    d = json.load(open(os.environ["VD_R"]))
except Exception:
    print("missing|no verification receipt for herdrtest — run the herdrtest update first")
    raise SystemExit
if not d.get("ok"):
    failed = "; ".join(c.get("name", "?") for c in d.get("checks", []) if not c.get("ok"))
    print(f"failed|herdrtest verification FAILED ({failed or 'unknown check'})")
elif d.get("sha256", "") != os.environ["VD_S"]:
    print("stale|the herdrtest binary changed after it was verified — re-run the herdrtest update")
else:
    print("ok|")
PY
)"
VSTATUS="${VERDICT%%|*}"; VREASON="${VERDICT#*|}"
if [[ "$VSTATUS" != "ok" ]]; then
  vd_log "REFUSED: $VREASON"
  vd_phase "$STAGE" failed "refused: herdrtest is not verified" 50 "$VREASON"
  exit 50
fi
vd_log "verification receipt: ok, bound to $SRC_SHA"

# ── snapshot hr's registry before touching anything ────────────────────────
"$REGISTRY_GUARD" snapshot "$VD_HR_ROOT/config" --label "pre-hr-promote" >/dev/null 2>&1 \
  && vd_log "hr registry snapshotted" \
  || vd_log "WARN: could not snapshot hr registry"

# ── resolve every path hr might exec on its next start ─────────────────────
# The running hr server does not necessarily exec the `~/.herdr-hr/bin` snapshot
# — an old launcher loop can keep it exec'ing `target/release/herdr` — so swap
# BOTH, or the restart would silently come up on the old build.
vd_phase "$STAGE" running "swapping hr's binary"
live_hr_exe() {
  local pid exe
  pid="$(lsof -t "$VD_HR_SOCK" 2>/dev/null | head -1)"
  [[ -n "$pid" ]] || return 0
  exe="$(ps -p "$pid" -o comm= 2>/dev/null | sed 's/^ *//;s/ *$//;s/ server$//')"
  [[ -n "$exe" && -f "$exe" && "$(basename "$exe")" == "herdr" ]] && printf '%s\n' "$exe"
}

TARGETS=()
if [[ -n "${VD_HR_TARGETS:-}" ]]; then
  read -r -a TARGETS <<<"$VD_HR_TARGETS"
else
  LIVE="$(live_hr_exe || true)"
  [[ -n "$LIVE" ]] && TARGETS+=("$LIVE")
  [[ -f "$SNAPSHOT_BIN" || -d "$(dirname "$SNAPSHOT_BIN")" ]] && TARGETS+=("$SNAPSHOT_BIN")
fi
if [[ ${#TARGETS[@]} -eq 0 ]]; then
  vd_phase "$STAGE" failed "no hr exec target resolved" 51 "live server exe and snapshot both absent"
  exit 51
fi

DEDUP=()
for t in "${TARGETS[@]}"; do
  skip=0
  for d in ${DEDUP+"${DEDUP[@]}"}; do [[ "$d" == "$t" ]] && skip=1 && break; done
  [[ $skip == 0 ]] && DEDUP+=("$t")
done

swap_into() {
  local dest="$1" dir stage_file got
  dir="$(dirname "$dest")"
  mkdir -p "$dir"
  # Stage on the SAME volume as the destination, then atomic mv. Copying
  # straight onto the live path rewrites the running inode and SIGKILLs hr; mv
  # swaps the directory entry and leaves the old inode alive for the running
  # process, which is what makes this safe to do to a live daily driver.
  stage_file="$dir/.herdr.new.$$"
  cp "$VD_TEST_BIN" "$stage_file" || return 1
  chmod +x "$stage_file"
  touch "$stage_file"
  mv -f "$stage_file" "$dest" || return 1
  # Prove the bytes that landed are the bytes we gated.
  got="$(shasum -a 256 "$dest" 2>/dev/null | awk '{print $1}')"
  if [[ "$got" != "$SRC_SHA" ]]; then
    vd_log "ERROR: $dest is $got, expected $SRC_SHA"
    return 1
  fi
  vd_log "promoted → $dest ($SRC_VER, $got)"
}

for dest in "${DEDUP[@]}"; do
  swap_into "$dest" || vd_fail "$STAGE" 52 "failed to swap hr binary at $dest"
done

# ── write hr's receipt for the NEW binary ──────────────────────────────────
# Deliberately a STATIC check (--no-relaunch): verifying hr properly would mean
# relaunching it, which this pipeline will not do. So hr stays OUT of `verified`
# until it is genuinely running the new build — the collector compares the
# receipt sha against hr's LIVE executable, so `verified` appears by itself after
# the user restarts, and cannot appear before.
# DELIBERATELY NOT verify-instance.sh. That script was called here with
# `--label hr --no-relaunch`, which read as safe and was not: its re-link step
# was gated on the relaunch flag rather than on the instance, so this line —
# on the happy path of the shipped button — performed the exact registry write
# that wiped four of the user's live plugins on 2026-07-27. The guard is fixed
# there, but this call has no business existing at all: hr's receipt needs only
# the bytes on disk, and the least dangerous way to touch a live instance is not
# to touch it. This block reads the binary and writes a JSON file. Nothing else.
VD_R_SHA="$SRC_SHA" VD_R_BIN="${DEDUP[0]}" VD_R_VER="$SRC_VER" VD_R_DIR="$VD_STATE_DIR" \
python3 - <<'PY' || vd_log "WARN: could not write hr receipt"
import json, os, time
d = os.environ["VD_R_DIR"]; os.makedirs(d, exist_ok=True)
out = os.path.join(d, "verify-hr.json")
tmp = out + ".tmp"
# ok=false ON PURPOSE. hr has NOT been verified: verifying it would mean
# restarting it, which this pipeline will never do. The collector compares this
# receipt's sha against hr's LIVE executable, so hr turns `verified` by itself
# once the user restarts, and cannot claim it before.
json.dump({
    "label": "hr", "ok": False, "binary": os.environ["VD_R_BIN"],
    "sha256": os.environ["VD_R_SHA"], "version": os.environ["VD_R_VER"],
    "written_unix": int(time.time()),
    "checks": [{"name": "awaiting_user_restart", "ok": False,
                "detail": "binary staged; hr must be restarted by the user (ctrl+opt+X)"}],
}, open(tmp, "w"))
os.replace(tmp, out)
PY

vd_phase "$STAGE" awaiting_consent "hr updated to $SRC_VER — press ⌃⌥X when you are ready"
vd_log "DONE. hr will run the new build after YOU restart it (⌃⌥X). Nothing was restarted."
exit 0
