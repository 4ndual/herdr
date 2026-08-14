
## Planner-B ship-first S bugs (2a/2b/2c) — done

| # | Defect | Fix | Commit |
|---|---|---|---|
| 2a | `fork_argv` catch-all silently resumed unknown runtimes | `_ => {}` → `_ => return None` | `c6ea4368` |
| 2b | Tab context menu offered Fork where forking is impossible (jcode, no agent) | `ContextMenuKind::Tab` carries `forkable`, decided at menu-open from the same authority `fork_argv` uses; Fork-less slice otherwise | `2585602f` |
| 2c | Restart auto-continue hardcoded to claude — resumed codex/pi/jcode panes sat at their prompt with the work abandoned | `Agent::accepts_auto_continue_nudge()` capability predicate | `affdda47` |

Tests: conditional-presence test replaces the position-pinning assumption in
`modal.rs`; `auto_continue_nudges_every_runtime_that_accepts_a_typed_continue`
and `auto_continue_does_not_type_into_an_unsupported_runtime` added.

## Observability fix (first, per the plan correction) — `dd791e10`

Justification per the user's third correction: `plugin.ui.status` reading
0x0 / `layer_count: 0` for a *painting* module is what let four wrong rail
diagnoses survive. Fixed at the source of the ambiguity:

- `panel_content_w/h` came only from the panel SKIP record, so a module that
  rendered fine reported 0x0. Added `rendered_surfaces`, `rendered_nodes`,
  `hit_rects`, `rendered_content_w/h`, measured from the live layer set.
- `wasm_memory_bytes`: guest linear memory is invisible to the process
  footprint, so a leaking module looked identical to an idle one.
- Guest `fd_write` (stdout/stderr) now routes to `tracing::warn` tagged with
  the module identity. UI guests run with no WASI, so anything a module
  printed either failed instantiation with "unknown import" or vanished.
- `.local/qa/tools/rail-control.ts` — permanent rig control. Refuses rail
  diagnosis unless (1) the window fits the display and (2) the rail column
  has pixels in it, cross-checked against the API's `RightPanel` claimants.
  Verified live on :9: window 1592x990 on 1600x1000, rail column painted,
  PASS. Correctly reports the old-server case (`rendered_surfaces` absent)
  and fails when the API is unreachable.

## #32 rail_icon — `844b8537`

Scope reduced to manifest-field wiring per the correction (the rail itself
works). `ui_modules.rail_icon` is now read and validated at load; the
hardcoded builtin-id table in `src/ui.rs` stays only as a fallback. Two
bundled manifests had already declared the field against a host that ignored
it. harness-panel moved off harness-manager's 🎛 so the two are
distinguishable in a one-column rail.

## #25 usage strip — `2f75d11d`

Two independent defects produced the same symptom (the strip showing the
wrong pane's numbers after a focus switch):

1. **Unkeyed snapshot.** One `Option<UsageSnapshot>` for the whole app meant
   "the last thing collected", not "this pane's usage". The out-of-process
   collector made it worse: a run launched for pane A landed after focus moved
   to pane B and was stored as B's. Now keyed by public pane id, with the
   launch recording which pane it was for (`collector_request_pane`), and a
   pane with no snapshot rendering `null` rather than its neighbour's numbers.
   The guest already handles a null snapshot — verified in
   `plugins/usage-dash-ui/src/render.rs`, no guest change needed.
2. **The focus event never fired on a pane-only move.** `sync_ui_plugin_focus_event`
   compared session, cwd and agent but *not* `pane_id`, so moving between two
   panes running the same agent in the same directory emitted nothing at all.
   Confirmed red: reverting just that one line fails the new test.

Focus now also makes the dash tick due immediately (`next_dash_tick = now`)
instead of leaving the strip stale for the rest of the ~1 Hz interval — the
INSTANT-UPDATE rule's mechanism.

Eviction retains against the live pane roster each tick, with a named cap as a
backstop. `agent_supports_subagent_view` replaces the bare claude check.

## #34 colors phase 1, #19 roster scope, #37 hidden tabs — `183c4a2c`

Landed as one commit (they touch the same sidebar projection); the message
records all three.

- **#34**: `try_parse_color` rejects unknown input, next to the lenient
  config-only `parse_color` that answers cyan to everything. The reserved
  `color` metadata token is validated at the *handler*, so a bad value fails
  the request with the prior value intact. Roles resolve against the live
  palette at render, so a tinted item follows a theme change. Pane borders go
  through one `pane_border_color` resolver, dimming a custom color when
  unfocused so the focus signal survives. Ghost references to a
  `recolor_to_palette` that does not exist are gone.
- **#19**: roster scope restored (all / space / tab) as presentation state —
  no serde, no wire — that *narrows* a plugin's `agent_view` rather than being
  replaced by it, with its own header hit-rect (deliberately live while a
  plugin view is active, unlike the sort toggle) and a scoped-empty message so
  a narrowed panel never renders blank.
- **#37**: `tab.hide` + `tab.create --hidden` + `tab list --hidden`, all
  through the one `set_tab_hidden` setter. The sidebar now filters hidden tabs
  (#18 B1) — previously a hidden tab was gone from the tab bar and fully
  listed in the sidebar, so the feature read as "move the tab". Tab cycling
  routes through `next_visible_tab`, so a hidden tab is no longer a dead
  keypress (#18 B2). `tab.show` on an already-visible tab succeeds.

Two frozen frame digests in `ui/tab_surface.rs` were updated for the new scope
control, after proving with a suppressed-control run that it is the *only*
frame change.

## #18 test debt, #26/#24 design, #42/#20 portability — `4bdabb1d`

- Carve release on plugin disable shipped in `516c91e6` with zero tests; the
  lifecycle wiring now has one.
- `.local/prd/26-runtime-parity-design.md` — the manifest `[commands]` spec
  with its full validation table. Not implemented: the value is a string typed
  into a live agent REPL and manifests can be remote. Verified while writing:
  `/clear` is hardcoded at two `goal-dash-core` sites, and `/remote` exists
  nowhere in the tree.
- `.local/prd/24-lower-bar-feasibility.md` — the five structural constraints a
  design has to absorb, read out of the tree rather than recalled.
- Single-root sweep: `fork-conversation` and `relocate-conversation` searched
  only the default claude root, so both *failed on harness panes specifically*.
  `claude_roots()` added to both `dash-common.sh` copies and verified live.
- macOS: the runtime swap silently returned an empty env on any Mac (no
  `/proc`), launching the new runtime against the default config root with the
  carried session missing. Falls back to `ps -Eww`. Unverified on real macOS —
  that is #22's Mac rollout.
