# 0003 — Inferred process groups must still drive acquisition, not just kernel-observed ones

Status: accepted
Date: 2026-08-05
Context: linux-compat merge blocker — `live_handoff_keeps_unmanaged_agent_name_bound_to_saved_session`
regressed by `38b7b540` ("fix: add opt-in child-group process detection (#2052)")
Host baseline: `bc9d3087` (`task/linux-compat`), fix on `task/linux-e2e-fix-pgid-detection` (`01a79fc4`)

## Problem

`38b7b540` added `process_group_for_change_tracking()` so that `last_foreground_pgid`
(the value compared against next tick's kernel-observed pgid to decide whether "the
foreground group changed") only latches onto a **kernel-observed** foreground pgid,
never a merely **probed/inferred** one:

```rust
fn process_group_for_change_tracking(
    observed_foreground_pgid: Option<u32>,
    probed_process_group_id: Option<u32>,
) -> Option<u32> {
    observed_foreground_pgid?;
    probed_process_group_id.or(observed_foreground_pgid)
}
```

That fixed a real bug: once an agent is already identified, if the kernel never
reports a foreground pgid (`None` every tick) but the process probe keeps inferring
the same group via job/leader lookup, the old code (`last_foreground_pgid =
process_group_id.or(foreground_pgid)`) would latch onto the inferred value —
making `foreground_group_changed(None, Some(inferred))` true on *every single
tick* forever, causing constant unnecessary re-probing (`38b7b540`'s own test,
`inferred_group_does_not_trigger_a_probe_on_every_tick`, pins exactly this case).

But the same function is also used **before** an agent is identified, during
acquisition. There, discarding the inferred value has a different effect:
`last_foreground_pgid` never latches onto anything, so
`foreground_group_changed()` can never fire, `acquisition_started_at` never
starts, and `should_probe_foreground_job()` falls through to the 30-second
`PROCESS_RECHECK_MISSING_FOREGROUND_GROUP` fallback instead of the
500ms/2s acquisition-window cadence (`PROCESS_ACQUISITION_FAST_RECHECK` /
`PROCESS_ACQUISITION_SLOW_RECHECK`, bounded by an 8s `PROCESS_ACQUISITION_WINDOW`).
If the very first probe (fired once, unconditionally, on the first tick) doesn't
catch the agent process before it's fully identifiable, detection now has to wait
30 seconds for the next attempt — far past the test's ~8s acquisition deadline.

Confirmed root cause by tracing the full chain
(`process_group_for_change_tracking` → `last_foreground_pgid` →
`foreground_group_changed` → `acquisition_started_at` →
`should_probe_foreground_job`) against the exact assertion the test hits
(`tests/live_handoff.rs:1357`, "agent process was not detected"). The test's own
`agent.get` poll deadline is ~5s; the observed ~8.7–8.8s panic time is that
deadline plus ~3.7–3.8s of pty/process setup and teardown overhead before the
deadline clock starts (per the prior triage's `origin/main` vs `linux-compat`
comparison in this log). With `acquisition_started_at` never set, detection has
only the unconditional first-tick probe plus the 30s
`PROCESS_RECHECK_MISSING_FOREGROUND_GROUP` fallback to work with — both fall
outside the test's ~5s poll window if the first probe doesn't catch the agent
immediately, instead of the 500ms/2s acquisition cadence
(`PROCESS_ACQUISITION_FAST_RECHECK` / `PROCESS_ACQUISITION_SLOW_RECHECK`,
bounded by the 8s `PROCESS_ACQUISITION_WINDOW`) that `acquisition_started_at`
being set would have enabled.

This is platform-dependent because it hinges on whether the kernel-observed
foreground-pgid read (`crate::detect::foreground_process_group_id`) reliably
returns `Some` early, or stays `None` while the probe's job/leader-based
inference still succeeds:

* **macOS** (`src/platform/macos.rs`) — local reproduction, 5/5 fail,
  ~8.7–8.8s panics each, on unmodified `bc9d3087`.
* **Linux** (crabbox sprite `violet-lobster`, Fly Sprite, kernel `6.12.91-fly`,
  `/proc`-based `tpgid` read in `src/platform/linux.rs`) — 10/10 pass on
  unmodified `bc9d3087` (5/5 in `Native` mode, 5/5 with
  `HERDR_PROCESS_DETECTION=child-groups` forced). Linux's `/proc/<pid>/stat`
  read is cheap and reliably observed, so the kernel-observed-only rule never
  actually engages during this test's acquisition window there — the
  regression is real but invisible on native Linux.

## Decision

Gate `process_group_for_change_tracking`'s kernel-observed-only rule on whether
an agent is already identified:

```rust
fn process_group_for_change_tracking(
    observed_foreground_pgid: Option<u32>,
    probed_process_group_id: Option<u32>,
    has_identified_agent: bool,
) -> Option<u32> {
    if has_identified_agent {
        observed_foreground_pgid?;
    }
    probed_process_group_id.or(observed_foreground_pgid)
}
```

Both call sites (`spawn_basic_detection_task`'s loop, used only after a live
handoff reconnect, and `PaneRuntime`'s inline loop, used for a pane's normal
lifecycle and the one actually exercised by the failing test) pass
`agent.is_some()` for the new parameter, where `agent` is the presence's current
agent captured at the top of that tick.

Before identification, an inferred group is latched exactly as it was
pre-`38b7b540`, restoring the fast acquisition cadence. Once identified, only
kernel-observed groups are latched, keeping `38b7b540`'s anti-churn fix intact.
The two rules are strictly phase-gated and never needed simultaneously.

## Alternatives considered

1. **Drop the `process_group_changed` condition that gates
   `acquisition_started_at`'s reset, instead of fixing the tracked value.**
   Rejected: broadens the change beyond the actual defect (the tracked value),
   and loses the "did anything actually change" signal that the pattern relies
   on elsewhere; the `has_identified_agent` gate is the smaller, more localized
   fix, isolated to the one function actually responsible for the regression.
2. **Revert `38b7b540` entirely.** Rejected: it fixed a real steady-state
   over-probing bug (per its own test and stated intent). Reverting trades the
   acquisition regression for reintroducing that churn.
3. **Lower `PROCESS_RECHECK_MISSING_FOREGROUND_GROUP` (30s) instead.**
   Rejected: papers over the real defect (`acquisition_started_at` never
   starting) with a blunter, unrelated timing knob that also governs the
   legitimate long-lived-idle-shell case `38b7b540`'s second commit ("fix:
   preserve lifecycle probes without foreground groups") was tuning.
4. **Track two separate "last group" values (kernel-only and probed) instead
   of a boolean gate.** Rejected: more state threaded through both call sites
   for no behavioral difference over a boolean, since the two rules are
   strictly phase-gated and never both needed at once.

## Conflict posture

`38b7b540` is itself fork-local (not upstream), and this change only touches
`process_group_for_change_tracking`'s third parameter plus its two call sites —
a small, additive, easily-rebased diff with low future-conflict risk.

## Verification

* New unit test `inferred_group_still_drives_acquisition_before_an_agent_is_identified`
  (`src/pane.rs`) pins the restored acquisition-phase behavior: an inferred
  group with no kernel observation and no identified agent is still latched,
  `foreground_group_changed` fires on the next comparison, and
  `should_probe_foreground_job` returns true well inside the fast-retry window.
* Existing `inferred_group_does_not_trigger_a_probe_on_every_tick` updated to
  pass `has_identified_agent: true`, still pinning `38b7b540`'s steady-state
  anti-churn behavior unchanged.
* Linux (crabbox sprite, same box as the pre-fix baseline): 5x rerun of
  `live_handoff_keeps_unmanaged_agent_name_bound_to_saved_session` plus the
  full `live_handoff` binary suite, `env -u HERDR_SOCKET_PATH -u
  HERDR_CLIENT_SOCKET_PATH -u HERDR_SESSION`, `--no-fail-fast` — see
  `e2e-campaign/logs/orchestrator-state-resumed.md` for the exact run output.
* The full-suite run surfaced one unrelated failure,
  `live_server_holds_one_pty_master_fd_per_pane` (panics at
  `tests/live_handoff.rs:512`, "replacement server ... did not appear").
  Isolated it by running the same test 3x on the fix branch and 3x on
  unmodified `bc9d3087`: it fails deterministically (3/3, ~10.7s each) on
  **both**, with an identical panic message. This is a pre-existing,
  box/environment-specific failure (unrelated to `process_group_for_change_tracking`
  or anything this fix touches) and is out of scope for this change.

## Known gap

Empirical macOS re-verification of this fix (re-running the original 5/5-fail
repro on macOS to confirm it now passes 5/5) was explicitly skipped for this
pass, per instruction, to keep all build/test cycles on the Linux sprite. Linux
never exercised the underlying bug in the first place (see above), so the
Linux-only verification here is a regression-safety check plus unit tests
pinning the exact fixed logic branch — it is **not** an empirical confirmation
that the macOS failure mode is resolved. That confirmation should happen before
treating this merge blocker as fully closed.
