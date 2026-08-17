#!/usr/bin/env bun
/**
 * The reviewer. Judges THE VIDEO, using Mage-VL as its only vision.
 *
 * Doctrine (skills/vl-gate/SKILL.md), enforced here rather than trusted:
 *   * REVIEWER ONLY — this file reads a video and writes a report. It never
 *     touches the plugin, the session, or the scenario.
 *   * the object judged is the VIDEO. claims.jsonl is a set of CLAIMS to
 *     cross-check, never truth. A criterion whose only support is a claim is
 *     reported as claim-only, and for a core criterion that is not a pass.
 *   * every visual statement traces to a Mage-VL answer about a named frame.
 *   * a REJECT ships clues: phase, timestamp, frame path, what was seen.
 *
 * Payload discipline (same skill, after a GPU-saturation incident): questions
 * are asked against a CROP of the bar band, not a 1920x1080 frame — image
 * prefill is where the GPU time goes, and a crop is ~10-20x cheaper. Full
 * frames are used only for the open-ended coherence probe.
 */
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { CHECKLIST, type Criterion, type Phase } from "./checklist.ts";
import type { Claim } from "./scenario.ts";

const VL_ENDPOINT = process.env.VL_ENDPOINT ?? "http://127.0.0.1:8090/v1/chat/completions";
const VL_MODEL = process.env.VL_MODEL ?? "mage-vl";

export type Ask = { q: string; img: string; a: string };
const asked: Ask[] = [];

/**
 * One Mage-VL question about one image.
 *
 * Retries because llama-swap returns 500 while the model cold-loads, and an
 * errored answer that is treated as "nothing wrong" is how a vision gate
 * silently stops seeing. An unanswered question stays unanswered.
 */
export async function ask(q: string, img: string, retries = 3): Promise<string> {
  const b64 = Buffer.from(readFileSync(img)).toString("base64");
  const body = {
    model: VL_MODEL, temperature: 0.1, max_tokens: 420,
    messages: [
      { role: "system", content: "You are a visual QA assistant. Answer questions about the provided screenshot(s) in plain, factual prose. Read text verbatim where asked. Never output UI actions." },
      { role: "user", content: [
        { type: "text", text: q },
        { type: "image_url", image_url: { url: "data:image/jpeg;base64," + b64 } }] },
    ],
  };
  let last = "";
  for (let i = 1; i <= retries; i++) {
    try {
      const r = await fetch(VL_ENDPOINT, {
        method: "POST", headers: { "content-type": "application/json" },
        body: JSON.stringify(body), signal: AbortSignal.timeout(300_000),
      });
      if (r.ok) {
        const a = ((await r.json()).choices?.[0]?.message?.content ?? "").trim();
        if (a) { asked.push({ q, img, a }); return a; }
        last = "VL-ERROR empty completion";
      } else last = `VL-ERROR HTTP ${r.status}`;
    } catch (e) { last = `VL-ERROR ${String(e).slice(0, 120)}`; }
    if (i < retries) await Bun.sleep(5000 * i);
  }
  asked.push({ q, img, a: last });
  return last;
}

const isErr = (a: string) => a.startsWith("VL-ERROR");
const unusable = (a: string) => isErr(a) || /^\s*$|UNREADABLE|NO FRAME|NO CLAIM/i.test(a);

/**
 * Did the transcription ELIDE the entries?
 *
 * On this build a bar entry is a ~50-character pane-title path, and Mage-VL
 * routinely returns it shortened ("andual@andual: /tmp/vgate-f…"). The instance
 * name lives at the END of that path, so elision removes exactly the token every
 * structural criterion matches on — and the run-to-run coin flip between "read
 * it" and "elided it" made the same recording produce different verdicts.
 *
 * Elision is therefore reported as UNREADABLE-AT-THIS-LABEL-LENGTH rather than
 * as absence. It is a consequence of C1, and saying so keeps the verdict stable
 * and points the implementer at the one defect that actually has to be fixed.
 */
const elided = (a: string) => /(\.\.\.|…)/.test(a);

/**
 * Terse-negative recovery, mandated by the skill's Mage-VL failure modes:
 * a bare "No" to a pointed question is as often the model declining to look as
 * it is a real absence, and multiple-choice phrasing returns "none of the
 * above" spuriously. So a negative or empty answer is re-asked OPEN-ENDED
 * against the SAME frame and the richer answer is trusted.
 *
 * This was not theoretical: the first judged run returned "(b) No (c) No (d) No"
 * on a frame that plainly showed a bracketed selection and an indented child
 * row, and the gate turned that into three false FAILs.
 */
async function askRich(q: string, img: string): Promise<{ a: string; rich?: string }> {
  const a = await ask(q, img);
  const terse = a.replace(/\s+/g, " ").trim().length < 24 || /(^|\W)no($|\W)/i.test(a.slice(0, 12));
  const negative = /\bno\b/i.test(a) && a.replace(/\s+/g, " ").trim().length < 90;
  if (!unusable(a) && !terse && !negative) return { a };
  const rich = await ask(
    "Describe everything you see in this image strip in detail: every row, every label, " +
    "and any box, bracket, underline, highlight or indentation you can make out. " +
    "Quote text verbatim.", img);
  return { a, rich };
}
/**
 * Text to reason over.
 *
 * When the primary answer was unusable ("UNREADABLE") the rich re-ask REPLACES
 * it rather than being appended: concatenating the two leaves the word
 * UNREADABLE in the text, which the usability test then matches, and every
 * criterion resolves UNVERIFIABLE on a frame the model went on to describe in
 * detail. Otherwise the two are combined, since the pointed answer and the open
 * description each carry something the other does not.
 */
const body = (r: { a: string; rich?: string }) => {
  if (!r.rich || unusable(r.rich)) return r.a;
  return unusable(r.a) ? r.rich : `${r.a}\n${r.rich}`;
};

/** Extract a frame; `crop` is an ffmpeg crop expression or null for the full frame. */
export function frame(video: string, sec: number, out: string, crop: string | null) {
  const vf = crop ? `${crop},scale=iw*2:ih*2:flags=neighbor` : "scale=1280:-2";
  const r = spawnSync("ffmpeg", ["-y", "-v", "error", "-ss", sec.toFixed(2), "-i", video,
    "-frames:v", "1", "-vf", vf, "-q:v", "3", out]);
  return r.status === 0 && existsSync(out) ? out : null;
}

/**
 * The bar band crop.
 *
 * The bar sits just above the pane box. Rather than hardcode a y, the band is
 * given generously (the bottom fifth of the stage) and doubled in size, which
 * is what makes chip text legible to the VL at 1080p — the skill's note that
 * small text is otherwise "unverifiable at video resolution".
 */
// Horizontally bounded on purpose: the full-width strip also contains the
// harness-indicator plugin's chip grid on the right, and the VL spent its
// answer transcribing THAT ("[H], [F], [S] …") instead of the tidus bar. The
// crop is the judged region, so it must contain the judged thing and little
// else — which is also the cheap-prefill rule from the skill.
const BAR_CROP = process.env.VGATE_BAR_CROP ?? "crop=1150:200:170:875";
const CLOCK_CROP = "crop=520:60:0:0";

export type Verdict = "PASS" | "FAIL" | "UNVERIFIABLE";
export type Result = {
  id: string; title: string; verdict: Verdict; core: boolean; rejectOnFail: boolean;
  evidence: string[]; clues: string[];
};

const yes = (a: string) => /\b(yes|present|visible|it does|correct|true)\b/i.test(a);
const mentions = (a: string, s: string) => a.toLowerCase().includes(s.toLowerCase());

export async function judge(video: string, claimsPath: string, workdir: string) {
  mkdirSync(workdir, { recursive: true });
  const claims: Claim[] = readFileSync(claimsPath, "utf8").trim().split("\n").filter(Boolean).map((l) => JSON.parse(l));
  const dur = Number(spawnSync("ffprobe", ["-v", "error", "-show_entries", "format=duration",
    "-of", "csv=p=0", video], { encoding: "utf8" }).stdout.trim());

  /** Claim timestamps are scenario-relative; the recorder starts first, so map. */
  const at = (c: Claim) => Math.min(dur - 0.4, c.t + Number(process.env.VGATE_REC_LEAD ?? 2));
  const byPhase = (p: Phase | string) => claims.find((c) => c.phase === p);

  const results: Result[] = [];
  const add = (c: Criterion, verdict: Verdict, evidence: string[], clues: string[] = []) =>
    results.push({ id: c.id, title: c.title, verdict, core: c.core, rejectOnFail: c.rejectOnFail, evidence, clues });

  // ── coherence + clock, sampled across the whole run ───────────────────────
  {
    const c = CHECKLIST.find((x) => x.id === "C11-coherence")!;
    const ev: string[] = [], clues: string[] = [];
    const times = [0.15, 0.3, 0.45, 0.6, 0.75, 0.9].map((f) => +(dur * f).toFixed(1));
    let barDrawnEverywhere = true, unreadable = 0;
    for (const t of times) {
      const f = frame(video, t, `${workdir}/coh-${t}.jpg`, BAR_CROP);
      if (!f) { clues.push(`t=${t}s: frame extraction failed`); barDrawnEverywhere = false; continue; }
      const a = await ask(
        "This is a horizontal strip cropped from the bottom of a terminal application. " +
        "1) Is there a row of small agent/tab labels (short hyphenated names) visible in this strip? " +
        "2) Quote every short name you can read, verbatim, comma separated. " +
        "3) Is anything garbled, overlapping, blank, or is an error dialog covering it? " +
        "If you cannot read any text say UNREADABLE.", f);
      if (isErr(a)) { clues.push(`t=${t}s: ${a}`); unreadable++; continue; }
      ev.push(`t=${t}s → ${a.replace(/\s+/g, " ").slice(0, 220)}`);
      if (/UNREADABLE/i.test(a)) unreadable++;
      // A keyword alone is not a report of a defect: the model answers the
      // question by REPEATING it ("No, there is nothing garbled, overlapping,
      // blank..."), and an earlier version matched the word and flagged three
      // clean frames. Only an un-negated mention counts.
      const complaint = a.split(/[.\n]/).find((sent) =>
        /garbled|overlap|blank|error dialog/i.test(sent) && !/\b(no|not|none|nothing|without)\b/i.test(sent));
      if (complaint) { clues.push(`t=${t}s: reported "${complaint.trim().slice(0, 120)}"`); barDrawnEverywhere = false; }
    }
    // clock monotonicity from the burned watermark
    const clockReads: string[] = [];
    for (const t of [dur * 0.2, dur * 0.5, dur * 0.8].map((v) => +v.toFixed(1))) {
      const f = frame(video, t, `${workdir}/clk-${t}.jpg`, CLOCK_CROP);
      if (!f) continue;
      const a = await ask("Read the text in this small strip verbatim. It contains a label and a time. " +
        "If unreadable say UNREADABLE.", f);
      clockReads.push(`t=${t}s → ${a.replace(/\s+/g, " ").slice(0, 120)}`);
    }
    ev.push(...clockReads);
    const v: Verdict = unreadable >= times.length ? "UNVERIFIABLE" : barDrawnEverywhere ? "PASS" : "FAIL";
    add(c, v, ev, clues);
  }

  // ── per-phase bar reads, reused across the name/structure criteria ────────
  const reads: Record<string, { frame: string | null; answer: string; claim?: Claim }> = {};
  for (const phase of ["root-selected", "alpha-selected", "a1-selected", "click-brother", "click-ancestor", "right-click", "close-child"] as Phase[]) {
    const cl = byPhase(phase);
    if (!cl) { reads[phase] = { frame: null, answer: "NO CLAIM RECORDED" }; continue; }
    const f = frame(video, at(cl), `${workdir}/${phase}.jpg`, BAR_CROP);
    // ONE open read per phase, reused by every criterion. Open-ended on purpose:
    // the skill is explicit that multiple-choice phrasing makes Mage-VL answer
    // "none of the above" on frames that plainly show the thing.
    const r = f ? await askRich(
      "This strip is cropped from a terminal application and shows an agent bar. " +
      "Transcribe it row by row: for EACH row, quote every entry you can read VERBATIM, in reading order, " +
      "separated by ' | '. Note which rows are indented relative to the row above, and note any entry that is " +
      "bracketed, boxed, underlined or highlighted. Do not summarise — quote the literal text of each entry. " +
      "If you cannot read the strip at all, say UNREADABLE.", f) : { a: "NO FRAME" };
    reads[phase] = { frame: f, answer: body(r), claim: cl };
  }

  const readOf = (p: string) => reads[p]?.answer ?? "";
  const frameOf = (p: string) => reads[p]?.frame ?? "(no frame)";

  // ── C1 names, not harness:jobid and not a pane title ──────────────────────
  //
  // Substring matching is NOT enough here and the first run proved it: the bar
  // drew "andual@andual: /tmp/vgate-fleet/vg-alpha/workspace", the check asked
  // "does the answer contain vg-alpha", and a pane title containing the name as
  // a path segment passed a criterion it plainly violates. An entry must be the
  // instance name ITSELF — so any entry carrying a path, a user@host, or a
  // colon-plus-digits tail fails, whatever else it also contains.
  {
    const c = CHECKLIST.find((x) => x.id === "C1-names")!;
    const ev: string[] = [], clues: string[] = [];
    let clean = 0, dirty = 0, unreadable = 0;
    for (const p of ["root-selected", "alpha-selected", "a1-selected"]) {
      const a = readOf(p), cl = byPhase(p);
      // The collector's own claim, cross-checked: a display_name that differs
      // from the harness name is exactly what the renderer prefers to draw.
      const mismatched = (cl?.bar?.chips ?? [])
        .filter((x: any) => x.display_name && x.display_name !== x.name)
        .map((x: any) => `${x.name} → "${x.display_name}"`);
      if (mismatched.length)
        clues.push(`${p}: collector emits display_name ≠ name for ${mismatched.length} chip(s), e.g. ${mismatched[0]}`);
      if (unusable(a)) { unreadable++; clues.push(`${p}: ${a.slice(0, 90)}`); continue; }
      // NB: C1 itself still judges an elided read — an entry shortened to
      // "andual@andual: /tmp/vgate-f…" is already proof it is not a bare name.
      ev.push(`${p} (${frameOf(p)}) → ${a.replace(/\s+/g, " ").slice(0, 340)}`);
      // Entries as the VL quoted them, split on the separator it was asked for.
      const entries = a.split("\n").flatMap((l) => l.split("|")).map((s) => s.trim()).filter(Boolean);
      const bad = entries.filter((e) => /vg-/i.test(e) && /[/@]|:\s*\d/.test(e));
      if (bad.length) {
        dirty++;
        clues.push(`${p}: entries are not instance names — e.g. ${JSON.stringify(bad[0].slice(0, 80))}`);
      } else if (entries.some((e) => /^\W*vg-[a-z0-9]+\W*$/i.test(e))) clean++;
      else clues.push(`${p}: no entry read as a bare instance name`);
    }
    add(c, unreadable === 3 ? "UNVERIFIABLE" : dirty > 0 ? "FAIL" : clean > 0 ? "PASS" : "FAIL", ev, clues);
  }

  /**
   * Lines of the transcription that look like a transcribed ROW of the bar.
   * The crop unavoidably catches the version-dash strip above the bar and the
   * pane's own title line below it; both mention the fleet path, so a bare
   * /vg-/ filter picked "harness shell · /tmp/vgate-fleet/…" as the child row.
   */
  const NOT_A_BAR_ROW = /harness shell|\b0\.\d+\.\d+|latest\b|no focused agent/i;
  const rowLines = (a: string) =>
    a.split("\n").map((l) => l.trim()).filter((l) => /vg-/i.test(l) && !NOT_A_BAR_ROW.test(l));
  /** Individual chip entries, however the VL separated them. */
  const entriesOf = (a: string) =>
    rowLines(a).flatMap((l) => l.split(/\s[·|▸]\s|\s{2,}/)).map((e) => e.trim()).filter(Boolean);
  /** The row the VL flagged as indented / lowest — the child row. */
  const childRowLine = (a: string) => {
    const rows = rowLines(a);
    const flagged = rows.filter((l) => /indent|below|beneath|under|third row|last row|child/i.test(l));
    return (flagged.length ? flagged[flagged.length - 1] : rows[rows.length - 1]) ?? "";
  };

  // ── C2 selected indicator ─────────────────────────────────────────────────
  {
    const c = CHECKLIST.find((x) => x.id === "C2-selected-indicator")!;
    const ev: string[] = [], clues: string[] = [];
    let ok = 0, unreadable = 0;
    for (const [p, want] of [["root-selected", "vg-root"], ["alpha-selected", "vg-alpha"], ["a1-selected", "vg-a1"]] as const) {
      const a = readOf(p);
      if (unusable(a) || elided(a)) { unreadable++; if (elided(a)) clues.push(`${p}: entries elided by the transcriber ("…") — unreadable at this label length, see C1`); continue; }
      // Any line where the VL reported a marker AND named the expected agent.
      // Per ENTRY, not per line: the VL often returns the whole bar as one
      // line, so a line-level test cannot say WHICH entry carried the marker.
      const marked = [...entriesOf(a), ...a.split("\n")]
        .filter((l) => /bracket|box|underlin|highlight|selected|cartouche|▌|▐|\[/i.test(l));
      const hit = marked.find((l) => mentions(l, want));
      ev.push(`${p}: expected marked=${want} → ${(hit ?? marked[0] ?? a).replace(/\s+/g, " ").slice(0, 220)}`);
      if (hit) ok++;
      else clues.push(`${p} (${frameOf(p)}): no line reported ${want} as bracketed/boxed/highlighted — VL said "${a.replace(/\s+/g, " ").slice(0, 160)}"`);
    }
    add(c, unreadable === 3 ? "UNVERIFIABLE" : ok === 3 ? "PASS" : "FAIL", ev, clues);
  }

  // ── C3 children under the SELECTED node ───────────────────────────────────
  {
    const c = CHECKLIST.find((x) => x.id === "C3-children-under-selected")!;
    const ev: string[] = [], clues: string[] = [];
    let ok = 0, unreadable = 0;
    for (const [p, kids, notKids] of [
      ["root-selected", ["vg-alpha", "vg-beta"], ["vg-a1", "vg-a2"]],
      ["alpha-selected", ["vg-a1", "vg-a2"], ["vg-beta"]],
    ] as const) {
      const a = readOf(p);
      if (unusable(a) || elided(a)) { unreadable++; if (elided(a)) clues.push(`${p}: entries elided by the transcriber ("…") — unreadable at this label length, see C1`); continue; }
      const row = childRowLine(a);
      ev.push(`${p}: expected child row [${kids.join(", ")}] → ${row.replace(/\s+/g, " ").slice(0, 240)}`);
      const present = kids.filter((k) => mentions(row, k));
      const wrong = notKids.filter((k) => mentions(row, k));
      if (present.length === kids.length && wrong.length === 0) ok++;
      else clues.push(`${p} (${frameOf(p)}): child row should be [${kids}] — read "${row.replace(/\s+/g, " ").slice(0, 160)}"` +
        (wrong.length ? ` (contains ${wrong.join(",")}, which belongs to another level)` : "") +
        (present.length < kids.length ? ` (missing ${kids.filter((k) => !present.includes(k)).join(",")})` : ""));
    }
    add(c, unreadable === 2 ? "UNVERIFIABLE" : ok === 2 ? "PASS" : "FAIL", ev, clues);
  }

  // ── C4 children COMPLETE vs the pane list ─────────────────────────────────
  {
    const c = CHECKLIST.find((x) => x.id === "C4-children-complete")!;
    const ev: string[] = [], clues: string[] = [];
    const expected: Record<string, string[]> = { "root-selected": ["vg-alpha", "vg-beta"], "alpha-selected": ["vg-a1", "vg-a2"] };
    let ok = 0, unreadable = 0;
    for (const p of Object.keys(expected)) {
      const cl = byPhase(p); const a = readOf(p);
      const truth = expected[p].filter((n) => cl?.panesPresent?.[n]);
      const drawnClaim = (cl?.bar?.chips ?? []).filter((x: any) => x.role === "child").map((x: any) => x.name);
      ev.push(`${p}: pane-list ground truth = [${truth}]; collector claims child row = [${drawnClaim}]`);
      if (unusable(a) || elided(a)) { unreadable++; if (elided(a)) clues.push(`${p}: entries elided ("…") — unreadable at this label length, see C1`); continue; }
      const dAns = childRowLine(a);
      const missingOnScreen = truth.filter((n) => !mentions(dAns, n));
      ev.push(`${p} video read (child row) → ${dAns.replace(/\s+/g, " ").slice(0, 220)}`);
      if (missingOnScreen.length === 0) ok++;
      else clues.push(`${p} (${frameOf(p)}): children present as panes but NOT read on the child row: ${missingOnScreen.join(", ")}`);
      const missingInClaim = truth.filter((n) => !drawnClaim.includes(n));
      if (missingInClaim.length) clues.push(`${p}: collector itself omitted ${missingInClaim.join(", ")} from the child row`);
    }
    add(c, unreadable === 2 ? "UNVERIFIABLE" : ok === 2 ? "PASS" : "FAIL", ev, clues);
  }

  // ── C5 brothers stay ──────────────────────────────────────────────────────
  {
    const c = CHECKLIST.find((x) => x.id === "C5-brothers-stay")!;
    const ev: string[] = [], clues: string[] = [];
    let ok = 0, unreadable = 0;
    for (const [p, brother] of [["alpha-selected", "vg-beta"], ["a1-selected", "vg-a2"], ["click-brother", "vg-a1"]] as const) {
      const a = readOf(p);
      if (unusable(a) || elided(a)) { unreadable++; if (elided(a)) clues.push(`${p}: entries elided ("…") — unreadable at this label length, see C1`); continue; }
      ev.push(`${p}: brother ${brother} must be on screen → ${a.replace(/\s+/g, " ").slice(0, 200)}`);
      if (mentions(a, brother)) ok++;
      else clues.push(`${p} (${frameOf(p)}): brother ${brother} was NOT read anywhere on the bar`);
    }
    add(c, unreadable === 3 ? "UNVERIFIABLE" : ok === 3 ? "PASS" : "FAIL", ev, clues);
  }

  // ── C6 lineage complete at depth / C7 recursion ───────────────────────────
  for (const [id, wants] of [["C6-lineage-complete", ["vg-root", "vg-alpha"]], ["C7-recursion-depth", ["vg-root", "vg-alpha", "vg-a1"]]] as const) {
    const c = CHECKLIST.find((x) => x.id === id)!;
    const a = readOf("a1-selected");
    const ev = [`a1-selected (${frameOf("a1-selected")}) → ${a.replace(/\s+/g, " ").slice(0, 320)}`];
    if (unusable(a) || elided(a)) { add(c, "UNVERIFIABLE", ev, [elided(a) ? "a1-selected: entries elided (\"…\") — unreadable at this label length, see C1" : `a1-selected: ${a.slice(0, 90)}`]); continue; }
    const missing = wants.filter((w) => !mentions(a, w));
    add(c, missing.length ? "FAIL" : "PASS", ev,
      missing.length ? [`with vg-a1 selected at depth 2, these were not read on the bar: ${missing.join(", ")}`] : []);
  }

  // ── C8 click navigates to THAT INSTANCE's pane ────────────────────────────
  {
    const c = CHECKLIST.find((x) => x.id === "C8-click-navigates-to-instance")!;
    const ev: string[] = [], clues: string[] = [];
    let ok = 0, attempted = 0, inert = 0;
    for (const p of ["click-brother", "click-ancestor"]) {
      const cl = byPhase(p); const act = cl?.action;
      if (!act) { clues.push(`${p}: no click was recorded`); continue; }
      attempted++;
      if (act.error) { clues.push(`${p}: ${act.error} — the chip could not be located on screen, so the click was never delivered`); continue; }
      ev.push(`${p}: clicked ${act.chip} at (${act.target?.x},${act.target?.y}); want tab=${act.want?.tab_id} pane=${act.want?.pane_id} term=${act.want?.terminal_id}; got tab=${act.got?.tab_id} pane=${act.got?.pane_id} term=${act.got?.terminal_id}`);
      // Three outcomes, not two. Landing on ANOTHER instance is the defect this
      // criterion exists to catch. Landing nowhere at all is indistinguishable
      // from a click that never reached the bar, and reporting the gate's own
      // aim as a product defect is worse than reporting nothing — so it is
      // INSTRUMENT-LIMITED and says so.
      // An AMBIGUOUS target that navigated somewhere else proves the bar is
      // live but proves nothing about which chip was hit — the aim, not the
      // bar, is the unknown. Only a chip located unambiguously can convict.
      if (act.landed) ok++;
      else if (act.target?.ambiguous && act.before && act.got?.tab_id !== act.before.tab_id) {
        inert++;
        clues.push(
          `${p}: click at (${act.target?.x},${act.target?.y}) DID navigate (${act.before.tab_id} → ${act.got?.tab_id}), ` +
          `so the bar is live and clickable — but the chip was targeted by even division of the row ` +
          `(${act.target?.clusters} ink cluster(s) for ${"" + (act.target?.expected ?? "n")} chips), so this cannot be ` +
          `attributed to a specific entry. INSTRUMENT-LIMITED, not a navigation defect.`);
      }
      else if (act.before && act.got?.tab_id === act.before.tab_id) {
        inert++;
        clues.push(
          `${p}: click at (${act.target?.x},${act.target?.y}) changed nothing — focus stayed on ${act.before.tab_id}. ` +
          `INSTRUMENT-LIMITED: a click that moves nothing cannot be told apart from a click that missed the bar, ` +
          `so this is NOT reported as a navigation defect. Confirm the target row before trusting it.`);
      }
      else clues.push(
        `${p}: click on ${act.chip} at (${act.target?.x},${act.target?.y}) focused ${act.got?.tab_id ?? "nothing"} ` +
        `(pane ${act.got?.pane_id}), NOT that instance's own pane ${act.want?.tab_id}/${act.want?.pane_id}` +
        (act.target?.ambiguous
          ? ` — NOTE: the chip was targeted by equal-division fallback (${act.target?.clusters} ink clusters vs the ` +
            `expected chip count), so this failure may be the gate's aim rather than the bar's navigation`
          : ` (targeted on a clean ${act.target?.clusters}-cluster row)`));
      // video corroboration that the click was visibly performed
      const a = readOf(p);
      if (!unusable(a)) ev.push(`${p} bar after click → ${a.replace(/\s+/g, " ").slice(0, 200)}`);
    }
    add(c, attempted === 0 || (ok === 0 && inert === attempted) ? "UNVERIFIABLE"
         : ok === attempted ? "PASS" : "FAIL", ev, clues);
  }

  // ── C9 right-click reports rather than navigating ─────────────────────────
  {
    const c = CHECKLIST.find((x) => x.id === "C9-context-menu")!;
    const cl = byPhase("right-click"); const act = cl?.action;
    const a = readOf("right-click");
    const ev = [`right-click claim → ${JSON.stringify(act ?? null).slice(0, 240)}`,
      `bar after right-click (${frameOf("right-click")}) → ${a.replace(/\s+/g, " ").slice(0, 240)}`];
    const clues: string[] = [];
    if (!act) add(c, "UNVERIFIABLE", ev, ["no right-click recorded"]);
    else if (act.error) add(c, "UNVERIFIABLE", ev, [
      `${act.error} — the gesture was never delivered, so this says nothing about the bar. ` +
      `INSTRUMENT-LIMITED: chip targeting is blocked by C1 (entries are truncated pane-title paths).`]);
    else {
      if (act.navigated) clues.push(`right-click NAVIGATED (focus moved ${act.before?.tab_id} → ${act.after?.tab_id}); it must report instead`);
      const menuish = !isErr(a) && /menu|report|option|detail|status|edge/i.test(a);
      if (!menuish) clues.push("no report/menu surface was read on the bar strip after the right-click");
      add(c, act.navigated ? "FAIL" : menuish ? "PASS" : "UNVERIFIABLE", ev, clues);
    }
  }

  // ── C10 close child returns to parent ─────────────────────────────────────
  {
    const c = CHECKLIST.find((x) => x.id === "C10-close-returns-to-parent")!;
    const cl = byPhase("close-child"); const act = cl?.action;
    const a = readOf("close-child");
    const ev = [`close claim → ${JSON.stringify(act ?? null).slice(0, 240)}`,
      `bar after close (${frameOf("close-child")}) → ${a.replace(/\s+/g, " ").slice(0, 240)}`];
    const clues: string[] = [];
    if (!act) add(c, "UNVERIFIABLE", ev, ["no close recorded"]);
    else {
      const wentToParent = act.currentAfter === "vg-alpha";
      if (!wentToParent) clues.push(`after closing vg-a1 the collector's selection was ${act.currentAfter ?? "none"}, expected the parent vg-alpha`);
      const videoAgrees = !isErr(a) && mentions(a, "vg-alpha");
      if (!videoAgrees) clues.push("vg-alpha was not read on the bar after the close");
      add(c, wentToParent && videoAgrees ? "PASS" : wentToParent || videoAgrees ? "FAIL" : "FAIL", ev, clues);
    }
  }

  // ── claim cross-check ─────────────────────────────────────────────────────
  //
  // Doctrine: claims are cross-checked, never trusted. Where the VIDEO could not
  // evidence a criterion but the collector's own snapshot satisfies it, that
  // DISAGREEMENT is the finding and must be stated — the bar may be modelling
  // the tree correctly while rendering it unreadably, and a report that leaves
  // that ambiguous sends the implementer hunting the wrong bug.
  const structural: Record<string, (c: Claim) => boolean> = {
    "C3-children-under-selected": (c) => {
      const kids = (c.bar?.chips ?? []).filter((x: any) => x.role === "child").map((x: any) => x.name);
      return c.phase === "root-selected"
        ? kids.includes("vg-alpha") && kids.includes("vg-beta")
        : kids.includes("vg-a1") && kids.includes("vg-a2");
    },
    "C4-children-complete": (c) => (c.bar?.chips ?? []).some((x: any) => x.role === "child"),
    "C5-brothers-stay": (c) => (c.bar?.chips ?? []).some((x: any) => x.role === "sibling"),
    "C7-recursion-depth": (c) => (c.bar?.chips ?? []).filter((x: any) => x.role === "ancestor").length >= 2,
  };
  for (const r of results) {
    const pred = structural[r.id];
    if (!pred || r.verdict === "PASS") continue;
    const satisfied = claims.filter((c) => c.bar?.chips?.length).filter(pred).map((c) => c.phase);
    if (satisfied.length)
      r.clues.push(
        `CLAIM CROSS-CHECK: the collector's own snapshot DOES satisfy this at [${satisfied.join(", ")}]. ` +
        `The failure above is that the VIDEO could not evidence it — which on this build follows from C1: ` +
        `entries are ~50-character pane-title paths, so a frame transcription truncates before the row is read. ` +
        `Fix the labels and re-run before treating this as a tree-model defect.`);
  }

  // ── verdict ───────────────────────────────────────────────────────────────
  const blocking = results.filter(
    (r) => (r.verdict === "FAIL" && r.rejectOnFail) || (r.verdict === "UNVERIFIABLE" && r.core));
  const verdict = blocking.length === 0 ? "ACCEPT" : "REJECT";

  const md: string[] = [];
  md.push(`# vl-gate report — tidus-bar-ui`, "");
  md.push(`VERDICT: ${verdict}`, "");
  md.push(`Video: \`${video}\` (${dur.toFixed(1)}s). Reviewer vision: Mage-VL only (${VL_MODEL} @ ${VL_ENDPOINT}).`);
  md.push(`${asked.length} vision queries. Claims cross-checked from \`${claimsPath}\`.`, "");
  md.push(`| criterion | verdict | blocking |`, `|---|---|---|`);
  for (const r of results)
    md.push(`| ${r.id} ${r.title} | ${r.verdict} | ${r.rejectOnFail ? (r.core ? "yes (core)" : "yes") : "no"} |`);
  md.push("");
  for (const r of results) {
    md.push(`## ${r.id} — ${r.title}`, "", `**${r.verdict}**`, "");
    if (r.evidence.length) { md.push("Evidence:", ""); for (const e of r.evidence) md.push(`- ${e}`); md.push(""); }
    if (r.clues.length) { md.push("Clues:", ""); for (const e of r.clues) md.push(`- ${e}`); md.push(""); }
  }
  md.push("## Collector claims per phase (cross-check, not truth)", "");
  md.push("| phase | focus | ancestors | current | siblings | children |", "|---|---|---|---|---|---|");
  for (const c of claims) {
    const by = (r: string) => (c.bar?.chips ?? []).filter((x: any) => x.role === r).map((x: any) => x.name).join(" ") || "—";
    md.push(`| ${c.phase} | ${c.bar?.focus_harness ?? "—"} | ${by("ancestor")} | ${by("current")} | ${by("sibling")} | ${by("child")} |`);
  }
  md.push("");
  md.push("## Vision transcript", "");
  for (const a of asked) md.push(`- \`${a.img}\` — ${a.q.slice(0, 90)}…\n  > ${a.a.replace(/\s+/g, " ").slice(0, 400)}`);
  writeFileSync(`${workdir}/gate-report.md`, md.join("\n"));
  writeFileSync(`${workdir}/gate-report.json`, JSON.stringify({ verdict, video, results, asked }, null, 2));

  console.log(`\nVERDICT: ${verdict}`);
  for (const r of results) console.log(`  ${r.verdict.padEnd(13)} ${r.id} ${r.title}`);
  if (blocking.length) {
    console.log("\nBlocking:");
    for (const r of blocking) { console.log(`  ${r.id}:`); for (const c of r.clues) console.log(`    - ${c}`); }
  }
  console.log(`\nreport → ${workdir}/gate-report.md`);
  return { verdict, results };
}

if (import.meta.main) {
  const video = process.argv[2], claims = process.argv[3], work = process.argv[4] ?? "/tmp/vgate/judge";
  if (!video || !claims) { console.error("usage: bun judge.ts <video.mp4> <claims.jsonl> [workdir]"); process.exit(2); }
  const { verdict } = await judge(video, claims, work);
  process.exit(verdict === "ACCEPT" ? 0 : 1);
}
