#!/usr/bin/env bun
/**
 * The scripted drive, on camera.
 *
 * Every state change a criterion depends on is performed the way a user would
 * perform it, and the two kinds are deliberately different:
 *
 *   selection setup   `tab focus` over the API. Cheap, deterministic, and not
 *                     itself under test — it only has to GET the bar into the
 *                     state a criterion is about.
 *   click criteria    a REAL xdotool button press at the chip's own pixels.
 *                     C8 is precisely the claim that a click navigates, so an
 *                     API call that fakes the effect would prove nothing. An
 *                     interaction that is not visibly performed does not count.
 *
 * Chip pixels are found by INK-RUN MATCHING rather than by reimplementing
 * render::layout(): that arithmetic is being actively rewritten by other agents
 * right now, and a copy of it here would drift silently. Instead the frame is
 * scanned for glyph runs and matched against the names the collector says are
 * on each row — which also means the gate clicks what is actually DRAWN, which
 * is the thing a video gate is supposed to judge.
 *
 * Every phase writes phase.txt (burned into the video top-left) and appends a
 * claim record: the collector snapshot AND the herdr pane list at that instant.
 * The pane list is the ground truth C4 needs — a frame alone cannot say what
 * children SHOULD have been on the row.
 */
import { spawnSync } from "node:child_process";
import { appendFileSync, mkdirSync, writeFileSync } from "node:fs";
import { DISPLAY, FIXTURES, SOCKET, barInput, herdr, sleep, snapshot } from "./stage.ts";
import type { Phase } from "./checklist.ts";

export const OUT = process.env.VGATE_OUT ?? "/tmp/vgate";
const PHASE_FILE = `${OUT}/phase.txt`;
const CLAIMS = `${OUT}/claims.jsonl`;
/** Long enough that a frame-sampling reviewer lands inside every state. */
const HOLD = Number(process.env.VGATE_HOLD ?? 9000);

let started = 0;
const elapsed = () => (Date.now() - started) / 1000;

function x(...args: string[]) {
  return spawnSync("xdotool", args, { encoding: "utf8", env: { ...process.env, DISPLAY } });
}

/** Frame → raw greyscale, so chip glyphs can be found without a PNG decoder. */
function grabGray(): { px: Uint8Array; w: number; h: number } {
  const w = 1920, h = 1080;
  const r = spawnSync("ffmpeg", [
    "-v", "error", "-f", "x11grab", "-video_size", `${w}x${h}`, "-i", DISPLAY,
    "-frames:v", "1", "-pix_fmt", "gray", "-f", "rawvideo", "-",
  ], { maxBuffer: 1 << 28 });
  return { px: new Uint8Array(r.stdout), w, h };
}

const INK = 60;

/** Contiguous bands of pixel rows that contain any ink. */
function rowBands(g: { px: Uint8Array; w: number; h: number }) {
  const bands: [number, number][] = [];
  let start: number | null = null;
  for (let y = 0; y < g.h; y++) {
    let ink = 0;
    for (let px = y * g.w, end = px + g.w; px < end; px += 2) if (g.px[px] > INK) { ink = 1; break; }
    if (ink && start === null) start = y;
    else if (!ink && start !== null) { bands.push([start, y - 1]); start = null; }
  }
  if (start !== null) bands.push([start, g.h - 1]);
  return bands.filter(([a, b]) => b - a >= 6);
}

/** Glyph runs within a row band: [x0, x1] separated by >= gapMin blank columns. */
function inkRuns(g: { px: Uint8Array; w: number; h: number }, y0: number, y1: number, gapMin = 5) {
  const runs: [number, number][] = [];
  let start: number | null = null, gap = 0;
  for (let cx = 0; cx < g.w; cx++) {
    let ink = false;
    for (let y = y0; y <= y1; y++) if (g.px[y * g.w + cx] > INK) { ink = true; break; }
    if (ink) { if (start === null) start = cx; gap = 0; }
    else if (start !== null) { if (++gap >= gapMin) { runs.push([start, cx - gap]); start = null; gap = 0; } }
  }
  if (start !== null) runs.push([start, g.w - 1]);
  return runs;
}

const CELL_W = Number(process.env.VGATE_CELL_W ?? 8.14);
/** Columns of blank pixels that separate two chips rather than two words. */
const CHIP_GAP = Number(process.env.VGATE_CHIP_GAP ?? 26);

/** Merge word-level runs into chip-level clusters. */
function cluster(runs: [number, number][], gap = CHIP_GAP): [number, number][] {
  const out: [number, number][] = [];
  for (const [a, b] of runs) {
    const last = out[out.length - 1];
    if (last && a - last[1] <= gap) last[1] = b;
    else out.push([a, b]);
  }
  return out;
}

/**
 * Locate the `index`-th chip of a row, by POSITION rather than by label width.
 *
 * Width matching was the first approach and it does not survive this build: the
 * bar draws pane titles, which are paths containing spaces and slashes, so one
 * chip becomes a dozen glyph runs — and the rendered text is TRUNCATED besides,
 * so a 50-character display_name occupies ~22 columns. Measured on the family
 * row: two ~50-char labels shared 365px of ink in five fragments. A matcher
 * sized to the label clicked empty space past the end of the row, and the gate
 * scored that as "the click did not navigate".
 *
 * So: merge fragments into chip-level clusters, and take the Nth. When the
 * clustering disagrees with how many chips the collector says are on the row,
 * fall back to dividing the row's ink span into equal parts — chips are laid out
 * left to right either way. `ambiguous` is reported so a click that rests on the
 * weaker method is not mistaken for a clean one.
 */
export function findChipAt(index: number, rowNames: string[], rowHint?: string):
  { x: number; y: number; ambiguous: boolean; clusters: number } | null {
  const g = grabGray();
  let best: { x: number; y: number; ambiguous: boolean; clusters: number; score: number } | null = null;
  // Search only the bar region. Without this bound the matcher happily picked a
  // band from the harness-indicator grid at the TOP of the stage and clicked
  // (1905, 26) — nowhere near the bar, which the gate then read as "the click
  // did not navigate". The bar is carved from the bottom edge of the pane area,
  // so the bottom quarter is a generous home for it and excludes everything else.
  const yMin = Number(process.env.VGATE_BAR_YMIN ?? Math.round(g.h * 0.78));
  const yMax = Number(process.env.VGATE_BAR_YMAX ?? Math.round(g.h * 0.95));
  for (const [y0, y1] of rowBands(g)) {
    if (y0 < yMin || y1 > yMax) continue;             // outside the bar region
    if (y1 - y0 > 40) continue;                       // merged bands are not one row
    const cl = cluster(inkRuns(g, y0, y1));
    if (!cl.length) continue;
    const span: [number, number] = [cl[0][0], cl[cl.length - 1][1]];
    if (span[1] - span[0] < 40) continue;             // too small to be the bar row
    let x: number, ambiguous: boolean;
    if (cl.length === rowNames.length) {
      const [a, b] = cl[index]; x = Math.round((a + b) / 2); ambiguous = false;
    } else {
      const wide = (span[1] - span[0]) / Math.max(1, rowNames.length);
      x = Math.round(span[0] + wide * (index + 0.5)); ambiguous = true;
    }
    // Prefer the band whose cluster count matches the expected chip count.
    const score = (cl.length === rowNames.length ? 1000 : 0) - Math.abs(cl.length - rowNames.length);
    if (!best || score > best.score)
      best = { x, y: Math.round((y0 + y1) / 2), ambiguous, clusters: cl.length, score };
  }
  return best ? { x: best.x, y: best.y, ambiguous: best.ambiguous, clusters: best.clusters } : null;
}

/** Identity of an instance's pane, for the C8 session-level check. */
function instanceIdentity(tabId: string) {
  const s = snapshot();
  const pane = (s?.panes ?? []).find((p: any) => p.tab_id === tabId);
  return { tab_id: tabId, pane_id: pane?.pane_id ?? null, terminal_id: pane?.terminal_id ?? null };
}

function focusedIdentity() {
  const s = snapshot();
  const pane = (s?.panes ?? []).find((p: any) => p.focused) ??
    (s?.panes ?? []).find((p: any) => p.pane_id === s?.focused_pane_id);
  return {
    tab_id: s?.focused_tab_id ?? null,
    pane_id: s?.focused_pane_id ?? null,
    terminal_id: pane?.terminal_id ?? null,
  };
}

/** Panes present in this session, keyed by the instance they were opened for. */
function paneGroundTruth(tabs: Record<string, { tab: string; pane: string }>) {
  const s = snapshot();
  const live = new Set((s?.tabs ?? []).map((t: any) => t.tab_id));
  return Object.fromEntries(
    Object.entries(tabs).filter(([, v]) => live.has(v.tab)).map(([k, v]) => [k, v.tab]),
  );
}

export type Claim = {
  t: number; phase: Phase | string; note: string;
  bar: any; focused: any; panesPresent: Record<string, string>;
  action?: any;
};

export function claim(phase: Phase | string, note: string, tabs: any, action?: any) {
  const rec: Claim = {
    t: +elapsed().toFixed(2), phase, note,
    bar: barInput(), focused: focusedIdentity(), panesPresent: paneGroundTruth(tabs), action,
  };
  appendFileSync(CLAIMS, JSON.stringify(rec) + "\n");
  console.log(`${String(rec.t).padStart(7)}s  [${phase}] ${note}`);
  return rec;
}

function setPhase(p: string) {
  writeFileSync(PHASE_FILE, `vgate: ${p}`);
}

/**
 * The LABELS the collector says are on each row, for the ink matcher.
 *
 * display_name, not name. The renderer draws display_name whenever it differs
 * from the harness name (render.rs), and on this build that is the pane title —
 * so a matcher sized to `vg-a2` (5 cells) hunts a 40px run while the bar
 * actually drew a 360px path. The first run clicked whatever happened to be
 * ~40px wide and landed on the wrong instance, which the gate then reported as
 * a navigation defect. Target what is DRAWN, or the gate manufactures findings.
 */
function rowsOf(bar: any) {
  const chips = bar?.chips ?? [];
  const label = (c: any) => (c.display_name && c.display_name.length ? c.display_name : c.name);
  const pick = (r: string) => chips.filter((c: any) => c.role === r);
  return {
    lineage: pick("ancestor").map(label),
    family: [...pick("current"), ...pick("sibling")].map(label),
    children: pick("child").map(label),
    labelOf: (name: string) => { const c = chips.find((x: any) => x.name === name); return c ? label(c) : name; },
  };
}

export async function runScenario(tabs: Record<string, { tab: string; pane: string }>) {
  mkdirSync(OUT, { recursive: true });
  writeFileSync(CLAIMS, "");
  started = Date.now();

  const focus = async (name: string, phase: Phase, note: string) => {
    setPhase(phase);
    herdr("tab", "focus", tabs[name].tab);
    await sleep(HOLD);
    claim(phase, note, tabs);
  };

  // ── 1-3: selection at three depths ────────────────────────────────────────
  await focus("vg-root", "root-selected", "vg-root selected — children row must hold vg-alpha + vg-beta");
  await focus("vg-alpha", "alpha-selected", "vg-alpha selected — brother vg-beta stays, children vg-a1 + vg-a2 appear");
  await focus("vg-a1", "a1-selected", "vg-a1 selected at depth 2 — lineage vg-root + vg-alpha both stay, brother vg-a2 stays");

  // ── 4: real click on a BROTHER ────────────────────────────────────────────
  setPhase("click-brother");
  {
    const rows = rowsOf(barInput());
    const target = findChipAt(rows.family.indexOf(rows.labelOf("vg-a2")), rows.family);
    const wantId = instanceIdentity(tabs["vg-a2"].tab);
    let action: any = { kind: "left-click", chip: "vg-a2", target, want: wantId, before: focusedIdentity() };
    if (target) {
      x("mousemove", String(target.x), String(target.y));
      await sleep(700);
      x("click", "1");
      await sleep(HOLD);
      action.got = focusedIdentity();
      action.landed = action.got.tab_id === wantId.tab_id;
    } else {
      action.error = "chip vg-a2 not found on screen";
      await sleep(2000);
    }
    claim("click-brother", "clicked brother vg-a2 — must focus vg-a2's OWN pane", tabs, action);
  }

  // ── 5: real click on an ANCESTOR (the way back up) ────────────────────────
  setPhase("click-ancestor");
  {
    const rows = rowsOf(barInput());
    const target = findChipAt(Math.max(0, rows.lineage.indexOf(rows.labelOf("vg-root"))), rows.lineage);
    const wantId = instanceIdentity(tabs["vg-root"].tab);
    let action: any = { kind: "left-click", chip: "vg-root", target, want: wantId, before: focusedIdentity() };
    if (target) {
      x("mousemove", String(target.x), String(target.y));
      await sleep(700);
      x("click", "1");
      await sleep(HOLD);
      action.got = focusedIdentity();
      action.landed = action.got.tab_id === wantId.tab_id;
    } else {
      action.error = "chip vg-root not found on screen";
      await sleep(2000);
    }
    claim("click-ancestor", "clicked ancestor vg-root — must land on vg-root's OWN pane", tabs, action);
  }

  // ── 6: right-click reports instead of navigating ──────────────────────────
  setPhase("right-click");
  {
    herdr("tab", "focus", tabs["vg-alpha"].tab);
    await sleep(3500);
    const rows = rowsOf(barInput());
    const target = rows.children.length ? findChipAt(0, rows.children) : findChipAt(rows.family.length - 1, rows.family);
    const before = focusedIdentity();
    let action: any = { kind: "right-click", target, before };
    if (target) {
      x("mousemove", String(target.x), String(target.y));
      await sleep(600);
      x("click", "3");
      await sleep(HOLD);
      action.after = focusedIdentity();
      action.navigated = action.after.tab_id !== before.tab_id;
    } else action.error = "no chip found for the right-click";
    claim("right-click", "right-clicked a chip — must REPORT, not navigate", tabs, action);
    x("key", "Escape");
    await sleep(1500);
  }

  // ── 7: closing the selected child returns to the parent ───────────────────
  setPhase("close-child");
  {
    herdr("tab", "focus", tabs["vg-a1"].tab);
    await sleep(4000);
    const before = barInput();
    herdr("tab", "close", tabs["vg-a1"].tab);
    await sleep(HOLD);
    const after = barInput();
    claim("close-child", "closed the selected child vg-a1 — selection must land on the parent", tabs, {
      kind: "close-tab", closed: tabs["vg-a1"].tab,
      currentBefore: (before?.chips ?? []).find((c: any) => c.role === "current")?.name ?? null,
      currentAfter: (after?.chips ?? []).find((c: any) => c.role === "current")?.name ?? null,
    });
  }

  setPhase("done");
  await sleep(2500);
  console.log(`scenario done in ${elapsed().toFixed(1)}s → ${CLAIMS}`);
}
