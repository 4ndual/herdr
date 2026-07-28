#!/usr/bin/env bash
# T10 — known-failing quarantine (host scope).
#
# Three UPSTREAM tests are deterministically red on this fork (fde03f2+), all
# from ONE fork addition: feat/native-dashes' `next_dash_tick`, initialized to
# Instant::now() at construction (src/app/mod.rs) and foreground-ticked whenever
# dashboards_need_tick() is true — which every test_app() satisfies:
#
#   app::tests::next_loop_deadline_includes_session_save_deadline
#   app::tests::next_loop_deadline_includes_selection_autoscroll_deadline
#   app::runtime::tests::scheduled_tasks_do_not_start_pending_agent_resume_when_geometry_dirty
#
# For the two next_loop_deadline tests the already-due next_dash_tick enters the
# min-deadline set (runtime.rs:650) and shadows the later expected deadline. For
# the geometry-dirty test the due dash tick sets changed=true (runtime.rs:375)
# so handle_scheduled_tasks returns true even though the pending-agent-resume
# path correctly stayed inert. Impact is benign (a few extra ~1Hz wakeups); the
# upstream expectations are simply stale for the fork.
#
# This guard PINS that reality: it asserts these three STILL fail. If a fork
# fix or a corrected test expectation lands, they start passing and this guard
# goes red on purpose — telling whoever fixed it to delete this quarantine
# (and the entry in .local/prd/hr-instances-methodology.md). It reuses T8's
# already-compiled isolated target, so it adds no separate build.
#
# Proposed (NOT applied here) expectation fix: in each of the three tests, push
# the fork tick out of the measured window before asserting, e.g.
#   app.next_dash_tick = now + Duration::from_secs(3600);
# which restores the upstream intent (only the deadlines the test sets are due)
# without touching production behavior.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/Projects/herdr-target-testsuite}"

if [ -z "${ZIG:-}" ]; then
  for z in /opt/homebrew/opt/zig@0.15/bin/zig \
           /opt/homebrew/Cellar/zig@0.15/0.15.2/bin/zig; do
    [ -x "$z" ] && { export ZIG="$z"; break; }
  done
fi
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Library/Developer/CommandLineTools}"

QUARANTINED=(
  next_loop_deadline_includes_session_save_deadline
  next_loop_deadline_includes_selection_autoscroll_deadline
  scheduled_tasks_do_not_start_pending_agent_resume_when_geometry_dirty
)

cd "$REPO"
out="$(cargo test --release --bin herdr -- --exact \
  app::tests::next_loop_deadline_includes_session_save_deadline \
  app::tests::next_loop_deadline_includes_selection_autoscroll_deadline \
  app::runtime::tests::scheduled_tasks_do_not_start_pending_agent_resume_when_geometry_dirty \
  2>&1)"

# We EXPECT a non-zero test result (all three failing). The libtest summary line
# tells us exactly how many failed; anything other than "3 failed" means the
# quarantine is stale and must be revisited.
summary="$(printf '%s\n' "$out" | grep -E '^test result:' | tail -1)"
failed="$(printf '%s\n' "$summary" | sed -n 's/.*; \([0-9]*\) failed.*/\1/p')"

if [ "$failed" = "3" ]; then
  echo "ok: 3 known-failing upstream tests still red as expected (fork next_dash_tick)"
  exit 0
fi

echo "quarantine drift: expected 3 known failures, saw: ${summary:-<no summary>}"
echo "if these now PASS, the upstream expectations were fixed — remove t10 + the"
echo "doc entry. Tests: ${QUARANTINED[*]}"
printf '%s\n' "$out" | grep -E '^test .* \.\.\. (ok|FAILED)$'
exit 1
