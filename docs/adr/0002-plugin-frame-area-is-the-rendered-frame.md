# 0002 — A plugin's geometry resolves against the rendered frame, not a union of view rects

Status: accepted
Date: 2026-08-05
Context: task #144 (hr plugins Linux E2E), task #99 (archive-dash broken in live hr)
Host baseline: `752d715d` (accept-080, upstream v0.8.0 merge)

## Problem

A wasm UI module reads `frame.rect` as **its absolute screen origin** and resolves
raw mouse events against it (`archive-dash` hit-tests chips this way; it declares
no hit-rects, by design, because the host suppresses hover over declared rects).

The host had two different notions of "the frame":

* `render_ui_plugin_layers` (src/ui.rs) resolves a panel against the **real**
  `frame.area()` and paints the layer's first line at `area.y`.
* the frame context handed to the guest (src/app/mod.rs, `ui_plugin_frame_area`)
  resolved the panel against a rect **reconstructed as the bounding union** of
  `sidebar/tab_bar/terminal/mobile_header/right_panel/usage_strip` + panel rects.

The union cannot see any row that no view rect covers. When the union is shorter
than the real frame, a band carved from its bottom lands *above* where the same
band is painted, so the origin published to the guest is not the origin the guest
is drawn at.

Measured on Linux (crabbox sprite, 190x55, host `752d715d`, archive-dash with
three seeded archives): chips **paint** on rows 48/49/50 while hover/click
**resolve** on rows 46/47/48 — a constant 2-row shift. Hovering chip 1 showed
chip 3's résumé; the same shift routes `restore`/`fork`/`delete` to the wrong
record, and rows 49-50 are dead. That is the user-visible "archive-dash is
broken" report (#99): the panel renders, so it looks alive, while every chip
resolves to the wrong archive or to nothing.

This is not archive-dash-specific. It hits any module that resolves raw mouse
against `frame.rect`; declared-hit-rect modules are affected too, since
`input/mouse.rs` converts to surface-local coordinates with the same helper.

## Decision

Record the frame `compute_view` laid out (`ViewState::frame_area`) and return it
from `ui_plugin_frame_area`. The union stays only as a fallback for callers that
run before the first layout pass (tests, early API calls), where no frame exists
yet and the old behaviour is no worse.

## Alternatives considered

1. **Have the guest correct for the offset.** Rejected: a guest cannot observe
   where it is painted, so it would be guessing a host-internal delta — and every
   plugin would have to reimplement the same guess.
2. **Pass a paint origin alongside `frame.rect`.** Rejected: two origins in the
   ABI is the bug made permanent, and it needs an API-version bump for something
   that is simply the host disagreeing with itself.
3. **Fix the union to cover the missing rows.** Rejected: it re-derives geometry
   the layout pass already computed exactly. Every future surface would have to
   remember to join the union — the same trap, rearmed.
4. **Resolve the guest's rect during render, where a `Frame` is in scope.**
   Rejected: frame dispatch is driven from `App`, not the renderer; threading a
   `Frame` into it inverts the layering.

## Conflict posture

Per the fork's compatibility doctrine this diminishes future conflicts: it
*deletes* a fork-local reconstruction of upstream layout instead of extending
it, so upstream changes to view geometry are picked up automatically rather than
needing a matching edit to a parallel implementation. If upstream later publishes
a frame area of its own, this field is the natural thing to drop.

## Verification

* `ui_plugin_frame_area_is_the_frame_the_renderer_laid_out` (src/ui.rs) pins
  both halves: the helper returns the laid-out frame, and the panel rect the
  guest is told about shares the row the panel is painted at.
* E2E on Linux: with the unfixed host, the choreography only works when aimed 2
  rows above the paint (`HIT_DY=2`); the archive-dash payload path itself is
  sound — `[fork#1]` resolved slot 1 → `pi --fork sess-gamma-0003` → pane `w1:p2`.
