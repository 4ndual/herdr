
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

## Promotion and the #22 E2E

### The E2E found a real gap, which is what it is for

The first promotion shipped a release binary built **before `task/pane-env-persistence`
was merged**. #36 was committed in its own worktree (`3f4e4f91`) and never merged
into `feat/host-action-bridge`, so every earlier "#36 is done" statement in this
report described a commit that was not in the branch being promoted.

Caught by running the actual scenario rather than trusting the commit: a harness
pane launched through the promoted binary with
`--env CLAUDE_CONFIG_DIR=~/harnesses/e2e-claude/config` produced a `session.json`
entry with **no `env` and no `launch_cwd`**. Merged as `80bfae97` and re-promoted.

### Two rig defects found and fixed along the way

1. **The window came back at 2005x971 on a 1600x1000 screen** — the exact
   viewport defect behind the retracted rail diagnosis, reproduced live because
   my relaunch used `--geometry=199x51`. `rail-control.ts` caught it on the first
   run with the right message. Relaunch fixed to `158x51`.
2. **`rail-control.ts` inherited `HERDR_SOCKET_PATH` from the calling shell** and
   silently reported on the **live hr instance** while claiming to describe the
   test rig — the same "measured the wrong thing" class the control exists to
   prevent. It no longer reads the ambient socket and now prints the socket and
   display it used, so every answer names its subject.

### The rail claim, settled with evidence

With a correct viewport and the new `plugin.ui.status` fields, from the live
test instance:

```
builtin.harness-manager/harness   surfaces=["RightPanel"] nodes=1
builtin.historial-dash/historial  surfaces=["RightPanel"] nodes=1
builtin.plugin-manager/plugins    surfaces=["RightPanel"] nodes=43
```

historial-dash and plugin-manager **do** render right-panel content. The
"unreachable" verdicts in the earlier gate files were the viewport artifact, as
the user's correction said. Re-verified rather than assumed.

New finding from the same read, worth a follow-up: **every module reports
`hit_rects=0`**, including plugin-manager with 43 nodes. Those panels publish no
declared click targets at all, so clicks can only reach them through the
`mouse_all` broadcast. That is a plausible root cause for several "dead click"
reports and was invisible before this fix.

### Server hygiene

The test server initially inherited `CLAUDE_CODE_CHILD_SESSION` from the
conducting session, which **disables transcript saving in every pane it spawns**
— any session-resume evidence taken from that rig would have been worthless.
Restarted from `/tmp/start-herdrtest-clean.sh`, which strips `CLAUDE*`/`HERDR_*`
before exec. Verified: zero `CLAUDE*` vars in the server's `/proc/<pid>/environ`.

### Four-axis parity (final state, re-verified after the #36 merge)

| Axis | Test instance | Live `hr` |
|---|---|---|
| 1. Binary identity | PASS | PASS (same sha256 staged) |
| 2. Running-process identity | PASS — verified by HASHING `/proc/<pid>/exe`, not just reading its path | **PENDING A RESTART** |
| 3. Plugin-registry parity | drift, see below | drift, see below |
| 4. Config parity | PASS — `[theme]`/`[ui]` blocks identical | PASS |

Final binary: `30b1907d6fd8fccbe50a500f23eaa92dfd382516f42c38264e6ef5e08004e5f6`,
identical across `~/.herdr-hr/bin/herdr`, `~/.local/bin/herdrtest` and
`target/release/herdr`. The test server's `/proc/<pid>/exe` hashes to exactly
that. `hr`'s still reads `(deleted)` — the previous inode, held open.

**Axis 2 on `hr` is deliberately not done.** This batch is running *inside* the
`hr` session (`HERDR_PANE_ID=w1C:p2`), so restarting that server would kill the
batch mid-flight, and the plan's own guidance is that restarting `hr` is the
hazard. The new binary is staged at `~/.herdr-hr/bin/herdr`; the running process
still holds the previous inode and will pick the new one up on the next restart
you perform.

**Axis 3 has real drift**, beyond the three documented singleton exclusions
(herdr-mirror, collie, harness-control):

- enabled on test but not on hr: `builtin.claude-image-preview`,
  `builtin.harness-panel`, `builtin.image-view-go`
- `probe.rail` (rooted at `/tmp/rail-probe`) — **removed.** Unlinked from the
  test instance and its directory deleted. A throwaway diagnostic from the
  retracted rail investigation, so removing it needed no decision from you.
- `claude-image-preview` is linked from *different roots* on the two instances
  (`~/Projects/herdr-plugins/...` on hr, `~/Projects/herdr/plugins/...` on test)

I have not enabled plugins on the live instance or deleted the probe: both are
user-visible changes to a running environment, and the root divergence is a
question about which copy is canonical rather than a mechanical sync. Listed
here for your decision.

## What is NOT done, and why

Reported plainly rather than left to be discovered:

- **`hr` axis-2 restart** — see above. Binary staged, restart is yours.
- **Plugin-registry drift** — three plugins and one throwaway probe, listed
  above. Needs your call on the canonical root for `claude-image-preview`.
- **#21 collied / herdr-mirror update** — not attempted. 18 live mirror
  processes bridge hr↔Mac and the plan's own hazard note is that the danger is
  restarting hr, which this session cannot do from inside itself.
- **#22 Mac rollout** — gated on Linux being fully green, which requires the
  `hr` restart above. The macOS-specific fix in this batch (the runtime-swap
  `ps -Eww` fallback) is therefore **unverified on real macOS**.
- **#39 QA defect sweep** — the plan's ordered task table does not contain a
  #39 entry, so there was no specification to work to. The open FAILs in the
  gate files fall into three classes: tick-shaped focus lag (which #25's
  event-driven focus fix should have closed — needs re-measurement after the
  `hr` restart), the mouse-not-forwarded-into-plugin-overlay-panes host gap
  (filmed on three overlays, still open), and the rail/viewport verdicts
  (retracted above).
- **#34 phase 2** — **shipped.** I originally skipped it on a false premise:
  I read §7's open questions (default "no") and missed the
  `USER ANSWERS (2026-08-13 ~18:00)` section ~45 lines further down in the same
  file, which explicitly overrides the listed defaults and answers both
  questions "yes". Corrected and delivered; see the section below.
- **#26 / #24** — design documents only, as the plan directs.

## #22 — the E2E, proven

Recording: `.local/qa/batch-e2e.mp4` (827.7 s, burned clock), gates in
`.local/qa/batch-e2e-gates.md`, timeline in `.local/qa/batch-e2e.timeline`.

The plan's success criterion for #36 is that harness panes restore **without**
`CLAUDE_CONFIG_DIR`/`CODEX_HOME` exported into the server. Both halves were
proven on the promoted binary, with the server's `/proc/<pid>/environ` verified
to contain neither variable:

**Capture.** A pane launched with
`agent start --env CLAUDE_CONFIG_DIR=~/harnesses/e2e-claude/config` produced:

```json
{ "cwd": "/home/andual/harnesses/e2e-claude/workspace",
  "agent_name": "e2e36b",
  "env": { "CLAUDE_CONFIG_DIR": "/home/andual/harnesses/e2e-claude/config" },
  "launch_cwd": "/home/andual/harnesses/e2e-claude/workspace",
  "launch_argv": ["claude", "--dangerously-skip-permissions"] }
```

**Restore.** After a full server stop/start, the restored pane answered
`POST=/home/andual/harnesses/e2e-claude/config`.

### Why the proof uses a shell probe rather than a claude resume

The first attempt resumed a real claude session and got *No conversation found
with session ID*. That looks like #36 failing, and it is worth being precise
about why it is not: **no transcript existed in any root**. The pane's claude had
been started while the server still carried an inherited
`CLAUDE_CODE_CHILD_SESSION`, which disables transcript saving — so herdr
recorded a session id for a conversation that was never written to disk. A
resume of a non-existent transcript fails whichever config root it looks in, so
that test could not distinguish the two causes.

The shell probe (`echo POST=$CLAUDE_CONFIG_DIR` in a restored pane) isolates
exactly what #36 changes — whether a pane's launch env survives the restart —
with no dependency on claude's transcript behaviour. That is the evidence above.

## #34 phase 2 — persisted colors and `pane.set_color`

Shipped after the correction. Two halves, both answering the same requirement:
a colour is a property of the pane, not a decoration that lives until the next
restart.

**Persistence.** `PaneSnapshot` gains a `color` field, captured from the
terminal's reserved `color` token and re-applied on restore with no TTL. Only
that one key is persisted — the rest of `metadata_tokens` is TTL'd status text
written by plugins that rewrite it within seconds, and persisting a TTL'd value
would resurrect stale status across a restart. `skip_serializing_if` keeps an
uncoloured pane's snapshot byte-identical, so existing session files do not
churn, and an old snapshot with no `color` key still loads.

**`pane.set_color`, behind a new `Metadata` capability.** The method validates
through the same `validate_metadata_color` the phase-1 handler uses, so an
invalid value fails the request with the previous colour intact rather than
being accepted and ignored at render time. Clearing is always legal — a pane
coloured by a since-uninstalled plugin has to be returnable to the default.

The capability is deliberately narrow. `pane.report_metadata` stays
host-reserved and `Metadata` grants exactly one method, pinned by a test that
asserts `pane.report_metadata`, `pane.report_agent` and
`pane.report_agent_session` all still deny. Colouring a pane is a preference;
claiming its agent or session identity is an authority claim, and a plugin that
wants the first must not acquire the second as a side effect.

**Protocol version: not bumped, deliberately.** Source is at 19; stable
publishes 17 and preview publishes 18, so protocol 19 has never been published
in either channel. Per the repo rule, a bump is due only when the *published*
protocol changes incompatibly — and adding a method is additive regardless.

Docs updated for plugin authors (`plugins.mdx`) and API callers
(`socket-api.mdx`), including the "prefer a role over a literal" guidance:
roles resolve against the live palette at render time, so a coloured pane
follows a theme change instead of freezing one colour that looks wrong in every
other theme.

### A note on the operator's instance

The `hr` restart remains for you. The staged binary is
`30b1907d…` (now superseded — see below); the running server still holds the
previous inode. I have not restarted it and will not: your orchestrator session
lives in that instance.
