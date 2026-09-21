# hr idle-repaint fix — e2e report (2026-08-21)

Branch `fix/hr-idle-repaint-20260821` = `feat/tidus-bar-codenames` + two cherry-picked
commits authored 2026-08-15 by 4ndual, never merged into the deployed branch:

- `89dced0d` fix(ui): stop the 1 Hz dash tick forcing a full repaint on an idle screen
- `65afef9f` fix(ui): keep the direct-mode repaint unconditional — it delivers the tick

## The defect

`handle_scheduled_tasks_headless` ended its dashboard-tick block with an unconditional
`changed = true`. The block is gated on `dashboards_need_tick()`, true whenever any UI
module is loaded — permanently, in production. The server therefore reported a change
once per DASH_TICK_INTERVAL forever and repainted every plugin layer on an idle screen.

## Reproduction (terminals only, zero agents)

24 terminal panes, nobody typing, on the deployed binary:

    render_ui_plugin_layers layers=9 usage_nodes=7 usage_rect=... x7 per second

## Result after the fix

| measurement | pre-fix | post-fix |
|---|---|---|
| idle repaints | 7/sec (178 per 30 s) | **0** |
| 60 s idle window, client attached | ~360 repaints | **0 repaints, 3% CPU** |
| soak average CPU | — | 3.1% |
| RSS idle | — | 33.7 MB (peaks ~56 MB under UI activity, returns) |

## No regressions

Full suite, 3 runs per branch, identical conditions:

| | run 1 | run 2 | run 3 |
|---|---|---|---|
| base `7f8fa291` | 11 fail | 12 fail | 11 fail |
| fix `ae663bd8` | 11 fail | 11 fail | 12 fail |

Tests failing on the fix but NOT on base: **none** (fix's failing set is a strict subset).
The fix's own regression test `headless_empty_dash_tick_does_not_request_a_repaint` passes.

### Pre-existing test-suite defect (separate finding, NOT caused by this change)

`detect::manifest*`, `detect::manifest_update*`, `read_ui_input_blob*` and the two
keybinding tests are **not hermetic** — they touch real manifest caches, XDG state and
keybinding config, so they fail whenever a live hr server is running on the same box.
On a quiet box the same base commit scores 3317/3317. Worth fixing independently.

## No features removed (driven through the real UI, recorded)

Verified by real clicks/keystrokes on an isolated X display, recorded to
`rec/hr-e2e.mp4` with burned-in phase + wall-clock watermarks:

- 29 panes and 11 tabs restored across the binary swap
- click → focus → keystroke transcription: `HR-E2E-CLICK-PANE1-OK`, `PANE2-FOCUS-OK`, `PANE3-OK`
- tab context menu intact: New tab, Rename, Archive, **Make invisible**, Close,
  Convert to workspace, Move to workspace…, Copy metadata
- **Make invisible** round-trip: hidden via UI click (tab vanished from the bar, API still
  reported 11 tabs = hidden not closed) → revealed via `tab.show` → `hidden=false`,
  focused, all 4 panes intact
- all plugin layers render (usage strip, harness chips, keybinding chips, version line)
- zero `failed to handle ui plugin event` warnings

## Mage-VL fact-check of the recording

Confirmed: phase watermarks read verbatim and progress in order
(`07-RIGHTCLICK-tab-colors-invisible` → `08-CLICK-make-invisible`); hr in the foreground
with real content; pane markers read back correctly; selected tab shifts 6 → 5 after the
hide, consistent with the claim.

**Limitation, stated rather than hidden:** Mage-VL confabulates on repeated small-digit
patterns — it invented panes 4/5/6 with `PAN4-OK`-style output that do not exist, and
enumerated tabs as `1..9,0`. Its enumerations were therefore NOT used as evidence; only
verbatim watermark reads, marker text and the selection change were counted.

## Deployment note

The binary resolves plugin roots from its build location. Building in a worktree without
the wasm plugins produced 139 `failed to read wasm ui module` warnings for
`builtin.harness-manager`. Fixed by mirroring the built `.wasm` artifacts into the
worktree; re-verified at zero warnings. Any future build outside `~/Projects/herdr` needs
the same step.

Rollback: `cp <scratch>/herdr-PREFIX-backup ~/.herdr-hr/bin/herdr` (pre-fix binary kept).

## Memory: 1-hour concurrent A/B (added after the first soak showed RSS drift)

The first post-fix soak showed RSS climbing ~5.5 MB/h while idle. That is not
"no leak", so a matched pre-fix instance was built (isolated XDG root, own
socket, own tmux session, 33 panes vs 29 — biased against the fix) and both were
soaked for one hour concurrently on the same box.

| binary | avg CPU | RSS first→last | slope |
|---|---|---|---|
| PRE-FIX | 1.00% | 48424→50216 kB | **+2756 kB/h** |
| POST-FIX | 2.18% | 37824→39372 kB | **+1686 kB/h** |

**Conclusion: the fix introduces no leak — the drift is pre-existing and larger
without it.** A slow ~1.7–2.8 MB/h RSS drift exists in herdr regardless; on a
24/7 box that is ~40–65 MB/day and deserves a separate investigation.

**The CPU column of this A/B is NOT valid evidence** and was not used: the bare
test instance has no user plugins, 0 harness chips and no attached client, while
the production instance runs 11 tabs, the full plugin set and a live client.
CPU evidence rests on the same-instance before/after only (7 repaints/sec → 0).

Note: the bare pre-fix instance still logged 3195 idle repaints in one hour with
zero configured plugins, confirming the defect fires from built-in modules alone.
