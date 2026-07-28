#!/usr/bin/env bash
# T8 — restorable-agents regression (host scope).
#
# Real bug: a cx (herdr:claude) session derived restorable=false, so its rows
# were silently dropped from the historial. The guard is the herdr crate's own
# unit tests that pin the restorable derivation:
#   - agent_resume::planner_allows_supported_agents
#   - pane_close_history::resume_plan_treats_cx_display_agent_as_restorable_claude
#   - app::build_historial_rows_excludes_subagents_and_nonrestorable_agents
#
# We run exactly those three tests, nothing else — the full crate compiles once
# (into the ISOLATED target dir so the live hr server's binary is never
# relinked), then the targeted run is fast. Never modifies the source.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/Projects/herdr-target-testsuite}"

# libghostty-vt is built with a pinned zig (0.15.2). Point build.rs at it if the
# pinned one is installed; the system zig is a newer, incompatible version.
if [ -z "${ZIG:-}" ]; then
  for z in /opt/homebrew/opt/zig@0.15/bin/zig \
           /opt/homebrew/Cellar/zig@0.15/0.15.2/bin/zig; do
    [ -x "$z" ] && { export ZIG="$z"; break; }
  done
fi
[ -n "${ZIG:-}" ] && echo "using ZIG=$ZIG ($("$ZIG" version 2>/dev/null))"
export DEVELOPER_DIR="${DEVELOPER_DIR:-/Library/Developer/CommandLineTools}"

cd "$REPO"
# Named tests across the three modules; --exact-less substring match on each.
# libtest takes multiple filter substrings after `--` (OR-matched); the cargo
# positional TESTNAME slot only accepts one, so all three go after `--`.
cargo test --release --bin herdr -- \
  planner_allows_supported_agents \
  resume_plan_treats_cx_display_agent_as_restorable_claude \
  build_historial_rows_excludes_subagents_and_nonrestorable_agents \
  --nocapture 2>&1
rc=$?
if [ $rc -ne 0 ]; then
  echo "restorable regression tests failed (rc=$rc)"; exit 1
fi
echo "ok: restorable derivation tests pass (cx/claude rows stay restorable)"
