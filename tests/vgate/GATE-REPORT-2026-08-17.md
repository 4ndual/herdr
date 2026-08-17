# vl-gate report — tidus-bar-ui

VERDICT: REJECT

Video: `/tmp/vgate/tidus-bar-vgate.mp4` (71.1s). Reviewer vision: Mage-VL only (mage-vl @ http://127.0.0.1:8090/v1/chat/completions).
17 vision queries. Claims cross-checked from `/tmp/vgate/claims.jsonl`.

| criterion | verdict | blocking |
|---|---|---|
| C11-coherence No flicker, no incoherence, no garbled or empty bar | PASS | yes (core) |
| C1-names Entries show instance names, not harness:jobid | FAIL | yes (core) |
| C2-selected-indicator Selected tab carries the visual indicator and sits immediately after its parent | UNVERIFIABLE | yes (core) |
| C3-children-under-selected Children row renders under the SELECTED tab and holds the SELECTED tab's children | UNVERIFIABLE | yes (core) |
| C4-children-complete Children row is COMPLETE against the herdr pane list | UNVERIFIABLE | yes (core) |
| C5-brothers-stay Brothers remain visible through every selection change | UNVERIFIABLE | yes (core) |
| C6-lineage-complete Full ancestor lineage visible at depth, root and every intermediate | UNVERIFIABLE | yes (core) |
| C7-recursion-depth Recursion holds one level deeper at minimum | UNVERIFIABLE | yes (core) |
| C8-click-navigates-to-instance Clicking an entry focuses THAT INSTANCE's actual pane (matched by session id) | UNVERIFIABLE | yes (core) |
| C9-context-menu Right-click reports (context menu) instead of navigating | UNVERIFIABLE | yes |
| C10-close-returns-to-parent Closing a child returns the selection to its parent | FAIL | yes |

## C11-coherence — No flicker, no incoherence, no garbled or empty bar

**PASS**

Evidence:

- t=10.7s → UNREADABLE
- t=21.3s → 1) Yes, there is a row of small agent/tab labels visible in this strip. 2) The short names that can be read are: andual@andual: /tmp/vgate-f... 3) No, there is nothing garbled, overlapping, blank, or an error dialog cove
- t=32s → 1) No, there is no row of small agent/tab labels visible in this strip. 2) UNREADABLE 3) No, nothing is garbled, overlapping, blank, or is an error dialog covering it.
- t=42.6s → 1) No, there is no row of small agent/tab labels visible in this strip. 2) UNREADABLE 3) No, nothing is garbled, overlapping, blank, or is an error dialog covering it.
- t=53.3s → 1) No, there is no row of small agent/tab labels visible in this strip. 2) UNREADABLE 3) No, there is nothing garbled, overlapping, blank, or an error dialog covering it.
- t=64s → UNREADABLE
- t=14.2s → UNREADABLE
- t=35.5s → UNREADABLE
- t=56.9s → UNREADABLE

## C1-names — Entries show instance names, not harness:jobid

**FAIL**

Evidence:

- root-selected (/tmp/vgate/judge-final/root-selected.jpg) → hr 0.8.0+288 · · test 0.8.0+288 · · latest 0.8.0 [andal@andal: /tmp/vgate-f...] · andal@andal: /tmp/vgate-f... · andal@andal: /tmp/vgate-f... harness shell · /tmp/vgate-fleet/vg-root/workspace - no focused agent session -
- alpha-selected (/tmp/vgate/judge-final/alpha-selected.jpg) → nr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-f... [andual@andual: /tmp/vgate-f...] · andual@andual: /tmp/vgate-f... · andual@andual: /tmp/vgate-f... harness shell · /tmp/vgate-fleet/vg-alpha/workspace - no focused agent session -
- a1-selected (/tmp/vgate/judge-final/a1-selected.jpg) → hr 0.8.0+288 · · test 0.8.0+288 · · latest 0.8.0 · andual@andual: /tmp/vgate-f... · andual@andual: /tmp/vgate-f... L [andual@andual: /tmp/vgate-f... · andual@andual: /tmp/vgate-f... harness shell · /tmp/vgate-fleet/vg-al/workspace - no focused agent session -

Clues:

- root-selected: collector emits display_name ≠ name for 3 chip(s), e.g. vg-root → "andual@andual: /tmp/vgate-fleet/vg-root/workspace"
- root-selected: entries are not instance names — e.g. "harness shell · /tmp/vgate-fleet/vg-root/workspace"
- alpha-selected: collector emits display_name ≠ name for 5 chip(s), e.g. vg-root → "andual@andual: /tmp/vgate-fleet/vg-root/workspace"
- alpha-selected: entries are not instance names — e.g. "harness shell · /tmp/vgate-fleet/vg-alpha/workspace"
- a1-selected: collector emits display_name ≠ name for 4 chip(s), e.g. vg-root → "andual@andual: /tmp/vgate-fleet/vg-root/workspace"
- a1-selected: entries are not instance names — e.g. "harness shell · /tmp/vgate-fleet/vg-al/workspace"

## C2-selected-indicator — Selected tab carries the visual indicator and sits immediately after its parent

**UNVERIFIABLE**

Clues:

- root-selected: entries elided by the transcriber ("…") — unreadable at this label length, see C1
- alpha-selected: entries elided by the transcriber ("…") — unreadable at this label length, see C1
- a1-selected: entries elided by the transcriber ("…") — unreadable at this label length, see C1

## C3-children-under-selected — Children row renders under the SELECTED tab and holds the SELECTED tab's children

**UNVERIFIABLE**

Clues:

- root-selected: entries elided by the transcriber ("…") — unreadable at this label length, see C1
- alpha-selected: entries elided by the transcriber ("…") — unreadable at this label length, see C1
- CLAIM CROSS-CHECK: the collector's own snapshot DOES satisfy this at [root-selected, alpha-selected, click-brother, click-ancestor, right-click]. The failure above is that the VIDEO could not evidence it — which on this build follows from C1: entries are ~50-character pane-title paths, so a frame transcription truncates before the row is read. Fix the labels and re-run before treating this as a tree-model defect.

## C4-children-complete — Children row is COMPLETE against the herdr pane list

**UNVERIFIABLE**

Evidence:

- root-selected: pane-list ground truth = [vg-alpha,vg-beta]; collector claims child row = [vg-alpha,vg-beta]
- alpha-selected: pane-list ground truth = [vg-a1,vg-a2]; collector claims child row = [vg-a1,vg-a2]

Clues:

- root-selected: entries elided ("…") — unreadable at this label length, see C1
- alpha-selected: entries elided ("…") — unreadable at this label length, see C1
- CLAIM CROSS-CHECK: the collector's own snapshot DOES satisfy this at [root-selected, alpha-selected, click-brother, click-ancestor, right-click, close-child]. The failure above is that the VIDEO could not evidence it — which on this build follows from C1: entries are ~50-character pane-title paths, so a frame transcription truncates before the row is read. Fix the labels and re-run before treating this as a tree-model defect.

## C5-brothers-stay — Brothers remain visible through every selection change

**UNVERIFIABLE**

Clues:

- alpha-selected: entries elided ("…") — unreadable at this label length, see C1
- a1-selected: entries elided ("…") — unreadable at this label length, see C1
- click-brother: entries elided ("…") — unreadable at this label length, see C1
- CLAIM CROSS-CHECK: the collector's own snapshot DOES satisfy this at [alpha-selected, a1-selected, click-brother, click-ancestor, right-click, close-child]. The failure above is that the VIDEO could not evidence it — which on this build follows from C1: entries are ~50-character pane-title paths, so a frame transcription truncates before the row is read. Fix the labels and re-run before treating this as a tree-model defect.

## C6-lineage-complete — Full ancestor lineage visible at depth, root and every intermediate

**UNVERIFIABLE**

Evidence:

- a1-selected (/tmp/vgate/judge-final/a1-selected.jpg) → hr 0.8.0+288 · · test 0.8.0+288 · · latest 0.8.0 · andual@andual: /tmp/vgate-f... · andual@andual: /tmp/vgate-f... L [andual@andual: /tmp/vgate-f... · andual@andual: /tmp/vgate-f... harness shell · /tmp/vgate-fleet/vg-al/workspace - no focused agent session -

Clues:

- a1-selected: entries elided ("…") — unreadable at this label length, see C1

## C7-recursion-depth — Recursion holds one level deeper at minimum

**UNVERIFIABLE**

Evidence:

- a1-selected (/tmp/vgate/judge-final/a1-selected.jpg) → hr 0.8.0+288 · · test 0.8.0+288 · · latest 0.8.0 · andual@andual: /tmp/vgate-f... · andual@andual: /tmp/vgate-f... L [andual@andual: /tmp/vgate-f... · andual@andual: /tmp/vgate-f... harness shell · /tmp/vgate-fleet/vg-al/workspace - no focused agent session -

Clues:

- a1-selected: entries elided ("…") — unreadable at this label length, see C1
- CLAIM CROSS-CHECK: the collector's own snapshot DOES satisfy this at [a1-selected]. The failure above is that the VIDEO could not evidence it — which on this build follows from C1: entries are ~50-character pane-title paths, so a frame transcription truncates before the row is read. Fix the labels and re-run before treating this as a tree-model defect.

## C8-click-navigates-to-instance — Clicking an entry focuses THAT INSTANCE's actual pane (matched by session id)

**UNVERIFIABLE**

Evidence:

- click-brother: clicked vg-a2 at (598,912); want tab=w1:t6 pane=w1:p6 term=term_659442a3c85326; got tab=w1:t3 pane=w1:p3 term=term_659442a3be37f3
- click-brother bar after click → nr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-f... [andual@andual: /tmp/vgate-f...] · andual@andual: /tmp/vgate-f... · andual@andual: /tmp/vgate-f... harness shell · /tmp/vg
- click-ancestor: clicked vg-root at (401,878); want tab=w1:t2 pane=w1:p2 term=term_659442a3ba7702; got tab=w1:t3 pane=w1:p3 term=term_659442a3be37f3
- click-ancestor bar after click → test 0.8.0 (+288 local) - version current, nui verified (latest 0.8.0) - https://github.com/nerddev/nerd/ releases/tag/v0.8.0 - checked + andual@andual: /tmp/vgate-f... [andual@andual: /tmp/vgate-f...

Clues:

- click-brother: click at (598,912) DID navigate (w1:t5 → w1:t3), so the bar is live and clickable — but the chip was targeted by even division of the row (1 ink cluster(s) for 2 chips), so this cannot be attributed to a specific entry. INSTRUMENT-LIMITED, not a navigation defect.
- click-ancestor: click at (401,878) changed nothing — focus stayed on w1:t3. INSTRUMENT-LIMITED: a click that moves nothing cannot be told apart from a click that missed the bar, so this is NOT reported as a navigation defect. Confirm the target row before trusting it.

## C9-context-menu — Right-click reports (context menu) instead of navigating

**UNVERIFIABLE**

Evidence:

- right-click claim → {"kind":"right-click","target":null,"before":{"tab_id":"w1:t3","pane_id":"w1:p3","terminal_id":"term_659442a3be37f3"},"error":"no chip found for the right-click"}
- bar after right-click (/tmp/vgate/judge-final/right-click.jpg) → test 0.8.0 (+288 local) - version current, nui verified (latest 0.8.0) - https://github.com/nerddev/nerd/ releases/tag/v0.8.0 - checked + andual@andual: /tmp/vgate-f... [andual@andual: /tmp/vgate-f...] - andual@andual: /tmp/vgate-f... - and

Clues:

- no chip found for the right-click — the gesture was never delivered, so this says nothing about the bar. INSTRUMENT-LIMITED: chip targeting is blocked by C1 (entries are truncated pane-title paths).

## C10-close-returns-to-parent — Closing a child returns the selection to its parent

**FAIL**

Evidence:

- close claim → {"kind":"close-tab","closed":"w1:t5","currentBefore":"vg-a1","currentAfter":"vg-beta"}
- bar after close (/tmp/vgate/judge-final/close-child.jpg) → The image displays a terminal window with a dark background and white text. The terminal is running a command that appears to be related to a version control system, specifically Git. The command being executed is `git tag -a v0.8.0 -m "tes

Clues:

- after closing vg-a1 the collector's selection was vg-beta, expected the parent vg-alpha
- vg-alpha was not read on the bar after the close

## Collector claims per phase (cross-check, not truth)

| phase | focus | ancestors | current | siblings | children |
|---|---|---|---|---|---|
| root-selected | vg-root | — | vg-root | — | vg-alpha vg-beta |
| alpha-selected | vg-alpha | vg-root | vg-alpha | vg-beta | vg-a1 vg-a2 |
| a1-selected | vg-a1 | vg-root vg-alpha | vg-a1 | vg-a2 | — |
| click-brother | vg-alpha | vg-root | vg-alpha | vg-beta | vg-a1 vg-a2 |
| click-ancestor | vg-alpha | vg-root | vg-alpha | vg-beta | vg-a1 vg-a2 |
| right-click | vg-alpha | vg-root | vg-alpha | vg-beta | vg-a1 vg-a2 |
| close-child | vg-beta | vg-root | vg-beta | vg-alpha | vg-b1 |

## Vision transcript

- `/tmp/vgate/judge-final/coh-10.7.jpg` — This is a horizontal strip cropped from the bottom of a terminal application. 1) Is there …
  > UNREADABLE
- `/tmp/vgate/judge-final/coh-21.3.jpg` — This is a horizontal strip cropped from the bottom of a terminal application. 1) Is there …
  > 1) Yes, there is a row of small agent/tab labels visible in this strip. 2) The short names that can be read are: andual@andual: /tmp/vgate-f... 3) No, there is nothing garbled, overlapping, blank, or an error dialog covering it.
- `/tmp/vgate/judge-final/coh-32.jpg` — This is a horizontal strip cropped from the bottom of a terminal application. 1) Is there …
  > 1) No, there is no row of small agent/tab labels visible in this strip. 2) UNREADABLE 3) No, nothing is garbled, overlapping, blank, or is an error dialog covering it.
- `/tmp/vgate/judge-final/coh-42.6.jpg` — This is a horizontal strip cropped from the bottom of a terminal application. 1) Is there …
  > 1) No, there is no row of small agent/tab labels visible in this strip. 2) UNREADABLE 3) No, nothing is garbled, overlapping, blank, or is an error dialog covering it.
- `/tmp/vgate/judge-final/coh-53.3.jpg` — This is a horizontal strip cropped from the bottom of a terminal application. 1) Is there …
  > 1) No, there is no row of small agent/tab labels visible in this strip. 2) UNREADABLE 3) No, there is nothing garbled, overlapping, blank, or an error dialog covering it.
- `/tmp/vgate/judge-final/coh-64.jpg` — This is a horizontal strip cropped from the bottom of a terminal application. 1) Is there …
  > UNREADABLE
- `/tmp/vgate/judge-final/clk-14.2.jpg` — Read the text in this small strip verbatim. It contains a label and a time. If unreadable …
  > UNREADABLE
- `/tmp/vgate/judge-final/clk-35.5.jpg` — Read the text in this small strip verbatim. It contains a label and a time. If unreadable …
  > UNREADABLE
- `/tmp/vgate/judge-final/clk-56.9.jpg` — Read the text in this small strip verbatim. It contains a label and a time. If unreadable …
  > UNREADABLE
- `/tmp/vgate/judge-final/root-selected.jpg` — This strip is cropped from a terminal application and shows an agent bar. Transcribe it ro…
  > hr 0.8.0+288 · · test 0.8.0+288 · · latest 0.8.0 [andal@andal: /tmp/vgate-f...] · andal@andal: /tmp/vgate-f... · andal@andal: /tmp/vgate-f... harness shell · /tmp/vgate-fleet/vg-root/workspace - no focused agent session -
- `/tmp/vgate/judge-final/alpha-selected.jpg` — This strip is cropped from a terminal application and shows an agent bar. Transcribe it ro…
  > nr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-f... [andual@andual: /tmp/vgate-f...] · andual@andual: /tmp/vgate-f... · andual@andual: /tmp/vgate-f... harness shell · /tmp/vgate-fleet/vg-alpha/workspace - no focused agent session -
- `/tmp/vgate/judge-final/a1-selected.jpg` — This strip is cropped from a terminal application and shows an agent bar. Transcribe it ro…
  > hr 0.8.0+288 · · test 0.8.0+288 · · latest 0.8.0 · andual@andual: /tmp/vgate-f... · andual@andual: /tmp/vgate-f... L [andual@andual: /tmp/vgate-f... · andual@andual: /tmp/vgate-f... harness shell · /tmp/vgate-fleet/vg-al/workspace - no focused agent session -
- `/tmp/vgate/judge-final/click-brother.jpg` — This strip is cropped from a terminal application and shows an agent bar. Transcribe it ro…
  > nr 0.8.0+288 · test 0.8.0+288 · latest 0.8.0 · andual@andual: /tmp/vgate-f... [andual@andual: /tmp/vgate-f...] · andual@andual: /tmp/vgate-f... · andual@andual: /tmp/vgate-f... harness shell · /tmp/vgate-fleet/vg-alpha/workspace - no focused agent session -
- `/tmp/vgate/judge-final/click-ancestor.jpg` — This strip is cropped from a terminal application and shows an agent bar. Transcribe it ro…
  > test 0.8.0 (+288 local) - version current, nui verified (latest 0.8.0) - https://github.com/nerddev/nerd/ releases/tag/v0.8.0 - checked + andual@andual: /tmp/vgate-f... [andual@andual: /tmp/vgate-f...] - andual@andual: /tmp/vgate-f... - andual@andual: /tmp/vgate-f... harness shell - /tmp/vgate-fleet/vg-alpha/workspace - no focused agent session -
- `/tmp/vgate/judge-final/right-click.jpg` — This strip is cropped from a terminal application and shows an agent bar. Transcribe it ro…
  > test 0.8.0 (+288 local) - version current, nui verified (latest 0.8.0) - https://github.com/nerddev/nerd/ releases/tag/v0.8.0 - checked + andual@andual: /tmp/vgate-f... [andual@andual: /tmp/vgate-f...] - andual@andual: /tmp/vgate-f... - andual@andual: /tmp/vgate-f... harness shell - /tmp/vgate-fleet/vg-alpha/workspace - no focused agent session -
- `/tmp/vgate/judge-final/close-child.jpg` — This strip is cropped from a terminal application and shows an agent bar. Transcribe it ro…
  > UNREADABLE
- `/tmp/vgate/judge-final/close-child.jpg` — Describe everything you see in this image strip in detail: every row, every label, and any…
  > The image displays a terminal window with a dark background and white text. The terminal is running a command that appears to be related to a version control system, specifically Git. The command being executed is `git tag -a v0.8.0 -m "test 0.8.0 (+288 local) - version current, noi verified (latest 0.8.0)" https://github.com/nerdadev/nerd/releases/tag/v0.8.0 - checked`. The output of the command 