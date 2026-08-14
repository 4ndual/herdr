# tidus-bar-ui

A bottom bar of agent chips: the **ancestor chain of the focused orchestrator
pinned left**, then that orchestrator's **direct spawned children**. Clicking a
chip swaps the terminal area to that agent; clicking the pinned parent swaps
back.

```text
⑂ ▌tb-root▐ │ ◆ tb-ask ○ tb-kid1 ○ tb-kid2        ← at tb-root: its two children (+ one needing a human)
⑂ ● tb-root ▸ ▌tb-kid1▐ │ ○ tb-gkid               ← at a nested child: ancestors, then ITS children
⑂ ● tb-root ▸ ● tb-kid1 ▸ ▌tb-gkid▐ │ no children ← never uncles: tb-kid2 is absent here
```

Why it exists: herdr's tab bar is a flat list in creation order. It cannot say
who spawned whom, and once tidus children launch hidden it will not show them at
all. This bar renders the fleet's **spawn graph**, rooted at whatever you are
looking at right now.

## What counts as a child

Parentage is a fleet fact; the plugin invents nothing and needed no
harness-fleet change. Three edges, strongest first:

| source | edge | meaning |
| --- | --- | --- |
| `~/harnesses/<child>/state/dispatch.json` | `dispatch` | a routed task is in flight right now |
| `~/harnesses/<child>/harness.json → team` | `team` | declared, persistent affiliation |
| `~/harnesses/<child>/state/last-result.json` | `history` | the most recent job came from this parent |

`dispatch.json` is deleted the instant a task completes, so it alone would make
children blink off the bar between jobs. Children must stay **attached at all
times**, so `team` and the last job keep the chip present and the *dot*, not the
chip's presence, carries "done" / "stopped". Live work sorts first, then team,
then history.

A **profile** (`profile: true` — an external config dir such as `~/.claude`
registered as a fleet member) can be an orchestrator but is never a child:
nobody spawns it.

## Status dots

Fleet truth (`statusOf`, mirrored exactly so the bar can never contradict
`harness list`) merged with herdr's pane truth, which wins whenever herdr can
see the terminal:

| dot | state | source |
| --- | --- | --- |
| ◆ | attention | herdr says `blocked` — a child sitting on an interactive prompt |
| ● | busy | herdr `working`, or a live dispatch |
| ○ | idle | herdr `idle`/`done`, or the `state/idle` marker with a live supervisor |
| ◔ | starting | a spawn lock younger than 30 s |
| ↻ | refresh-queued | `state/refresh-requested` |
| ✓ ✗ | done / failed | the outcome in `last-result.json` |
| · | stopped | no supervisor — a reaped child, still attached |

**Attention is the point.** A child blocked on a prompt is the silent-stall
failure mode; it renders bold and first-class rather than as one more grey dot.

herdr's `done` means *this turn finished*, not *this job finished*, so it maps to
**idle** — mapping it to "done" greyed out running orchestrators and disagreed
with `harness list`.

## Clicking

* **Left-click** — swap the terminal area to that agent: `tab.focus`, or
  `tab.show {focus: true}` once children launch hidden (both are `api_write`
  methods). The same gesture on the pinned parent swaps back.
* **Left-click a chip with no PTY** — a `light` child running inside `conductor`,
  or one the reaper took — reports instead of navigating. There is nothing to
  swap to, and a dead click target is worse than none.
* **Right-click** — report: name, status, edge, job tail, task, and the evidence
  behind the attribution. A surface you glance at should not carry a destructive
  second gesture.

The bar never steals focus on its own. It moves you only when you click.

## Which orchestrator am I looking at?

Strongest evidence first; a failed lookup is never upgraded to a guess:

1. `HARNESS_NAME` in the runtime process's environment — stamped by
   `harness spawn`. Definitive.
2. `CLAUDE_CONFIG_DIR` / `CODEX_HOME` / `JCODE_HOME` → the config dir the process
   is really reading → its harness **or profile**. This is what attributes the
   human's own session, which is normally a profile and normally the orchestrator.
   The pane's shell is scanned too: a tab created with `--env` carries the config
   dir there, and the runtime is not always a foreground process.
3. The focused session id against each profile's `state/session-id`.
4. `state/herdr-pane` — last, because only `harness spawn` writes it and it goes
   stale when a pane is replaced.

Every tab id is validated against **this** herdr instance before it becomes a
click target; `state/herdr-tab` is written by whichever herdr opened the harness,
so an unvalidated id would focus a stranger's tab. The pane behind a chip is
resolved from the validated tab via `agent.list`, not from `state/herdr-pane` —
that file only exists for `harness spawn` children, and trusting it reported
every hook-registered child as "stopped" while it sat on a permission prompt.

## Architecture

`collector/` (native, host-tick) → `tidus_bar_input.json` → `model/` (shared,
wasm-safe) → `src/` (wasm guest: one row, one hit rect per chip). Self-contained
cargo workspace, so `cargo build` here produces both artefacts and the root
`Cargo.toml` needs no member entry; `hot_reload = "on"` swaps either in on the
next tick.

No throttle, on purpose: the bar describes the tree you are looking at right
now, so staleness after a focus change is the one failure that would make it
worse than nothing. It earns that by being cheap — one scan of `~/harnesses/*`,
a few `/proc` reads and three local socket calls, **5–20 ms measured**, written
into every snapshot as `collect_us` so the claim is checkable from the artefact.

Linux only, honestly: attribution reads `/proc/<pid>/environ`. macOS needs
`KERN_PROCARGS2` first, and claiming support early would ship a permanently
empty bar.

## Phase 2

Children do not launch hidden yet — `tab.hide` and `tab.create {hidden}` do not
exist in the host, and the sidebar does not filter hidden tabs. Every chip
carries a `hidden` flag that is always `false` today, and the guest already
picks `tab.show` when it is set, so phase 2 is a collector change rather than a
protocol change. Full delta, including what tidus must call at spawn time:
`scratchpad/plan/tidus-bar-phase2.md`.

## Tests

`cargo test` in this directory: guest + model + collector, all passing;
`cargo clippy --all-targets` clean. E2E gates and video:
`.local/qa/tidus-bar-gates.md`, driven by `.local/qa/tools/tb-scenario.ts`.
