#!/bin/bash
# vd-common.sh — shared helpers for the version-dash update pipeline.
# Sourced, not executed.
#
# Provides the pipeline status file the strip narrates from, the log plumbing,
# and the escalation trigger. Everything here is deterministic: no step decides
# anything by judgement, and the escalation boundary is an EXIT CODE, never an
# opinion.

# Where the strip's state lives: the plugin state dir of the instance HOSTING
# the strip. When an action is launched by the host, that is handed to us as
# HERDR_PLUGIN_STATE_DIR, so the pipeline is automatically correct for whichever
# instance the user actually clicked in — hr in production, a sandbox in a
# sandbox — instead of a hardcoded hr path that a sandbox would silently write
# through to the live instance. The hr default only applies when the scripts are
# run by hand from a shell.
#
# NO hr FALLBACK. This used to end in `:-$HOME/.herdr-hr/state/...`, and that
# silent default is half of the 2026-07-27 incident: a sandbox run resolved it to
# hr and wrote its receipt into the live instance's state dir. The rule now is
# the user's own: a script that can touch an instance must be TOLD which one and
# must fail loudly when it is not. The host always supplies
# HERDR_PLUGIN_STATE_DIR when it launches an action, so the only way to reach
# this error is a hand-run with no instance named — which is exactly when
# guessing is most dangerous.
VD_STATE_DIR="${VD_STATE_DIR:-${HERDR_PLUGIN_STATE_DIR:-}}"
if [[ -z "$VD_STATE_DIR" ]]; then
  printf '✗ refusing to guess which herdr instance this pipeline belongs to.\n' >&2
  printf '  Set VD_STATE_DIR to the plugin state dir of the instance hosting the strip,\n' >&2
  printf '  or run this as a plugin action so the host supplies HERDR_PLUGIN_STATE_DIR.\n' >&2
  printf '  (There is deliberately no default: defaulting to hr wiped four live plugins\n' >&2
  printf '   on 2026-07-27 — see .local/prd/hr-plugin-registry-incident.md)\n' >&2
  exit 10
fi

# Per-instance overrides. A sandbox drops a `pipeline.env` next to its state and
# the buttons act on THAT sandbox's binaries — which is what makes it possible to
# click the real buttons in a test instance without touching the real herdrtest.
# Absent in production, where the defaults are the real paths.
if [[ -f "$VD_STATE_DIR/pipeline.env" ]]; then
  # shellcheck disable=SC1091
  source "$VD_STATE_DIR/pipeline.env"
fi
# Exported so CHILD scripts land in the same place. verify-instance.sh resolves
# VD_STATE_DIR itself and defaults to hr's; without this export a sandbox run
# wrote its receipt into the LIVE hr state dir, where a receipt describing a
# sandbox binary has no business being. Receipts are sha-bound so it could not
# have unlocked anything falsely, but "it fails safe" is not a reason to leave a
# script writing outside the instance it was invoked from.
export VD_STATE_DIR
VD_PIPELINE_FILE="$VD_STATE_DIR/pipeline.json"
VD_LOG_DIR="$VD_STATE_DIR/logs"
VD_COLLECTOR="${VD_COLLECTOR:-$HOME/Projects/herdr/target/release/version-dash-collector}"

mkdir -p "$VD_STATE_DIR" "$VD_LOG_DIR" 2>/dev/null

VD_RUN_ID="$(date +%Y%m%dT%H%M%S)-$$"
VD_LOG="$VD_LOG_DIR/$VD_RUN_ID.log"
VD_STARTED="$(date +%s)"

# ── surviving the parent's pipe ────────────────────────────────────────────
# Plugin actions are launched by the host and run detached. When the invoking
# call returns, the host closes the pipe it handed us — and the next write to
# stderr kills the script with SIGPIPE. That is exactly what happened the first
# time button 1 was invoked through the real action path: the install succeeded,
# then verify-instance.sh died with exit 141 mid-verification and the strip
# reported a verification failure that had never actually run. A pipeline whose
# outcome depends on whether its caller is still listening is not a pipeline.
#
# So when stderr is not a terminal, we own our own output: everything goes to
# the run log, and SIGPIPE is ignored outright.
trap '' PIPE
if [[ -t 2 ]]; then
  vd_log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" | tee -a "$VD_LOG" >&2; }
else
  exec >>"$VD_LOG" 2>&1
  vd_log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >&2; }
fi

# Write the pipeline status the collector reads and the strip renders.
#   vd_phase <stage> <phase> <step> [exit_code] [error] [agent_ref]
# `updated_unix` is refreshed on EVERY call: the collector treats a "running"
# status that has stopped being touched as dead, so a killed action process
# degrades to a visible failure instead of a strip that spins forever.
vd_phase() {
  local stage="$1" phase="$2" step="$3" code="${4:-}" err="${5:-}" agent="${6:-}"
  VD_P_STAGE="$stage" VD_P_PHASE="$phase" VD_P_STEP="$step" \
  VD_P_CODE="$code" VD_P_ERR="$err" VD_P_AGENT="$agent" \
  VD_P_STARTED="$VD_STARTED" VD_P_LOG="$VD_LOG" \
  python3 - >"$VD_PIPELINE_FILE.tmp.$$" <<'PY'
import json, os, time
def opt(k):
    v = os.environ.get(k, "")
    return v if v else None
code = opt("VD_P_CODE")
print(json.dumps({
    "stage": os.environ.get("VD_P_STAGE") or None,
    "phase": os.environ.get("VD_P_PHASE") or "idle",
    "step": os.environ.get("VD_P_STEP", ""),
    "started_unix": int(os.environ.get("VD_P_STARTED") or 0),
    "updated_unix": int(time.time()),
    "log_path": opt("VD_P_LOG"),
    "exit_code": int(code) if code and code.lstrip("-").isdigit() else None,
    "error": opt("VD_P_ERR"),
    "agent_ref": opt("VD_P_AGENT"),
}))
PY
  mv -f "$VD_PIPELINE_FILE.tmp.$$" "$VD_PIPELINE_FILE"
  vd_refresh
  vd_log "[$stage/$phase] $step${err:+ — $err}"
}

# Re-run the collector so the strip updates within a frame or two instead of
# waiting for the 10-minute cadence. `--skip-fetch` semantics come free: with no
# network reachable the collector carries the cached `latest` forward, so this
# stays sub-second even offline.
vd_refresh() {
  [[ -x "$VD_COLLECTOR" ]] || return 0
  "$VD_COLLECTOR" >/dev/null 2>&1 &
}

# Start a background ticker that refreshes the strip every few seconds for the
# duration of a long step, so the elapsed clock actually moves.
VD_TICKER_PID=""
vd_ticker_start() {
  vd_ticker_stop
  ( while :; do sleep "${VD_TICK_SECS:-3}"; vd_refresh; done ) >/dev/null 2>&1 &
  VD_TICKER_PID=$!
  # Detach from job control: without this, killing the ticker makes bash print
  # "Terminated: 15" into the action's captured output, which lands in the log
  # and reads like a failure in a run that succeeded.
  disown "$VD_TICKER_PID" 2>/dev/null || true
}
vd_ticker_stop() {
  [[ -n "$VD_TICKER_PID" ]] && kill "$VD_TICKER_PID" 2>/dev/null
  VD_TICKER_PID=""
}

# ── the escalation boundary ────────────────────────────────────────────────
#
# DETERMINISTIC BY CONSTRUCTION. Escalation is triggered by a failing step's
# EXIT CODE, not by a model reading a log and forming a view about whether the
# failure "looks bad". The contract:
#
#   exit 0            → the step succeeded; the pipeline continues
#   exit 40           → verification failed        → escalate
#   exit 22/23        → the bridge gate rejected a binary → escalate
#   exit 30           → registry damage detected   → escalate
#   any other nonzero → unclassified failure       → escalate
#
# There is no branch in which a script inspects output and decides. The only
# judgement in the whole pipeline happens AFTER the handoff, inside the agent,
# which is where judgement belongs.
#
# What the agent may do unattended vs. what waits for the user is enforced by
# what the agent is given, not by asking it nicely: it runs against the SANDBOX
# only, and every hr-touching path in this pipeline independently refuses
# without the user (see update-hr.sh and verify-instance.sh).
vd_escalate() {
  local stage="$1" code="$2" summary="$3"
  local ref="cx:$VD_RUN_ID"

  if [[ "${VD_ESCALATE:-1}" != "1" ]] || ! command -v cx >/dev/null 2>&1; then
    vd_phase "$stage" failed "$summary" "$code" "automation stopped; no agent handoff configured"
    return 0
  fi

  # Hand the agent structured context, not a vague "it broke": the exact failing
  # step, the exit code, the log, and the standing constraints it must not break.
  local brief="$VD_LOG_DIR/$VD_RUN_ID.brief.md"
  {
    printf '# herdr update failure — stage %s\n\n' "$stage"
    printf -- '- run id: `%s`\n- failing step: %s\n- exit code: %s\n- log: `%s`\n\n' \
      "$VD_RUN_ID" "$summary" "$code" "$VD_LOG"
    printf '## Standing constraints (do not break these)\n\n'
    printf -- '- NEVER restart, kill, or replace the live `hr` instance. It hosts the user'"'"'s session and every agent in it. Restarting hr is the user'"'"'s decision (ctrl+opt+X), never yours.\n'
    printf -- '- Work in the `herdrtest` sandbox only. Do not touch the official `herdr` (~/.config/herdr) or `~/.local/bin/herdrtest-v0.7.3-backup`.\n'
    printf -- '- Install binaries with a same-volume atomic `mv`, never `cp` over a running binary, and never point a cargo build at a live exec path.\n'
    printf -- '- A promotion is only safe if `herdr-bridge-check.sh` accepts the binary AND `verify-instance.sh` writes an ok=true receipt.\n\n'
    printf '## What to do\n\n'
    printf 'Diagnose and fix the failure above, then re-run:\n\n```\nbash %s/verify-instance.sh --label test\n```\n\n' "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    printf 'Leave the pipeline status reflecting reality when you finish.\n\n'
    printf '## Tail of the log\n\n```\n%s\n```\n' "$(tail -60 "$VD_LOG" 2>/dev/null)"
  } >"$brief"

  vd_phase "$stage" escalated "$summary" "$code" "handed to an agent" "$ref"
  # Detached: the action process must not block, and the agent outlives it.
  ( cd "${VD_REPO:-$HOME/Projects/herdr}" && \
    nohup cx -p "$(cat "$brief")" >>"$VD_LOG_DIR/$VD_RUN_ID.agent.log" 2>&1 & ) >/dev/null 2>&1
  vd_log "escalated to an agent (brief: $brief)"
}

# Fail a step: record it and escalate. Callers use `|| vd_fail <stage> $? <what>`.
vd_fail() {
  local stage="$1" code="$2" what="$3"
  vd_ticker_stop
  vd_escalate "$stage" "$code" "$what"
  exit "$code"
}
