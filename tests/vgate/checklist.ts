/**
 * The tidus-bar video gate checklist.
 *
 * Each entry is a CRITERION the recording must evidence. `rejectOnFail: true`
 * means a failure blocks "done" — the gate exits non-zero and no amount of
 * passing siblings rescues it.
 *
 * Two evidence channels per criterion, and they are not interchangeable:
 *
 *   video   — a Mage-VL answer about an extracted frame. This is what the gate
 *             JUDGES. Per the vl-gate doctrine the reviewer's only vision is
 *             Mage-VL; nothing here may assert "I saw the video".
 *   claim   — the collector snapshot + herdr pane list captured at the same
 *             moment. These are CLAIMS to cross-check, never truth. Their job
 *             is to make a video failure legible ("the bar drew 2 children, the
 *             pane list says there are 3") and to supply ground truth for the
 *             completeness rules, which a frame alone cannot establish.
 *
 * A criterion whose video evidence is unreadable is UNVERIFIABLE, not passed.
 * For a `core` criterion that still rejects — the skill is explicit that
 * unreadable evidence for a core claim is a REJECT, because a gate that passes
 * what it cannot see is not a gate.
 */

export type Phase =
  | "root-selected"
  | "alpha-selected"
  | "a1-selected"
  | "click-brother"
  | "click-ancestor"
  | "right-click"
  | "close-child";

export type Criterion = {
  id: string;
  title: string;
  /** Which recorded phases can evidence it. */
  phases: Phase[];
  rejectOnFail: boolean;
  /** Unreadable video evidence rejects rather than merely weakening. */
  core: boolean;
  /** What the reviewer is looking for, in one line (goes into the report). */
  expect: string;
};

export const CHECKLIST: Criterion[] = [
  {
    id: "C1-names",
    title: "Entries show instance names, not harness:jobid",
    phases: ["root-selected", "alpha-selected", "a1-selected"],
    rejectOnFail: true,
    core: true,
    expect:
      "every bar entry reads as a plain agent name (vg-root, vg-alpha, vg-a1 …); " +
      "no entry contains a colon followed by a job id, and none is of the form <harness>:<jobid>",
  },
  {
    id: "C2-selected-indicator",
    title: "Selected tab carries the visual indicator and sits immediately after its parent",
    phases: ["root-selected", "alpha-selected", "a1-selected"],
    rejectOnFail: true,
    core: true,
    expect:
      "exactly one entry is visibly marked as selected (bracketed/highlighted cartouche), " +
      "and it is the first entry of the family row that hangs off its parent",
  },
  {
    id: "C3-children-under-selected",
    title: "Children row renders under the SELECTED tab and holds the SELECTED tab's children",
    phases: ["root-selected", "alpha-selected"],
    rejectOnFail: true,
    core: true,
    expect:
      "the row beneath the selection lists the selected agent's own children; " +
      "when the selection moves down a level the ancestor's children must NOT remain underneath",
  },
  {
    id: "C4-children-complete",
    title: "Children row is COMPLETE against the herdr pane list",
    phases: ["root-selected", "alpha-selected"],
    rejectOnFail: true,
    core: true,
    expect:
      "every child instance of the selected tab that has a pane in this session is present on the child row " +
      "(cross-checked against the pane list captured at the same instant)",
  },
  {
    id: "C5-brothers-stay",
    title: "Brothers remain visible through every selection change",
    phases: ["alpha-selected", "a1-selected", "click-brother"],
    rejectOnFail: true,
    core: true,
    expect:
      "the selected agent's siblings stay drawn on the family row before and after the selection moves onto one of them; " +
      "a selection must never empty the row",
  },
  {
    id: "C6-lineage-complete",
    title: "Full ancestor lineage visible at depth, root and every intermediate",
    phases: ["a1-selected"],
    rejectOnFail: true,
    core: true,
    expect:
      "with a selection 2+ levels deep, the root AND every intermediate ancestor are still rendered on the lineage row " +
      "(vg-root and vg-alpha both present while vg-a1 is selected)",
  },
  {
    id: "C7-recursion-depth",
    title: "Recursion holds one level deeper at minimum",
    phases: ["a1-selected"],
    rejectOnFail: true,
    core: true,
    expect:
      "the bar renders a coherent lineage → family → child structure at depth 2, not only at the root",
  },
  {
    id: "C8-click-navigates-to-instance",
    title: "Clicking an entry focuses THAT INSTANCE's actual pane (matched by session id)",
    phases: ["click-brother", "click-ancestor"],
    rejectOnFail: true,
    core: true,
    expect:
      "after a real click on a bar entry the terminal area shows that instance's own pane; " +
      "verified by the focused pane/session id equalling the clicked chip's own pane id — " +
      "landing on another pane of the same-named harness is a FAIL",
  },
  {
    id: "C9-context-menu",
    title: "Right-click reports (context menu) instead of navigating",
    phases: ["right-click"],
    rejectOnFail: true,
    core: false,
    expect: "a right-click on an entry visibly opens a report/menu surface naming that agent",
  },
  {
    id: "C10-close-returns-to-parent",
    title: "Closing a child returns the selection to its parent",
    phases: ["close-child"],
    rejectOnFail: true,
    core: false,
    expect:
      "after the selected child's pane is closed the bar's selection lands on the parent, " +
      "and the parent's family row is drawn again",
  },
  {
    id: "C11-coherence",
    title: "No flicker, no incoherence, no garbled or empty bar",
    phases: [
      "root-selected",
      "alpha-selected",
      "a1-selected",
      "click-brother",
      "click-ancestor",
      "right-click",
      "close-child",
    ],
    rejectOnFail: true,
    core: true,
    expect:
      "the burned clock is strictly monotonic across samples, the bar is drawn in every sample " +
      "(never blank, never overlapping/garbled), and no error dialog covers the stage",
  },
];

export const byId = (id: string) => CHECKLIST.find((c) => c.id === id)!;
