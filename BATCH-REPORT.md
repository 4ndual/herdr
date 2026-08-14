
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
