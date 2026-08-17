# vl-gate report — tidus-bar-ui

VERDICT: REJECT

Video: `/tmp/vgate/tidus-bar-vgate.mp4` (80.9s). Reviewer vision: Mage-VL only (mage-vl @ http://127.0.0.1:8090/v1/chat/completions).
16 vision queries. Claims cross-checked from `/tmp/vgate/claims.jsonl`.

| criterion | verdict | blocking |
|---|---|---|
| C11-coherence No flicker, no incoherence, no garbled or empty bar | PASS | yes (core) |
| C1-names Entries show instance names, not harness:jobid | FAIL | yes (core) |
| C2-selected-indicator Selected tab carries the visual indicator and sits immediately after its parent | FAIL | yes (core) |
| C3-children-under-selected Children row renders under the SELECTED tab and holds the SELECTED tab's children | FAIL | yes (core) |
| C4-children-complete Children row is COMPLETE against the herdr pane list | FAIL | yes (core) |
| C5-brothers-stay Brothers remain visible through every selection change | FAIL | yes (core) |
| C6-lineage-complete Full ancestor lineage visible at depth, root and every intermediate | PASS | yes (core) |
| C7-recursion-depth Recursion holds one level deeper at minimum | FAIL | yes (core) |
| C8-click-navigates-to-instance Clicking an entry focuses THAT INSTANCE's actual pane (matched by session id) | UNVERIFIABLE | yes (core) |
| C9-context-menu Right-click reports (context menu) instead of navigating | UNVERIFIABLE | yes |
| C10-close-returns-to-parent Closing a child returns the selection to its parent | FAIL | yes |

## C11-coherence — No flicker, no incoherence, no garbled or empty bar

**PASS**

Evidence:

- t=12.1s → UNREADABLE
- t=24.3s → 1) No, there is no row of small agent/tab labels visible in this strip. 2) UNREADABLE 3) No, there is nothing garbled, overlapping, blank, or an error dialog covering it.
- t=36.4s → 1) Yes, there is a row of small agent/tab labels visible in this strip. 2) andual@andual:/tmp/vgate-fleet/vg-al/workspace, andual@andual:/tmp/vgate-fleet/vg-alpha/workspace, andual@andual:/tmp/vgate-fleet/vg-a2/workspace
- t=48.5s → UNREADABLE
- t=60.7s → 1) No, there is no row of small agent/tab labels visible in this strip. 2) UNREADABLE 3) No, nothing is garbled, overlapping, blank, or is an error dialog covering it.
- t=72.8s → UNREADABLE
- t=16.2s → UNREADABLE
- t=40.4s → UNREADABLE
- t=64.7s → UNREADABLE

## C1-names — Entries show instance names, not harness:jobid

**FAIL**

Evidence:

- root-selected (/tmp/vgate/judge-final/root-selected.jpg) → hr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 ∧[andal@andal: /tmp/vgate-fleet/vg-root/workspace] · andal@andal: /tmp/vgate-fleet/vg-alpha/workspace · andal@andal: /tmp/vgate-fleet/vg-beta/workspace harness shell · /tmp/vgate-fleet/vg-root/workspace - no focused agent session -
- alpha-selected (/tmp/vgate/judge-final/alpha-selected.jpg) → nr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 + andual@andual: /tmp/vgate-fleet/vg-root/workspace [ andual@andual: /tmp/vgate-fleet/vg-alpha/workspace ] · andual@andual: /tmp/vgate-fleet/vg-beta/workspace · andual@andual: /tmp/vgate-fleet/vg-al/workspace · andual@andual: /tmp/vgate-fleet/vg-a2/workspace harness shell · /tmp/vgate-fleet/vg-
- a1-selected (/tmp/vgate/judge-final/a1-selected.jpg) → hr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-fleet/vg-root/workspace · andual@andual: /tmp/vgate-fleet/vg-alpha/workspace [andual@andual: /tmp/vgate-fleet/vg-al/workspace] · andual@andual: /tmp/vgate-fleet/vg-a2/workspace harness shell · /tmp/vgate-fleet/vg-al/workspace - no focused agent session -

Clues:

- root-selected: collector emits display_name ≠ name for 3 chip(s), e.g. vg-root → "andual@andual: /tmp/vgate-fleet/vg-root/workspace"
- root-selected: entries are not instance names — e.g. "∧[andal@andal: /tmp/vgate-fleet/vg-root/workspace]"
- alpha-selected: collector emits display_name ≠ name for 5 chip(s), e.g. vg-root → "andual@andual: /tmp/vgate-fleet/vg-root/workspace"
- alpha-selected: entries are not instance names — e.g. "+ andual@andual: /tmp/vgate-fleet/vg-root/workspace"
- a1-selected: collector emits display_name ≠ name for 4 chip(s), e.g. vg-root → "andual@andual: /tmp/vgate-fleet/vg-root/workspace"
- a1-selected: entries are not instance names — e.g. "· andual@andual: /tmp/vgate-fleet/vg-root/workspace · andual@andual: /tmp/vgate-"

## C2-selected-indicator — Selected tab carries the visual indicator and sits immediately after its parent

**FAIL**

Evidence:

- root-selected: expected marked=vg-root → ∧[andal@andal: /tmp/vgate-fleet/vg-root/workspace]
- alpha-selected: expected marked=vg-alpha → [ andual@andual: /tmp/vgate-fleet/vg-alpha/workspace ]
- a1-selected: expected marked=vg-a1 → [andual@andual: /tmp/vgate-fleet/vg-al/workspace]

Clues:

- a1-selected (/tmp/vgate/judge-final/a1-selected.jpg): no line reported vg-a1 as bracketed/boxed/highlighted — VL said "hr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-fleet/vg-root/workspace · andual@andual: /tmp/vgate-fleet/vg-alpha/workspace [andual@an"

## C3-children-under-selected — Children row renders under the SELECTED tab and holds the SELECTED tab's children

**FAIL**

Evidence:

- root-selected: expected child row [vg-alpha, vg-beta] → · andal@andal: /tmp/vgate-fleet/vg-alpha/workspace · andal@andal: /tmp/vgate-fleet/vg-beta/workspace
- alpha-selected: expected child row [vg-a1, vg-a2] → · andual@andual: /tmp/vgate-fleet/vg-al/workspace · andual@andual: /tmp/vgate-fleet/vg-a2/workspace

Clues:

- alpha-selected (/tmp/vgate/judge-final/alpha-selected.jpg): child row should be [vg-a1,vg-a2] — read "· andual@andual: /tmp/vgate-fleet/vg-al/workspace · andual@andual: /tmp/vgate-fleet/vg-a2/workspace" (missing vg-a1)
- CLAIM CROSS-CHECK: the collector's own snapshot DOES satisfy this at [root-selected, alpha-selected, right-click]. The failure above is that the VIDEO could not evidence it — which on this build follows from C1: entries are ~50-character pane-title paths, so a frame transcription truncates before the row is read. Fix the labels and re-run before treating this as a tree-model defect.

## C4-children-complete — Children row is COMPLETE against the herdr pane list

**FAIL**

Evidence:

- root-selected: pane-list ground truth = [vg-alpha,vg-beta]; collector claims child row = [vg-alpha,vg-beta]
- root-selected video read (child row) → · andal@andal: /tmp/vgate-fleet/vg-alpha/workspace · andal@andal: /tmp/vgate-fleet/vg-beta/workspace
- alpha-selected: pane-list ground truth = [vg-a1,vg-a2]; collector claims child row = [vg-a1,vg-a2]
- alpha-selected video read (child row) → · andual@andual: /tmp/vgate-fleet/vg-al/workspace · andual@andual: /tmp/vgate-fleet/vg-a2/workspace

Clues:

- alpha-selected (/tmp/vgate/judge-final/alpha-selected.jpg): children present as panes but NOT read on the child row: vg-a1
- CLAIM CROSS-CHECK: the collector's own snapshot DOES satisfy this at [root-selected, alpha-selected, right-click, close-child]. The failure above is that the VIDEO could not evidence it — which on this build follows from C1: entries are ~50-character pane-title paths, so a frame transcription truncates before the row is read. Fix the labels and re-run before treating this as a tree-model defect.

## C5-brothers-stay — Brothers remain visible through every selection change

**FAIL**

Evidence:

- alpha-selected: brother vg-beta must be on screen → nr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 + andual@andual: /tmp/vgate-fleet/vg-root/workspace [ andual@andual: /tmp/vgate-fleet/vg-alpha/workspace ] · andual@andual: /tmp/vgate-fleet/vg-beta/worksp
- a1-selected: brother vg-a2 must be on screen → hr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-fleet/vg-root/workspace · andual@andual: /tmp/vgate-fleet/vg-alpha/workspace [andual@andual: /tmp/vgate-fleet/vg-al/workspace] 
- click-brother: brother vg-a1 must be on screen → hr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-fleet/vg-root/workspace · andual@andual: /tmp/vgate-fleet/vg-alpha/workspace [andual@andual: /tmp/vgate-fleet/vg-al/workspace] 

Clues:

- click-brother (/tmp/vgate/judge-final/click-brother.jpg): brother vg-a1 was NOT read anywhere on the bar
- CLAIM CROSS-CHECK: the collector's own snapshot DOES satisfy this at [alpha-selected, a1-selected, click-brother, click-ancestor, right-click, close-child]. The failure above is that the VIDEO could not evidence it — which on this build follows from C1: entries are ~50-character pane-title paths, so a frame transcription truncates before the row is read. Fix the labels and re-run before treating this as a tree-model defect.

## C6-lineage-complete — Full ancestor lineage visible at depth, root and every intermediate

**PASS**

Evidence:

- a1-selected (/tmp/vgate/judge-final/a1-selected.jpg) → hr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-fleet/vg-root/workspace · andual@andual: /tmp/vgate-fleet/vg-alpha/workspace [andual@andual: /tmp/vgate-fleet/vg-al/workspace] · andual@andual: /tmp/vgate-fleet/vg-a2/workspace harness shell · /tmp/vgate-fleet/vg-al/workspace - no focused agent se

## C7-recursion-depth — Recursion holds one level deeper at minimum

**FAIL**

Evidence:

- a1-selected (/tmp/vgate/judge-final/a1-selected.jpg) → hr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-fleet/vg-root/workspace · andual@andual: /tmp/vgate-fleet/vg-alpha/workspace [andual@andual: /tmp/vgate-fleet/vg-al/workspace] · andual@andual: /tmp/vgate-fleet/vg-a2/workspace harness shell · /tmp/vgate-fleet/vg-al/workspace - no focused agent se

Clues:

- with vg-a1 selected at depth 2, these were not read on the bar: vg-a1
- CLAIM CROSS-CHECK: the collector's own snapshot DOES satisfy this at [a1-selected, click-brother, click-ancestor]. The failure above is that the VIDEO could not evidence it — which on this build follows from C1: entries are ~50-character pane-title paths, so a frame transcription truncates before the row is read. Fix the labels and re-run before treating this as a tree-model defect.

## C8-click-navigates-to-instance — Clicking an entry focuses THAT INSTANCE's actual pane (matched by session id)

**UNVERIFIABLE**

Evidence:

- click-brother: clicked vg-a2 at (1473,961); want tab=w1:t6 pane=w1:p6 term=term_659439e58bf246; got tab=w1:t5 pane=w1:p5 term=term_659439e5892ab5
- click-brother bar after click → hr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-fleet/vg-root/workspace · andual@andual: /tmp/vgate-fleet/vg-alpha/workspace [andual@andual: /tmp/vgate-fleet/vg-al/workspace] 
- click-ancestor: clicked vg-root at (801,970); want tab=w1:t2 pane=w1:p2 term=term_659439e5812812; got tab=w1:t5 pane=w1:p5 term=term_659439e5892ab5
- click-ancestor bar after click → hr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-fleet/vg-root/workspace · andual@andual: /tmp/vgate-fleet/vg-alpha/workspace [andual@andual: /tmp/vgate-fleet/vg-al/workspace] 

Clues:

- click-brother: click at (1473,961) changed nothing — focus stayed on w1:t5. INSTRUMENT-LIMITED: a click that moves nothing cannot be told apart from a click that missed the bar, so this is NOT reported as a navigation defect. Confirm the target row before trusting it.
- click-ancestor: click at (801,970) changed nothing — focus stayed on w1:t5. INSTRUMENT-LIMITED: a click that moves nothing cannot be told apart from a click that missed the bar, so this is NOT reported as a navigation defect. Confirm the target row before trusting it.

## C9-context-menu — Right-click reports (context menu) instead of navigating

**UNVERIFIABLE**

Evidence:

- right-click claim → {"kind":"right-click","target":{"x":801,"y":961,"ambiguous":false,"clusters":2},"before":{"tab_id":"w1:t3","pane_id":"w1:p3","terminal_id":"term_659439e583d6d3"},"after":{"tab_id":"w1:t3","pane_id":"w1:p3","terminal_id":"term_659439e583d6d3
- bar after right-click (/tmp/vgate/judge-final/right-click.jpg) → nr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-fleet/vg-root/workspace · andual@andual: /tmp/vgate-fleet/vg-alpha/workspace · andual@andual: /tmp/vgate-fleet/vg-beta/workspace · andual@andual: /tmp/vgate-fleet/vg-a

Clues:

- no report/menu surface was read on the bar strip after the right-click

## C10-close-returns-to-parent — Closing a child returns the selection to its parent

**FAIL**

Evidence:

- close claim → {"kind":"close-tab","closed":"w1:t5","currentBefore":"vg-a1","currentAfter":"vg-beta"}
- bar after close (/tmp/vgate/judge-final/close-child.jpg) → nr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-fleet/vg-root/workspace · andual@andual: /tmp/vgate-fleet/vg-beta/workspace · andual@andual: /tmp/vgate-fleet/vg-alpha/workspace · andual@andual: /tmp/vgate-fleet/vg-b

Clues:

- after closing vg-a1 the collector's selection was vg-beta, expected the parent vg-alpha

## Collector claims per phase (cross-check, not truth)

| phase | focus | ancestors | current | siblings | children |
|---|---|---|---|---|---|
| root-selected | vg-root | — | vg-root | — | vg-alpha vg-beta |
| alpha-selected | vg-alpha | vg-root | vg-alpha | vg-beta | vg-a1 vg-a2 |
| a1-selected | vg-a1 | vg-root vg-alpha | vg-a1 | vg-a2 | — |
| click-brother | vg-a1 | vg-root vg-alpha | vg-a1 | vg-a2 | — |
| click-ancestor | vg-a1 | vg-root vg-alpha | vg-a1 | vg-a2 | — |
| right-click | vg-alpha | vg-root | vg-alpha | vg-beta | vg-a1 vg-a2 |
| close-child | vg-beta | vg-root | vg-beta | vg-alpha | vg-b1 |

## Vision transcript

- `/tmp/vgate/judge-final/coh-12.1.jpg` — This is a horizontal strip cropped from the bottom of a terminal application. 1) Is there …
  > UNREADABLE
- `/tmp/vgate/judge-final/coh-24.3.jpg` — This is a horizontal strip cropped from the bottom of a terminal application. 1) Is there …
  > 1) No, there is no row of small agent/tab labels visible in this strip. 2) UNREADABLE 3) No, there is nothing garbled, overlapping, blank, or an error dialog covering it.
- `/tmp/vgate/judge-final/coh-36.4.jpg` — This is a horizontal strip cropped from the bottom of a terminal application. 1) Is there …
  > 1) Yes, there is a row of small agent/tab labels visible in this strip. 2) andual@andual:/tmp/vgate-fleet/vg-al/workspace, andual@andual:/tmp/vgate-fleet/vg-alpha/workspace, andual@andual:/tmp/vgate-fleet/vg-a2/workspace 3) No, there is no garbled, overlapping, blank, or error dialog covering it.
- `/tmp/vgate/judge-final/coh-48.5.jpg` — This is a horizontal strip cropped from the bottom of a terminal application. 1) Is there …
  > UNREADABLE
- `/tmp/vgate/judge-final/coh-60.7.jpg` — This is a horizontal strip cropped from the bottom of a terminal application. 1) Is there …
  > 1) No, there is no row of small agent/tab labels visible in this strip. 2) UNREADABLE 3) No, nothing is garbled, overlapping, blank, or is an error dialog covering it.
- `/tmp/vgate/judge-final/coh-72.8.jpg` — This is a horizontal strip cropped from the bottom of a terminal application. 1) Is there …
  > UNREADABLE
- `/tmp/vgate/judge-final/clk-16.2.jpg` — Read the text in this small strip verbatim. It contains a label and a time. If unreadable …
  > UNREADABLE
- `/tmp/vgate/judge-final/clk-40.4.jpg` — Read the text in this small strip verbatim. It contains a label and a time. If unreadable …
  > UNREADABLE
- `/tmp/vgate/judge-final/clk-64.7.jpg` — Read the text in this small strip verbatim. It contains a label and a time. If unreadable …
  > UNREADABLE
- `/tmp/vgate/judge-final/root-selected.jpg` — This strip is cropped from a terminal application and shows an agent bar. Transcribe it ro…
  > hr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 ∧[andal@andal: /tmp/vgate-fleet/vg-root/workspace] · andal@andal: /tmp/vgate-fleet/vg-alpha/workspace · andal@andal: /tmp/vgate-fleet/vg-beta/workspace harness shell · /tmp/vgate-fleet/vg-root/workspace - no focused agent session -
- `/tmp/vgate/judge-final/alpha-selected.jpg` — This strip is cropped from a terminal application and shows an agent bar. Transcribe it ro…
  > nr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 + andual@andual: /tmp/vgate-fleet/vg-root/workspace [ andual@andual: /tmp/vgate-fleet/vg-alpha/workspace ] · andual@andual: /tmp/vgate-fleet/vg-beta/workspace · andual@andual: /tmp/vgate-fleet/vg-al/workspace · andual@andual: /tmp/vgate-fleet/vg-a2/workspace harness shell · /tmp/vgate-fleet/vg-alpha/workspace [ ] [ ↑F ] [ ↑F ] [ S ] [ ↑C ] [ ↑L ] [ ↑A ]
- `/tmp/vgate/judge-final/a1-selected.jpg` — This strip is cropped from a terminal application and shows an agent bar. Transcribe it ro…
  > hr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-fleet/vg-root/workspace · andual@andual: /tmp/vgate-fleet/vg-alpha/workspace [andual@andual: /tmp/vgate-fleet/vg-al/workspace] · andual@andual: /tmp/vgate-fleet/vg-a2/workspace harness shell · /tmp/vgate-fleet/vg-al/workspace - no focused agent session -
- `/tmp/vgate/judge-final/click-brother.jpg` — This strip is cropped from a terminal application and shows an agent bar. Transcribe it ro…
  > hr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-fleet/vg-root/workspace · andual@andual: /tmp/vgate-fleet/vg-alpha/workspace [andual@andual: /tmp/vgate-fleet/vg-al/workspace] · andual@andual: /tmp/vgate-fleet/vg-a2/workspace harness shell · /tmp/vgate-fleet/vg-al/workspace - no focused agent session -
- `/tmp/vgate/judge-final/click-ancestor.jpg` — This strip is cropped from a terminal application and shows an agent bar. Transcribe it ro…
  > hr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-fleet/vg-root/workspace · andual@andual: /tmp/vgate-fleet/vg-alpha/workspace [andual@andual: /tmp/vgate-fleet/vg-al/workspace] · andual@andual: /tmp/vgate-fleet/vg-a2/workspace harness shell · /tmp/vgate-fleet/vg-al/workspace - no focused agent session -
- `/tmp/vgate/judge-final/right-click.jpg` — This strip is cropped from a terminal application and shows an agent bar. Transcribe it ro…
  > nr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-fleet/vg-root/workspace · andual@andual: /tmp/vgate-fleet/vg-alpha/workspace · andual@andual: /tmp/vgate-fleet/vg-beta/workspace · andual@andual: /tmp/vgate-fleet/vg-al/workspace · andual@andual: /tmp/vgate-fleet/vg-a2/workspace harness shell · /tmp/vgate-fleet/vg-alpha/workspace - no focused agent session -
- `/tmp/vgate/judge-final/close-child.jpg` — This strip is cropped from a terminal application and shows an agent bar. Transcribe it ro…
  > nr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-fleet/vg-root/workspace · andual@andual: /tmp/vgate-fleet/vg-beta/workspace · andual@andual: /tmp/vgate-fleet/vg-alpha/workspace · andual@andual: /tmp/vgate-fleet/vg-bl/workspace harness shell · /tmp/vgate-fleet/vg-beta/workspace - no focused agent session -