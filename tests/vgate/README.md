# vgate — video quality gate for `tidus-bar-ui`

An automated gate that drives a real herdr session through a scripted fleet
scenario, records the screen, and has **Mage-VL judge the recording** against a
bar-specific checklist. It returns `VERDICT: ACCEPT` or `VERDICT: REJECT` with
clues, and exits non-zero on REJECT so it can block "done".

Built on the [`vl-gate`](~/.claude/skills/vl-gate/SKILL.md) skill (doctrine,
question phrasing, payload discipline) and the recording rig in
`harness-fleet/e2e/` (burned watermarks via `-filter_script:v`, the
VAAPI-or-x264 policy from `encode-pick.sh`).

## Run it

```bash
bun tests/vgate/vgate.ts run                    # stage → record → drive → judge
bun tests/vgate/vgate.ts stage                  # bring the isolated stage up only
bun tests/vgate/vgate.ts judge <mp4> <claims>   # re-judge an existing recording
bun tests/vgate/vgate.ts down                   # tear the stage down
```

Exit `0` = ACCEPT, `1` = REJECT. Wire it as the last step of a tidus-bar task
and let the non-zero exit stop the work being called finished.

| env | default | |
|---|---|---|
| `VGATE_DISPLAY` | `:21` | private Xvfb for the recording |
| `VGATE_SESSION` | `tbvgate` | herdr session name |
| `VGATE_FLEET` | `/tmp/vgate-fleet` | private `HARNESS_ROOT` |
| `VGATE_OUT` | `/tmp/vgate` | video, claims, frames, report |
| `VGATE_HOLD` | `9000` | ms each phase is held on camera |
| `VGATE_KEEP` | unset | leave the stage up after the run |
| `VL_ENDPOINT` / `VL_MODEL` | llama-swap / `mage-vl` | the reviewer's only vision |

Requires: `bun`, `ffmpeg`, `xdotool`, `Xvfb`, `xfce4-terminal`, and Mage-VL
served by llama-swap on `:8090` (~6 GiB VRAM; it auto-loads on first request).

## What it drives

An **isolated** stage — its own Xvfb, its own herdr session (the `herdr-test`
XDG tree), and its own `HARNESS_ROOT`. It never touches `~/harnesses` or the
live `hr` session, because that session belongs to a human and other agents are
editing this plugin right now.

The fleet is fixtures; the **session, the panes and the clicks are real**. Each
of six instances gets a real herdr tab carrying `HARNESS_NAME`, which is the
collector's strongest attribution rule and what gives every chip a `tab_id` —
a team edge with no pane renders nothing at all, so without real panes every
click criterion would be vacuous. Fixtures rather than `harness spawn` because
a spawn costs real model calls per instance and the bar renders the spawn
*graph*, which a set of `harness.json` files is.

```
vg-root
├── vg-alpha        brothers: vg-beta
│   ├── vg-a1       brothers: vg-a2
│   └── vg-a2
└── vg-beta
    └── vg-b1
```

Seven recorded phases: `root-selected`, `alpha-selected`, `a1-selected`,
`click-brother`, `click-ancestor`, `right-click`, `close-child`. Selection setup
uses `tab focus` (not itself under test); the click criteria use **real xdotool
presses** at the chip's own pixels — an interaction that is not visibly
performed does not count.

Chips are located by **ink-run matching** against the frame rather than by
reimplementing `render::layout()`: that arithmetic is being actively rewritten,
and a copy here would drift silently. A chip name is drawn as one contiguous
glyph run of `len(name)` cells, so the run whose width matches, on the band
whose run sequence matches that row's expected names, is the chip. This also
means the gate clicks **what is actually drawn**, which is the point of a video
gate.

## Checklist

`checklist.ts` is the source of truth. All eleven are REJECT-on-fail; `core`
means unreadable video evidence also rejects, because a gate that passes what it
cannot see is not a gate.

| id | criterion | core |
|---|---|---|
| C1 | entries show instance names, not `harness:jobid` | yes |
| C2 | selected tab carries the visual indicator, immediately after its parent | yes |
| C3 | children row holds the **selected** tab's children (not the ancestor's) | yes |
| C4 | children row is **complete** vs the herdr pane list | yes |
| C5 | brothers stay visible through every selection change | yes |
| C6 | full ancestor lineage at depth — root *and* every intermediate | yes |
| C7 | recursion holds one level deeper at minimum | yes |
| C8 | clicking an entry focuses **that instance's own pane**, matched by id | yes |
| C9 | right-click reports instead of navigating | no |
| C10 | closing a child returns the selection to its parent | no |
| C11 | no flicker/incoherence; clock monotonic; bar never blank or garbled | yes |

## How judging works

Two evidence channels, deliberately not interchangeable:

* **video** — a Mage-VL answer about a named extracted frame. This is what is
  judged. Nothing in the report may assert "I saw the video".
* **claims** — `claims.jsonl`: the collector snapshot *and* the herdr pane list
  captured at each phase. These are claims to cross-check, never truth. They
  make a video failure legible ("the bar drew 2 children, the pane list says 3")
  and supply the ground truth C4 needs, which a frame alone cannot establish.

C8 is checked at instance granularity: the clicked chip's own `tab_id`/`pane_id`
/`terminal_id` versus the focused pane's after the click. Landing on another
pane of the same-named harness is a FAIL, not a pass.

Questions are asked against a **crop** of the bar band, upscaled 2×. Image
prefill is where the GPU time goes and a crop is ~10–20× cheaper than a full
1080p frame; the upscale is what makes chip text legible at all. VL calls are
retried, because llama-swap returns HTTP 500 while the model cold-loads and an
errored answer treated as "nothing wrong" is how a vision gate silently stops
seeing.

Output lands in `$VGATE_OUT/judge/`: `gate-report.md`, `gate-report.json`, and
every frame that was asked about.

## Scope

The gate is a **reviewer only**. It never edits the plugin; a REJECT loops back
to whoever implements the bar. It is deliberately not in
`plugins/tidus-bar-ui/` — that tree is under active rework by other agents, and
the gate must be runnable against whatever is currently deployed without
colliding with them.
