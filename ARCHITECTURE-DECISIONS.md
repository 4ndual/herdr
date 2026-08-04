# Architecture decisions

This file records fork-only core changes for the update pipeline and FIXER AGENT. Before resolving an upstream merge or an end-to-end failure, preserve the goals and public seams below. Apply the update-pipeline matrix: prefer upstream-shaped seams and the smallest fork surface; rearchitect the custom side when either side breaks; deprecate a custom feature when native functionality fully replaces its goal.

## ADR-001: Persistent invisible tabs

**Goals.** Hide a tab from tab-bar geometry, hit testing, and ordinary navigation without closing its panes, PTYs, layouts, agents, notifications, or runtime registrations; keep the state across restart; provide a mouse-visible `Make invisible` tab-menu item; expose stable-ID `tab.list {hidden}` and `tab.show` APIs so `invisible-dash` can list and restore tabs. Never allow the last visible tab to be hidden, and move focus to a visible neighbor when hiding the active tab.

**Alternatives considered.** Closing/archiving the tab (destructive and semantically different); removing it from `Workspace.tabs` (loses ownership and pane identity); a plugin-only record (cannot preserve host runtime state); a vector-index API (unstable across reorder/restore); a generic visibility toggle without explicit list/show methods.

**Decision and why.** Keep the tab in `Workspace.tabs` and add `Tab.hidden` plus serde-default `TabSnapshot.hidden`; filter only visible tab UI/navigation while retaining all runtime ownership. Use the existing public stable tab ID (`workspace public ID:tab number`) in API payloads, `tab.list` with optional `hidden`, and `tab.show {tab_id, focus?}`. Keep `Make invisible` host-owned in both TUI and API-backed dispatch paths because it mutates core state; keep restore in the plugin. The seams to preserve are snapshot capture/restore, active-tab visibility invariant, tab geometry/hit testing, and API schema/bridge capability checks. If upstream adds native tab hiding, migrate to its state and APIs rather than retaining parallel fork fields; if upstream changes tab identity, adapt the plugin contract at the stable-ID seam.

## ADR-002: `open_plugin_panes` and `OpenPluginPane`

**Goals.** Let a WASM UI plugin open a visible, interactive PTY pane for long-running work, with the correct plugin root, declared entrypoint, placement, and live stdout/stderr, so autonomous update steps and their fixer agent are visible. Gate the effect by an explicit manifest capability and preserve legacy action invocation for direct/API callers and older hosts.

**Alternatives considered.** Continue using `InvokePluginAction` (background detached thread and captured log, no visible PTY); hard-code version-dash panes in the host; add a capability query/fallback ABI; make the plugin draw fake progress instead of running the real command in a PTY.

**Decision and why.** Add `OpenPluginPane {entrypoint, placement}` to the fork guest ABI, `open_plugin_panes` to manifest capabilities, and host routing through the existing plugin-pane placement implementation (overlay/split/zoom/tab). Version-dash declares focused tab entrypoints and emits this effect after the same model safety gates; its old actions remain declared. Capability filtering is intentional: an old host may drop the effect rather than guess a fallback, while direct action compatibility remains. Preserve the ABI enum/serialization, capability filter, pane placement seam, entrypoint resolution, and plugin-root execution environment. If upstream gains an equivalent native pane-opening API, rebase the guest onto that API and remove the fork effect/capability; do not fork host placement further.

## ADR-003: In-process WASM UI plugin bridge

**Goals.** Provide fork-only in-process WASM UI modules for right-panel, bottom-dash, tab-surface, pane-overlay, context-menu, and pane-graphics/text surfaces; deliver host state/input; allow narrowly declared API reads/writes, plugin actions, toasts, file input, pane movement/graphics, collectors, and pane opening; support hot reload and live-handoff registry/state preservation. This backs both invisible-tab restoration and version-dash panes while remaining compatible with upstream's out-of-process plugin v1.

**Alternatives considered.** Use upstream plugin v1 only (out-of-process manifest commands, no in-process frame rendering); hard-code each dashboard into the host; let WASM access the socket/filesystem without capability declarations; use a separate host implementation for every surface; make visibility/update behavior external scripts with no host bridge.

**Decision and why.** Keep the generic `UiPluginManager`/manifest bridge, vendored `herdr-ui-plugin-api`, declared surfaces and capabilities, host-side effect filtering, `ui_input_file` state envelope, collector boundary, and plugin registry/hot-reload/handoff seams. Plugins own presentation and workflow-specific commands; the host owns tab/runtime invariants, placement, and capability enforcement. `api_read` permits `tab.list {hidden:true}` and `api_write` permits `tab.show`; `plugin_actions` and `open_plugin_panes` remain explicit rather than implicit. Preserve upstream's out-of-process v1 path instead of replacing it. If upstream supplies native in-process modules with equivalent capability isolation, rearchitect plugins to that contract and deprecate duplicate bridge code; if upstream only changes v1, adapt the bridge at manifest/registry seams rather than embedding plugin-specific logic in core.

## ADR-004: Version-dash update pipeline

**Goals.** Make “update latest” an observable, gated test-then-promote pipeline: open a visible pane for each autonomous step, stream command output, atomically publish `pipeline.json` and a per-stage log, show working/terminal state and elapsed time, require a passing receipt whose SHA matches the test binary before promotion, and never promote with failing E2E. On any merge conflict or E2E failure, invoke the FIXER AGENT in the same visible pane with logs plus this file as context.

**Alternatives considered.** Existing detached `InvokePluginAction` with in-memory 64-KiB logs; silent background shell scripts; build directly into the watched/plugin target; promote without a receipt/hash gate; automatic restart or live handoff; a tracker inferred from archived files rather than atomically written state.

**Decision and why.** Keep update scripts as the authoritative build/promotion gates, run them through manifest tab-pane entrypoints and `OpenPluginPane`, and atomically replace state in `HERDR_PLUGIN_STATE_DIR/pipeline.json` while teeing a stage log. Keep test build and HR promotion distinct: build/sanity-check the test binary, require its matching `verify-test.json`, then atomically stage/copy promotion; restart remains opt-in. Preserve `pipeline.json` field semantics consumed by the version-dash collector/model, explicit repository and instance paths, receipt/hash validation, and no-promotion-on-failure behavior. The host pipeline/fixer must use `cx` inside the pane, not an invisible subprocess. If upstream provides a native visible task runner/tracker with equivalent receipts and failure handoff, adopt it and deprecate the scripts/collector pieces; otherwise rearchitect only the custom side around upstream APIs to minimize merge conflicts.

## Fork-only changes found in the 118-commit history but not fully documented here

- Generic in-process WASM bridge and native dash migration (`d2388e5f`, `fbd20ad6`, purge commits), including goal/usage/historial/button/harness plugins and plugin lifecycle/hot reload.
- Pane graphics/image compositor and plugin-owned graphics (`88370e15`, `47964c82`, `869780c6`, `6d8a5ecd`, `6a3f7380`), plus pane text overlays and pane reorder/move effects (`57fb9bb9`, `66866971`).
- Floating popup panes and in-app resume picker (`2c7c8beb`, `364e1534`), and plugin focus/state handoff preservation (`a22454f2`, `07e6a105`, `4aaf75bc`).
- Harness-manager host collector/model, enable/disable/stash flows, plugin administration, and per-frame host-clock stamping (`d7e88f3b`, `5ca3dff6`, `488cc795`, `5e0f12b5`, `73121a55`).
- Usage/version provenance and two-instance dashboard behavior (`6caf1359`, `a6e8480f`, `2b012356`, `c03a27be`), plus fork protocol/provenance/build-marker changes.
- Existing native-dash rendering, sidebar/theme/layout, archive/history/session restore, and image/graphics test changes remain fork-owned; future entries should record their goals and upstream replacement policy before merge.
