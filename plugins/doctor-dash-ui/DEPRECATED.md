# DEPRECATED (2026-08-13)

`builtin.doctor-dash` — the "one-click healer" overlay (a `[🩺]` bottom-dash
button opening a modal of health checks driven by `~/.config/herdr/fixers.json`)
is deprecated by operator decision. It never worked in practice and is not
coming back in this shape.

**State when this note was written.** The plugin has no source in this branch.
It exists only on `origin/task/doctor-dash` (single commit `d2a9a192`,
2026-07-27, 13 files) and was never merged. It is registered in **no** instance:
absent from `~/.herdr-hr/config/herdr/plugins.json`,
`~/.herdr-test/config/herdr/plugins.json`, and every other live registry.
Nothing in `plugins/`, `~/Projects/herdr-plugins/`, or any linked plugin root
references it. So there is nothing to unlink, disable, or strip — the
deprecation is the decision itself, recorded where the next person will look.

**Do not merge `origin/task/doctor-dash` and do not link it.** If the healer
idea returns, design it fresh against the current plugin API rather than
reviving that branch.

**Do not delete the two host references to it.** `src/app/mod.rs`,
`src/app/ui_capture.rs`, `crates/herdr-ui-plugin-api/src/lib.rs`, and
`ARCHITECTURE-DECISIONS.md` (ADR-007) cite doctor-dash as the motivating example
for the *"keys are broadcast to UI plugins but never consumable"* guarantee
(`offer_key_to_ui_plugins`). That guarantee is still live and still load-bearing
for other plugins; only the example is dead. Those comments should be reworded
to stop naming a plugin that does not exist — a source-only change, deliberately
left for a host-rebuild pass.

Follows the same pattern as `plugins/harness-manager-ui/DEPRECATED.md`.
