#!/bin/bash
# prebuild-trigger.sh — decide, WITHOUT a model, whether a prebuild is due.
#
# Runs on the same LaunchAgent cadence as the collector (see
# install-collector-agent.sh). It is a pure comparison of recorded facts:
#
#   rebuild if   the fork checkout's HEAD sha != the sha recorded in
#                staged-build.json          (the tree moved)
#     or         there is no staged build at all
#     or         the staged binary named in the record is gone, or no longer
#                hashes to what was recorded  (something overwrote it)
#
# NOT rebuilt on: a new upstream release. Upstream landing 0.7.6 does not make
# the fork buildable — merging is a git operation with judgement in it, and this
# trigger deliberately does not attempt it. It builds what is IN the checkout.
# A new upstream release changes `latest` in the strip, which is how the user
# learns a merge is wanted; the merge itself is the escalation path.
#
# THIS IS THE DETERMINISTIC TRIGGER. There is no branch anywhere in the pipeline
# where something reads a log, a diff or a build transcript and forms a view
# about whether to proceed. Every decision is one of:
#   - a string comparison of two recorded shas (here),
#   - an exit code (everywhere else).
# The first point at which judgement is applied is inside the escalated agent,
# after automation has already stopped.
#
# EXIT CODES
#   0  nothing to do, or a prebuild ran and succeeded
#   1  a prebuild was due and FAILED (prebuild-fork.sh has already escalated;
#      the code is passed through so a scheduler can see it)
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=vd-common.sh
source "$SCRIPT_DIR/vd-common.sh"

HERDR_REPO="${HERDR_REPO:-$HOME/Projects/herdr}"
STAGED_JSON="$VD_STATE_DIR/staged-build.json"

[[ -d "$HERDR_REPO/.git" ]] || exit 0
HEAD_SHA="$(cd "$HERDR_REPO" && git rev-parse HEAD 2>/dev/null)" || exit 0

need_build() {
  [[ -f "$STAGED_JSON" ]] || { echo "no staged build yet"; return 0; }
  local rec
  rec="$(VD_F="$STAGED_JSON" VD_H="$HEAD_SHA" python3 -c '
import json, os, hashlib
try:
    d = json.load(open(os.environ["VD_F"]))
except Exception:
    print("staged-build.json is unreadable"); raise SystemExit
if d.get("head_sha", "") != os.environ["VD_H"]:
    print("the fork checkout moved since the last prebuild"); raise SystemExit
p = d.get("path", "")
try:
    h = hashlib.sha256(open(p, "rb").read()).hexdigest()
except Exception:
    print("the staged binary is missing"); raise SystemExit
if h != d.get("sha256", ""):
    print("the staged binary no longer matches its record")
' 2>/dev/null)"
  [[ -n "$rec" ]] && { echo "$rec"; return 0; }
  return 1
}

REASON="$(need_build)" || { exit 0; }

vd_log "prebuild trigger: $REASON — building"
bash "$SCRIPT_DIR/prebuild-fork.sh"
rc=$?
[[ $rc -eq 0 ]] && exit 0
vd_log "prebuild trigger: prebuild failed with $rc"
exit 1
