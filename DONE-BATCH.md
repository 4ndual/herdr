# DONE-BATCH — overnight build batch, 2026-08-14

Branch `feat/host-action-bridge`, 15 commits. Full detail and evidence in
`BATCH-REPORT.md`; this is the per-task summary.

Read the three "not done" rows first — they are the ones that need you.

## Per task

| Task | State | Commit | Evidence |
|---|---|---|---|
| G0 parity gate | done | — | recorded in `BATCH-REPORT.md` |
| #36 env + cwd persistence | **done and proven live** | `3f4e4f91`, merged `80bfae97` | capture: `session.json` carries `env`+`launch_cwd`; restore: pane answers `POST=…/harnesses/e2e-claude/config` after a full restart, server env free of the workaround |
| 2a `fork_argv` catch-all | done | `c6ea4368` | unknown runtime returns `None` instead of a silent bare resume |
| 2b conditional Fork row | done | `2585602f` | new test asserts the row is absent where forking cannot work |
| 2c auto-continue predicate | done | `affdda47` | codex/pi/jcode now nudged; unsupported runtimes are not typed into |
| Observability fix | done | `dd791e10` | live: `rendered_surfaces`/`nodes`/`hit_rects`/`wasm_memory_bytes` all reporting from the test instance |
| Rail control rig | done | `dd791e10` | caught a 2005x971 window on a 1600x1000 screen on its first real run |
| #32 `rail_icon` | done | `844b8537` | two bundled manifests had already declared the field against a host that ignored it |
| #25 usage strip | done | `2f75d11d`, `41dbfef0` | per-pane keying + in-flight correlation; the focus event was then found never to fire on the server+client path at all — fixing that took harness-indicator from 562 ms to 74 ms |
| #34 colors phase 1 | done | `183c4a2c` | strict parser, handler-side validation, one border resolver |
| #34 phase 2 | **done, verified live** | `e5e1eaea` + follow-up | persisted `color` in `PaneSnapshot`, `pane.set_color` behind the new `Metadata` capability, `herdr pane set-color` CLI; colour survived a real server restart on the rig |
| #37 invisible API | done | `183c4a2c` | `tab.hide`, `tab.create --hidden`, `tab list --hidden`, sidebar filtered, cycling fixed |
| #19 roster scope | done | `183c4a2c` | composes with a plugin `agent_view` instead of being disabled by one |
| #18 code half | done | `183c4a2c`, `4bdabb1d` | B1/B2 closed; carve-release-on-disable finally has a test |
| #26 | design only, as planned | `4bdabb1d` | `.local/prd/26-runtime-parity-design.md` |
| #24 | design only, as planned | `4bdabb1d` | `.local/prd/24-lower-bar-feasibility.md` |
| #42 / #20 portability | done | `4bdabb1d` | multi-root claude lookup verified live; macOS env fallback added |
| #39 QA defects | **partial** | — | no #39 entry exists in the plan's task table; open FAILs triaged in `BATCH-REPORT.md` |
| #21 collied / mirror | **not done** | — | needs an `hr` restart this session cannot perform from inside itself |
| #22 E2E + video | done (Linux) | — | `.local/qa/batch-e2e.mp4`, both #36 halves proven |
| #22 Mac rollout | **not done** | — | gated on Linux being fully green, which needs the `hr` restart |

## What needs you

1. **Restart `hr`.** The new binary is staged at `~/.herdr-hr/bin/herdr`
   (sha256 `30b1907d…`), but the running server still holds the previous inode.
   This batch runs *inside* the `hr` session, so restarting it would have killed
   the batch — and the plan's own note is that restarting `hr` is the hazard.
   Everything else on that instance is already in place.

2. **Decide the plugin-registry drift.** Enabled on the test instance but not on
   `hr`: `claude-image-preview`, `harness-panel`, `image-view-go`. And
   `claude-image-preview` is linked from *different roots* on the two instances
   (`~/Projects/herdr-plugins/…` vs `~/Projects/herdr/plugins/…`) — that is a
   question about which copy is canonical, not a mechanical sync, so I left it
   alone. (`probe.rail`, the throwaway from the retracted rail investigation, I
   did remove.)

3. **A follow-up the observability fix surfaced.** Every UI module reports
   `hit_rects=0` — including plugin-manager, which renders 43 nodes. Those
   panels declare no click targets at all, so clicks reach them only through the
   `mouse_all` broadcast. That is a plausible root cause for several of the
   "dead click" reports in the gate files and was invisible before tonight.

## Corrections to earlier claims in this batch

- **I skipped #34 phase 2 on a false premise.** I read §7's open questions
  (default "no") and missed the `USER ANSWERS (2026-08-13 ~18:00)` section ~45
  lines later in the same file, which explicitly overrides those defaults and
  answers both "yes". You caught it; phase 2 is now shipped in `e5e1eaea`.
- **The focus event never fired on the path that renders `hr`.**
  `sync_ui_plugin_focus_event` was wired only into the direct-TUI render loop,
  which a server+client herdr never runs — so #25's per-pane keying was correct
  but could not help. Found by re-measuring rather than assuming the fix worked.

- **"#36 done" was wrong until `80bfae97`.** It was committed in its own
  worktree and never merged into this branch. The E2E caught it — a pane
  launched through the promoted binary produced a snapshot with no `env` at all.
  Every earlier statement in this report that #36 had landed described a commit
  that was not in the branch being promoted.
- **The rail was never broken.** Confirmed with a correct viewport and the new
  status fields: harness-manager, historial-dash and plugin-manager all render
  `RightPanel` content. The earlier "unreachable" verdicts were the 2002px
  window, exactly as you said.
- **`rail-control.ts` reported on the wrong instance on its first run**,
  inheriting `HERDR_SOCKET_PATH` from the calling shell — the same
  measured-the-wrong-thing failure it exists to prevent. Fixed to ignore the
  ambient socket and to print the socket and display it used.

## Validation

`cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean after
every task. Final suite: **3544 tests, 3533 passed, 11 failed** — all 11 in the
pre-existing set verified against the base commit (`api_ping` ×4 and
`auto_detect` ×3 are socket-contention flakes under parallel load and pass when
run alone; plus `cli::sessions` legacy restore, `client_mode` cwd fallback,
`multi_client` sigkill, and `read_ui_input_blob`). No new failures.
