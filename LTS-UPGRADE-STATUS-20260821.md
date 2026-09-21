# LTS upgrade v0.8.0 → v0.8.2 — status (2026-08-21)

Branch: `trial/lts-0.8.2-merge`, merge commit `9978c467`
(two real parents: `ae663bd8` fork + `9eb52145` upstream v0.8.2)

**NOT DEPLOYED.** hr is still running the standalone idle-repaint fix.

## Scope

133 upstream commits (113 behavioral / 20 docs+release).
490 files merged clean, 32 conflicted — all resolved by UNION.

## The central decision: union, not replacement

Your rule was "if upstream implements it natively and fully replaces ours, take it."
For pane graphics the answer is **no** — the two sides solve different halves:

- UPSTREAM optimizes TRANSPORT for kitty-capable terminals: z_index compositing,
  BGRA, direct file-frame streaming (400 MiB vs our 16 MiB cap), ack/sequence/
  revision protocol, aggregate resource caps, capability discovery.
- THE FORK owns SEMANTICS upstream has no equivalent for: layers keyed by
  (owner, layer_id) for untrusted wasm-plugin writers, `source_path` +
  graphics_roots allowlist, the half-block fallback that renders images on
  terminals WITHOUT kitty support, plugin-panel images, the Claude `[Image #N]`
  preview — plus `pane.text.*` / `pane.carve.*`, which merely live in the same
  files and would have been deleted by a wholesale swap.

Upstream's plugin bridge never threads an `owner` through `pane.graphics` at
all, so taking upstream wholesale is not a merge conflict — it is a missing
subsystem.

**Result:** graphics set/clear/stream now DUAL-WRITE — upstream's
`pane_graphics::Runtime` slots (kitty path) and `state.pane_graphics_layers`
(half-block path). Both renderers keep working.

## Verification status

| gate | state |
|---|---|
| `cargo check --release --bin herdr` | 0 errors |
| `cargo check --release --bin herdr --tests` | 0 errors |
| API schema | regenerated from merged types (`HERDR_UPDATE_API_SCHEMA=1`), protocol 20 |
| full suite (3614 tests, 3 runs) | 19 / 17 / 16 failures |
| — of which environmental | 12 (identical set fails on BASE while an hr server is live) |
| — genuine regressions | 5, being fixed |
| — golden digests | 2, deliberately left failing (see below) |
| e2e / runtime image rendering | NOT DONE |

### The 5 genuine regressions — 4 FIXED (commit b6983ad0), 1 open
1. `headless_empty_dash_tick_does_not_request_a_repaint` — **the merge re-broke the
   deployed idle-repaint fix.** Instrumented: `frame_changed` is true with no UI
   modules loaded (manifests/modules/ui_events/pending all false). Root cause
   under investigation — likely upstream registering a built-in module/surface
   (tab-bar status) that the fork's tree did not load by default.
2. FIXED `static_set_and_clear_reclaim_inactive_stream_layers` — upstream's test
   needed porting to the fork's owner-aware clear API (an EMPTY owner means the
   privileged socket caller, which by existing design ignores layer_id and wipes
   the whole pane). Production dual-write was correct as merged.
3. FIXED `unwritten_direct_full_falls_back_without_stickiness_but_disconnect_retires`
   — never actually broken; failed only because the crate didn't compile.
4. FIXED `manifest_action_invoke_injects_plugin_paths` — same, compile-only.
5. FIXED `tab_click_survives_stray_drag_report_off_the_tab_bar` — and this one
   found a REAL PRE-EXISTING BUG IN THE FORK, not a merge artifact:
   `usage_host::route_mouse` swallowed every Drag/Up landing on the usage strip
   via a catch-all, even when the event completed a gesture anchored elsewhere.
   A tab click whose mouse-up landed over the strip was silently lost; split
   border, scrollbar and workspace drags ending over the strip break the same
   way. Fixed by skipping the swallow while a gesture is in flight. The fork
   never caught this because upstream has no usage strip to test against —
   the LTS upgrade paid for itself here.

Suite after the fixes: **3599 passed / 15 failed** = 12 environmental + 2 golden
digests + 1 open (regression #1). No unexplained failures.

### Why the 2 golden digests are LEFT FAILING on purpose
`fork_geometry_*` and `upstream_geometry_*_semantic_frame_is_characterized` are
characterization tests over rendered frames. Upstream changed theme rendering, so
they legitimately need new values — but regenerating them BEFORE the rendering is
visually verified would bless whatever the merge produced, including the BGRA bug
below if it is real. Regenerate only after the image e2e passes.

## Flagged for your judgment

1. **`layer_id` resolved to upstream's `Option<String>`** (the fork had `String`).
   Matches every call site's `.as_deref()`/`Some(..)`/`None` usage, but it is a
   real API-shape decision made during conflict cleanup.
2. **BGRA channel order — RESOLVED, not a bug.** Verified on the merged binary:
   a red/green/blue/white quadrant image renders pixel-correct via BOTH the
   `source_path` path AND the `data_base64` path, each against a
   verified-blank baseline (the previous layer was cleared first, so nothing
   could show through). Red stays top-left, blue stays bottom-left. The concern
   below was raised from code reading; it does not reproduce in either transport.

   Original concern, kept for the record:
   **BGRA channel order is inconsistent.** The half-block fallback now swaps
   B/R correctly. `src/kitty_graphics.rs` `pane_graphics_kitty_format` maps
   `Bgra -> KittyImageFormat::Rgba` and passes raw bytes through WITHOUT a swap
   (pre-existing upstream code, untouched). If that is wrong, BGRA frames render
   red/blue swapped over the kitty protocol while rendering correctly in the
   fallback. A test image (`e2e-img/quadrants.png`, red/green/blue/white
   quadrants) is prepared to settle this visually on both paths.
3. **The graphics dual-write itself** — two stores kept in sync by hand is a
   maintenance cost; worth deciding whether to converge them later.

## Remaining work before this could ship

1. Fix the 5 regressions (in progress)
2. Verify image rendering on BOTH paths with the quadrant image (kitty + half-block)
3. Regenerate the 2 golden digests only after step 2 passes
4. Build the binary, deploy to a THROWAWAY instance (not hr), soak ≥1 h
5. Only then consider hr

## Pre-existing defect found on the way (not caused by this merge)

The test suite is NOT hermetic: `detect::manifest*`, `detect::manifest_update*`,
`read_ui_input_blob*`, the two keybinding tests and `plugin_collector timeout`
read real manifest caches / XDG state, so they fail on ANY branch whenever a
live hr server runs on the same box. On a quiet box the same base commit scores
3317/3317. Never judge an hr change from a single suite run — run base and
candidate 3x each and compare FAILING SETS, not counts.

## Root cause of regression #1 (diagnosed, NOT fixed — deliberately)

`headless_empty_dash_tick_does_not_request_a_repaint` fails because
`dispatch_ui_plugin_frame_events()` returns true. Instrumenting the gate and then
the dispatch itself showed why:

- gate terms in the failing fixture: `manifests=false modules=false
  ui_events=false pending=false frame=true`
- the fixture DOES load modules after the merge:
  `builtin.goal-dash:goal [RightPanel]`, `builtin.harness-manager:harness
  [RightPanel]`, and others
- the error path (`failed to handle ui plugin event`) still returns `false`,
  same as pre-merge — so this is NOT a failed-dispatch miscount
- the success path correctly returns `layers_changed || had_effects`

So the test's PREMISE — "no UI modules are loaded, so the tick genuinely produces
nothing" — is no longer true on the merged tree. A module that legitimately
produces layers on its first frame makes the tick a real change.

**Why this was left unfixed rather than patched:** the obvious move is to adjust
the fixture so no modules load. That is also exactly how a real performance
regression gets blessed by a green test. Deciding between "the fixture is stale"
and "the merge made modules repaint every tick" requires measuring actual idle
repaint rate on the MERGED binary — a fresh soak against the same
`render_ui_plugin_layers` counter used to prove the original fix (pre-fix 7/sec,
post-fix 0/sec). That measurement has not been run.

Do NOT resolve this by editing the test until that soak exists.

## Branch survey re-run AFTER the v0.8.2 merge (priority + quality decision)

Re-surveyed every branch still ahead of `trial/lts-0.8.2-merge`:

| branch | src files differing | behind merge | verdict |
|---|---|---|---|
| `origin/task/doctor-dash` | **0** | 569 | already in — fully superseded |
| `origin/plugin-v1` | 62 | **893** | stale (Jun); the fork's own plugin system long overtook it |
| `origin/task/wave1-no-images` | 11 | 576 | stale (Jul) |
| `origin/task/historial-persist-fix` | 4 | 569 | stale (Jul) |
| `integration/linux-e2e` (+origin) | 5 | 257 | newest (Aug 5), carries the inferred-pgid acquisition fix |
| `origin/task/linux-compat` | 4 | 257 | same vintage |
| `agent/fix-ui-render-loop` | — | — | NOT a candidate: its two commits were cherry-picked, so the
  original SHAs are not ancestors but the CONTENT is (as `1f68ce82`/`ae663bd8`) |

**Decision: merge nothing further.** The only recent candidate is the pgid
acquisition fix (`01a79fc4`, Aug 5, `src/pane.rs`). Upstream v0.8.2 reworked that
subsystem — the merged `pane.rs` holds 129 pgid-related lines and the branch diff
runs in BOTH directions, i.e. divergent implementations, not a missing patch.
Applying a 257-commits-behind patch on top of upstream's newer rework would fight
it. If the pgid symptom recurs after the LTS upgrade ships, re-derive the fix
against the CURRENT code rather than replaying that branch.

The features prioritised in the goal — native invisible pane, workspace and
colors — needed no branch work: they were already merged into the deployed
branch (`10a76c77` hidden/restorable live tabs, `752d715d` "Make invisible"
dispatch tests, `183c4a2c` hidden tabs + per-item colors + agents-panel roster
scope, `3e7b10ce` workspace colour persistence). Verified live: the Make
invisible round-trip was driven through the real UI and the tab restored with
all 4 panes intact.

## Per-branch e2e on the merged binary (the decisive test)

Static comparison said no branch was needed; this verified it by exercising each
branch's FEATURE AREA against the merged binary, without that branch applied:

| branch | feature area | result on merged binary |
|---|---|---|
| `origin/plugin-v1` | plugin API | **221 passed, 0 failed** |
| `origin/task/linux-compat` | plugin geometry | **17 passed, 0 failed** |
| `origin/task/historial-persist-fix` | historial | **6 passed, 0 failed** |
| `origin/task/wave1-no-images` | image handling | **48 passed, 0 failed** |
| `integration/linux-e2e` | foreground-group / pgid | **15 passed, 0 failed** |
| `origin/task/doctor-dash` | dash | **6 passed, 0 failed** |

313 tests, zero failures. Note the pgid branch's OWN named reproduction
(`live_handoff_keeps_unmanaged_agent_name_bound_to_saved_session`) does not exist
in the merged tree — it was a branch-local test — and its symptom was reported
"confirmed via local reproduction on macOS"; this box is Linux.

**FINAL MERGE DECISION: merge nothing beyond v0.8.2.** Every candidate branch's
functionality is present and passing on the merged tree. Merging any of them
would replay 250-900-commit-old code over a subsystem upstream has since
reworked — strictly more risk than value.

## Gap the LTS upgrade does NOT close: invisible WORKSPACES

Asked directly during this session. Confirmed absent three ways:
- no `hidden` field on `Workspace` anywhere in `src/workspace/` (the concept
  exists only on `Tab`)
- the workspace context menu is hardcoded:
  `ContextMenuKind::Workspace { .. } => vec!["Rename", "Close"]`
- the API has `tab.hide` / `tab.show` but NO `workspace.hide` / `workspace.show`
  (`workspace.*` = create/close/focus/get/list/move/rename)

Upstream v0.8.2 does not add it either, so the upgrade will not bring it.

Of the three prioritised features:
- invisible TABS — exists, verified live (hidden via UI click, restored via
  `tab.show`, 4 panes intact)
- workspace COLORS — exists (`3e7b10ce`, already merged)
- invisible WORKSPACES — **does not exist; would be new work**, closely modelled
  on hidden tabs: a `hidden` flag on Workspace, `workspace.hide`/`workspace.show`
  mirroring the tab pair, a context-menu entry, sidebar filtering, persistence.
