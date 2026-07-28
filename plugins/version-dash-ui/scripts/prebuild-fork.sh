#!/bin/bash
# prebuild-fork.sh — build the fork tip AHEAD of the button press, so that
# clicking "test" is a promote-and-verify, not a compile.
#
# ── WHY THIS EXISTS (measured, not guessed) ────────────────────────────────
# A cold `cargo build --release` of the fork is 126 s on this machine. The
# acceptance criterion is "update herdrtest, wait like 1 or 2 mins, everything
# okay". A build INSIDE the click spends the entire budget before verification
# even starts, and after a conflicted upstream merge the build time is unbounded
# — the 0.7.5 merge produced 8 distinct compile breaks that git did not flag as
# conflicts (duplicate `AgentPromptParams`, duplicate `fn agent_prompt`, …).
# So the expensive, failure-prone half runs OUT OF BAND, and the click consumes
# its result. Click 1 becomes promote + relaunch + verify ≈ 20–30 s.
#
# ── THE BUILD IS THE GATE ──────────────────────────────────────────────────
# Nothing here scans markers or reads `git status` to decide whether the merge
# was clean. Those all passed on the broken 0.7.5 merge: it was rustfmt-clean,
# valid JSON, git-status-clean and marker-free, and still had 8 compile breaks.
# The ONLY accepted evidence that a merge is sound is a SUCCESSFUL RELEASE BUILD
# of the whole workspace. That is what this script runs and what it reports.
#
# ── THE RE-LINK LIVES HERE ─────────────────────────────────────────────────
# 0.7.5 moves the plugin registry to a global one and IGNORES the per-session
# `plugins.json` the fork writes, so an upgrade with no re-link comes up with a
# silently empty plugin layer — every binary-level check still passes. The
# re-link therefore belongs in the UPGRADE PATH, not in a troubleshooting note.
# It runs in verify-instance.sh (against the sandbox, where the registry lives)
# and the populated-by-name assertion there is what actually catches the miss.
#
# ── OUTPUT ────────────────────────────────────────────────────────────────
# `staged-build.json` in the plugin state dir, which the collector renders as the
# "ready to install" hint and which update-herdrtest.sh consumes:
#   { path, version, sha256, local_changes, built_unix, bridge_ok }
# A staged build is only written if the release build SUCCEEDED and the resulting
# binary PASSES THE BRIDGE GATE. There is no partial success.
#
# ── EXIT CODES (the escalation contract) ──────────────────────────────────
#    0  a staged build is ready
#   60  the release build FAILED  → escalate (this is the merge-conflict case)
#   61  the build succeeded but the binary failed the bridge gate → escalate
#        (a fork build that lost its bridge is a source-level regression, and
#         shipping it would be exactly the incident this whole system prevents)
#   62  repo/toolchain unusable
#
# Nothing in this script writes to hr, to `~/.local/bin/herdrtest`, or to any
# path a running process execs. It only produces a candidate.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=vd-common.sh
source "$SCRIPT_DIR/vd-common.sh"

BRIDGE_CHECK="$SCRIPT_DIR/herdr-bridge-check.sh"
HERDR_REPO="${HERDR_REPO:-$HOME/Projects/herdr}"
# Isolated target dir on purpose: cargo relinking into the target/ that hr's
# launcher execs would SIGKILL the live instance mid-build.
VD_TARGET_DIR="${VD_TARGET_DIR:-$HOME/.cache/version-dash/prebuild-target}"
VD_STAGE_DIR="${VD_STAGE_DIR:-$HOME/.cache/version-dash/staged}"
STAGED_JSON="$VD_STATE_DIR/staged-build.json"
STAGE=test
export ZIG="${ZIG:-$HOME/.local/zig/0.15.2/zig}"
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Library/Developer/CommandLineTools}"

[[ -d "$HERDR_REPO" ]] || { vd_log "repo not found: $HERDR_REPO"; exit 62; }
mkdir -p "$VD_TARGET_DIR" "$VD_STAGE_DIR" || exit 62

json_field() {
  VD_J="$1" VD_K="$2" python3 -c 'import json,os
try: print(json.loads(os.environ["VD_J"]).get(os.environ["VD_K"], ""))
except Exception: print("")' 2>/dev/null
}

vd_log "prebuild: release build of $HERDR_REPO → $VD_TARGET_DIR"
BUILD_START="$(date +%s)"
if ! CARGO_TARGET_DIR="$VD_TARGET_DIR" \
     cargo build --manifest-path "$HERDR_REPO/Cargo.toml" --release --bin herdr \
     >>"$VD_LOG" 2>&1; then
  # This is the merge-conflict / compile-break path — the one case where a human
  # or an agent genuinely has to think. Hand it over with the compiler output.
  vd_escalate "$STAGE" 60 "the fork release build failed — the tree does not compile"
  exit 60
fi
BUILD_SECS=$(( $(date +%s) - BUILD_START ))
vd_log "prebuild: build succeeded in ${BUILD_SECS}s"

FRESH="$VD_TARGET_DIR/release/herdr"
[[ -x "$FRESH" ]] || { vd_escalate "$STAGE" 60 "build reported success but produced no binary"; exit 60; }

# Copy out of the cargo target dir: a later `cargo build` would relink over this
# path, and the staged candidate must stay byte-stable until it is promoted.
STAGED_BIN="$VD_STAGE_DIR/herdr"
cp "$FRESH" "$STAGED_BIN.tmp.$$" && chmod +x "$STAGED_BIN.tmp.$$" && mv -f "$STAGED_BIN.tmp.$$" "$STAGED_BIN" \
  || { vd_log "could not stage the built binary"; exit 62; }

# The bridge gate applies to our OWN builds too. A fork build that lost its
# bridge (a bad merge dropping the wasm host, a stripped release profile) is
# precisely the binary this system exists to keep away from hr.
BRIDGE_JSON="$("$BRIDGE_CHECK" "$STAGED_BIN" --json --quiet 2>/dev/null)"
BRIDGE_RC=$?
if [[ $BRIDGE_RC -ne 0 ]]; then
  REASON="$(json_field "$BRIDGE_JSON" reason)"
  python3 -c "import os;os.path.exists('$STAGED_JSON') and os.remove('$STAGED_JSON')" 2>/dev/null
  vd_escalate "$STAGE" 61 "the freshly built binary failed the bridge gate: ${REASON:-unknown}"
  exit 61
fi

SHA="$(json_field "$BRIDGE_JSON" sha256)"
VER="$(json_field "$BRIDGE_JSON" version)"
LOCAL_CHANGES="$(cd "$HERDR_REPO" && git rev-list --count "v${VER%%+*}..HEAD" 2>/dev/null || echo 0)"
# Recorded so the trigger can tell "already built this tree" from "the tree moved"
# without rebuilding to find out.
HEAD_SHA="$(cd "$HERDR_REPO" && git rev-parse HEAD 2>/dev/null || echo "")"

VD_B_PATH="$STAGED_BIN" VD_B_VER="$VER" VD_B_SHA="$SHA" VD_B_LC="$LOCAL_CHANGES" \
VD_B_HEAD="$HEAD_SHA" \
VD_B_SECS="$BUILD_SECS" python3 - >"$STAGED_JSON.tmp.$$" <<'PY'
import json, os, time
lc = os.environ.get("VD_B_LC", "0").strip()
print(json.dumps({
    "path": os.environ["VD_B_PATH"],
    "version": os.environ.get("VD_B_VER", ""),
    "sha256": os.environ.get("VD_B_SHA", ""),
    "local_changes": int(lc) if lc.isdigit() else 0,
    "head_sha": os.environ.get("VD_B_HEAD", ""),
    "built_unix": int(time.time()),
    "build_secs": int(os.environ.get("VD_B_SECS") or 0),
    "bridge_ok": True,
}))
PY
mv -f "$STAGED_JSON.tmp.$$" "$STAGED_JSON"
vd_refresh
vd_log "prebuild: staged $VER ($SHA) — ready to install"
exit 0
