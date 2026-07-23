#!/usr/bin/env bun
// resume-picker — full-screen-in-pane TUI that searches every Claude-family
// transcript on this machine and, on Enter/click, prints a resume command to
// stdout for a thin wrapper to `exec` in the SAME pane.
//
// Rendering goes to STDERR (which stays attached to the tty even when stdout is
// captured by the launching `$(...)`), input is read raw from STDIN, and the
// single line of stdout is the resolved resume command (fzf model). This is what
// lets the picker hand its own pane to the resumed session.
//
// Three sections (exact / similar / gpt) stream INDEPENDENTLY: each renders the
// moment its searcher yields, pending sections show a spinner, and a slow
// section never blocks a fast one. A new keystroke aborts all in-flight
// searchers via a shared AbortSignal. S2/S3 are optional plug-in modules — if
// absent or erroring, that section shows a visible degradation line.

import type { ResultLine, SectionId, SectionSearcher } from "../src/types.ts";
import { isSentinel } from "../src/types.ts";
import { loadConfig, MetaCache } from "../src/sessions.ts";
import { makeExactSearcher } from "../src/s1-exact.ts";
import { resolveResume } from "../src/resume.ts";

const DEBOUNCE_MS = 1000;
const SPINNER = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

type SectionStatus = "idle" | "searching" | "done" | "degraded";

interface SectionState {
  id: SectionId;
  label: string;
  status: SectionStatus;
  lines: ResultLine[];
  note: string; // degradation / status text
  searcher: SectionSearcher | null;
}

const out = process.stderr; // the tty
const cols = () => out.columns ?? 80;
const rows = () => out.rows ?? 24;

// --- global state -----------------------------------------------------------
let query = "";
let cursor = 0; // caret position within query
let selected = 0; // index into the flat result list
let generation = 0;
let controller: AbortController | null = null;
let debounceTimer: ReturnType<typeof setTimeout> | null = null;
let spinnerFrame = 0;
let spinnerTimer: ReturnType<typeof setInterval> | null = null;
let toast = "";
let expanded = false; // F5: is the selected row's compaction chain expanded?

const config = loadConfig();
const metaCache = new MetaCache(config);

const sections: Record<SectionId, SectionState> = {
  exact: { id: "exact", label: "exact", status: "idle", lines: [], note: "", searcher: makeExactSearcher(metaCache) },
  similar: { id: "similar", label: "similar", status: "idle", lines: [], note: "", searcher: null },
  gpt: { id: "gpt", label: "gpt", status: "idle", lines: [], note: "", searcher: null },
};
const ORDER: SectionId[] = ["exact", "similar", "gpt"];

// Map screen row -> flat result index, rebuilt every render for mouse hit-test.
let rowHitMap = new Map<number, number>();

// --- optional plug-in searchers ---------------------------------------------
async function loadPlugin(id: SectionId, modPath: string): Promise<void> {
  const s = sections[id];
  try {
    const mod: any = await import(modPath);
    const cand = mod.search ?? mod.default ?? (typeof mod.makeSearcher === "function" ? mod.makeSearcher(metaCache) : null);
    if (typeof cand === "function") {
      s.searcher = cand as SectionSearcher;
    } else {
      s.status = "degraded";
      s.note = `${id}: module has no searcher export`;
    }
  } catch {
    s.status = "degraded";
    s.note = id === "similar" ? "similar: no local index module" : "gpt: module unavailable";
  }
}

// --- search orchestration ---------------------------------------------------
function scheduleSearch(): void {
  // Cancel in-flight work immediately on every keystroke.
  if (controller) controller.abort();
  if (debounceTimer) clearTimeout(debounceTimer);
  for (const id of ORDER) {
    const s = sections[id];
    s.lines = [];
    s.status = s.searcher ? "idle" : "degraded";
    if (!s.searcher && !s.note) s.note = `${id}: unavailable`;
  }
  selected = 0;
  expanded = false;
  if (!query.trim()) {
    controller = null;
    render();
    return;
  }
  debounceTimer = setTimeout(fireSearch, DEBOUNCE_MS);
  render();
}

// S3 (gpt) is an ESCALATION, not a per-keystroke search: firing the Codex/luna
// route on every debounce burns quota and is slow. Fire exact + similar
// immediately; only auto-fire gpt once BOTH local sections finish with zero
// selectable results, or when the user presses Tab ("search deeper").
const LOCAL: SectionId[] = ["exact", "similar"];

function fireSearch(): void {
  debounceTimer = null; // the debounce has elapsed; clear it so the spinner stops
  const gen = ++generation;
  const ctl = new AbortController();
  controller = ctl;
  const q = query;
  for (const id of LOCAL) {
    const s = sections[id];
    if (!s.searcher) continue;
    s.status = "searching";
    runSection(s, q, ctl.signal, gen);
  }
  // gpt idles until escalation.
  const gpt = sections.gpt;
  if (gpt.searcher) {
    gpt.status = "idle";
    gpt.note = "gpt: press Tab to search deeper";
  }
  render();
}

/** Count selectable (non-sentinel) results currently held by a section. */
function selectableCount(id: SectionId): number {
  return sections[id].lines.filter((l) => !isSentinel(l)).length;
}

/** Fire the gpt escalation for the current query/generation (Tab or auto). */
function fireGpt(): void {
  const gpt = sections.gpt;
  if (!gpt.searcher || !controller || !query.trim()) return;
  if (gpt.status === "searching") return;
  gpt.lines = [];
  gpt.status = "searching";
  runSection(gpt, query, controller.signal, generation);
  render();
}

/** Auto-escalate to gpt when both local sections finished empty. */
function maybeAutoEscalate(): void {
  const gpt = sections.gpt;
  if (!gpt.searcher || gpt.status === "searching" || gpt.status === "done") return;
  const localDone = LOCAL.every((id) => {
    const st = sections[id].status;
    return st === "done" || st === "degraded";
  });
  if (!localDone) return;
  const localHits = LOCAL.reduce((n, id) => n + selectableCount(id), 0);
  if (localHits === 0) fireGpt();
}

async function runSection(s: SectionState, q: string, signal: AbortSignal, gen: number): Promise<void> {
  try {
    for await (const line of s.searcher!(q, signal)) {
      if (gen !== generation) return; // stale (query changed)
      s.lines.push(line);
      render();
    }
    if (gen === generation) {
      s.status = "done";
      if (s.lines.length === 0) s.note = `${s.id}: no matches`;
      if (s.id !== "gpt") maybeAutoEscalate();
      render();
    }
  } catch (e) {
    if (gen === generation) {
      s.status = "degraded";
      s.note = `${s.id}: ${(e as Error).message}`;
      if (s.id !== "gpt") maybeAutoEscalate();
      render();
    }
  }
}

// --- flat result list (selection crosses section boundaries) ----------------
function flatResults(): { section: SectionId; line: ResultLine }[] {
  const flat: { section: SectionId; line: ResultLine }[] = [];
  // Sentinels (degradation/status lines, sessionId==="") are rendered but never
  // selectable, so they are excluded from the flat selection list.
  for (const id of ORDER) for (const line of sections[id].lines) if (!isSentinel(line)) flat.push({ section: id, line });
  return flat;
}

// --- rendering --------------------------------------------------------------
function agoSuffix(r: ResultLine): string {
  const parts: string[] = [r.shortId];
  if (r.lastUserAgo) parts.push(`you ${r.lastUserAgo}`);
  if (r.lastAgentAgo) parts.push(`ai ${r.lastAgentAgo}`);
  if (r.createdAgo) parts.push(`created ${r.createdAgo}`);
  return parts.join(" · ");
}

function formatResult(r: ResultLine, width: number): string {
  const suffix = agoSuffix(r);
  const sep = "  ";
  const room = Math.max(8, width - suffix.length - sep.length - 2); // 2 = left gutter
  let snip = r.snippet;
  if (snip.length > room) snip = snip.slice(0, room - 1) + "…";
  const pad = Math.max(0, width - 2 - snip.length - sep.length - suffix.length);
  return snip + sep + " ".repeat(pad) + suffix;
}

function render(): void {
  const width = cols();
  const height = rows();
  const flat = flatResults();
  if (selected >= flat.length) selected = Math.max(0, flat.length - 1);

  const lines: string[] = [];
  // Header / search bar.
  const spin =
    ORDER.some((id) => sections[id].status === "searching") || (debounceTimer && query.trim())
      ? ` [${SPINNER[spinnerFrame % SPINNER.length]}]`
      : "";
  lines.push(clip(`┌ resume ${"─".repeat(Math.max(0, width - 10))}`, width));
  lines.push(clip(`│ > ${query}${spin}`, width));

  let flatIdx = 0;
  rowHitMap = new Map();
  for (const id of ORDER) {
    const s = sections[id];
    lines.push(clip(`── ${s.label} ${"─".repeat(Math.max(0, width - s.label.length - 4))}`, width));
    if (s.lines.length === 0) {
      if (s.status === "searching" || (s.status === "idle" && debounceTimer && query.trim())) {
        lines.push(clip(`  ${SPINNER[spinnerFrame % SPINNER.length]} searching…`, width));
      } else if (s.status === "degraded") {
        lines.push(clip(`  ⚠ ${s.note}`, width));
      } else if (s.status === "done") {
        lines.push(clip(`  ${s.note || "no matches"}`, width));
      } else if (s.status === "idle" && s.note) {
        // e.g. "gpt: press Tab to search deeper" — tells the user how to escalate.
        lines.push(clip(`  ${s.note}`, width));
      } else {
        lines.push(clip(`  —`, width));
      }
    } else {
      for (const line of s.lines) {
        if (isSentinel(line)) {
          // Status/degradation row: shown but not selectable, no hit-map entry.
          lines.push(clip(`  ${line.snippet}`, width));
          continue;
        }
        const isSel = flatIdx === selected;
        const body = formatResult(line, width);
        const marker = isSel ? "›" : " ";
        const row = lines.length; // 0-based screen row for hit map
        rowHitMap.set(row, flatIdx);
        const text = `${marker} ${body}`;
        lines.push(isSel ? `\x1b[7m${clip(text, width)}\x1b[0m` : clip(text, width));
        flatIdx++;
      }
    }
  }
  // F3: detail area for the SELECTED row — first & last user prompt so Andres
  // can recognise the conversation. F5: when expanded, list the compaction chain.
  const sel = flat[selected];
  if (sel && !isSentinel(sel.line)) {
    const r = sel.line;
    lines.push(clip("─".repeat(width), width));
    const first = r.firstPrompt || "(no first prompt)";
    const last = r.lastPrompt || first;
    lines.push(clip(`  first ┃ ${first}`, width));
    lines.push(clip(`  last  ┃ ${last}`, width));
    if (r.chain.length > 0) {
      if (expanded) {
        lines.push(clip(`  chain (${r.chain.length} checkpoint${r.chain.length === 1 ? "" : "s"}):`, width));
        for (const seg of r.chain) {
          const mark = seg.kind === "segment" ? "»" : "·";
          lines.push(clip(`    ${mark} ${seg.ago.padStart(3)} ago  ${seg.label}`, width));
        }
      } else {
        lines.push(clip(`  → expand ${r.chain.length} compaction checkpoint${r.chain.length === 1 ? "" : "s"}`, width));
      }
    }
  }
  if (toast) lines.push(clip(`  ${toast}`, width));
  // Footer key hints.
  lines.push(clip("  ↑↓ select · → chain · Tab deeper · Enter resume · Esc close", width));

  // Clamp to terminal height; keep the search bar + selected row visible.
  const maxBody = height - 1;
  const view = lines.slice(0, maxBody);

  // Paint: home cursor, clear screen, write lines.
  let buf = "\x1b[H\x1b[2J";
  buf += view.join("\r\n");
  // Park caret at the search bar position.
  buf += `\x1b[2;${4 + cursor + 1}H`;
  out.write(buf);
}

function clip(s: string, width: number): string {
  // Naive width clamp (assumes mostly single-width). Good enough for a picker.
  if (s.length <= width) return s;
  return s.slice(0, width);
}

// --- input ------------------------------------------------------------------
function enterAltScreen(): void {
  out.write("\x1b[?1049h"); // alt screen
  out.write("\x1b[?25l"); // hide cursor while painting
  out.write("\x1b[?1000h\x1b[?1006h"); // SGR mouse
}
function leaveAltScreen(): void {
  out.write("\x1b[?1000l\x1b[?1006l");
  out.write("\x1b[?25h");
  out.write("\x1b[?1049l");
}

function cleanupAndExit(command: string | null, code: number): never {
  if (spinnerTimer) clearInterval(spinnerTimer);
  if (debounceTimer) clearTimeout(debounceTimer);
  if (controller) controller.abort();
  try {
    process.stdin.setRawMode(false);
  } catch {}
  leaveAltScreen();
  metaCache.flush();
  if (command) process.stdout.write(command + "\n");
  process.exit(code);
}

function doResume(): void {
  const flat = flatResults();
  const pick = flat[selected];
  if (!pick) return;
  const res = resolveResume(pick.line.resumeRef);
  if (res.command) {
    cleanupAndExit(res.command, 0);
  } else {
    toast = res.reason ?? "not resumable";
    render();
  }
}

function moveSelection(delta: number): void {
  const n = flatResults().length;
  if (n === 0) return;
  selected = (selected + delta + n) % n;
  expanded = false; // collapse the chain when moving to a different row
  render();
}

function handleMouse(seq: string): void {
  // SGR: ESC [ < b ; x ; y (M|m)
  const m = seq.match(/\[<(\d+);(\d+);(\d+)([Mm])/);
  if (!m) return;
  const b = parseInt(m[1], 10);
  const y = parseInt(m[3], 10); // 1-based screen row
  const release = m[4] === "m";
  if (b !== 0) return; // left button only
  const idx = rowHitMap.get(y - 1);
  if (idx === undefined) return;
  selected = idx;
  if (release) doResume(); // resume on click release over a row
  else render();
}

function onData(chunk: Buffer): void {
  const str = chunk.toString("utf8");

  // Mouse sequences.
  if (str.startsWith("\x1b[<")) {
    handleMouse(str);
    return;
  }

  for (let i = 0; i < str.length; i++) {
    const ch = str[i];
    const code = str.charCodeAt(i);

    if (ch === "\x1b") {
      // Arrow keys: ESC [ A/B/C/D
      if (str[i + 1] === "[") {
        const k = str[i + 2];
        if (k === "A") {
          moveSelection(-1);
          i += 2;
          continue;
        }
        if (k === "B") {
          moveSelection(1);
          i += 2;
          continue;
        }
        if (k === "C") {
          // Right: expand the selected row's compaction chain (F5).
          expanded = true;
          render();
          i += 2;
          continue;
        }
        if (k === "D") {
          // Left: collapse the chain.
          expanded = false;
          render();
          i += 2;
          continue;
        }
      }
      // Bare ESC -> exit.
      cleanupAndExit(null, 0);
    } else if (code === 3) {
      // Ctrl-C
      cleanupAndExit(null, 130);
    } else if (code === 9) {
      // Tab: force the gpt (Codex/luna) deeper search (F4 escalation).
      fireGpt();
    } else if (ch === "\r" || ch === "\n") {
      doResume();
    } else if (code === 127 || code === 8) {
      // Backspace
      if (cursor > 0) {
        query = query.slice(0, cursor - 1) + query.slice(cursor);
        cursor--;
        toast = "";
        scheduleSearch();
      }
    } else if (code >= 32) {
      query = query.slice(0, cursor) + ch + query.slice(cursor);
      cursor++;
      toast = "";
      scheduleSearch();
    }
  }
}

// --- main -------------------------------------------------------------------
async function main(): Promise<void> {
  // Load optional plug-ins (non-blocking for S1). Both are relative to src/.
  await Promise.allSettled([
    loadPlugin("similar", new URL("../src/s2-similar.ts", import.meta.url).pathname),
    loadPlugin("gpt", new URL("../src/s3-gpt.ts", import.meta.url).pathname),
  ]);
  for (const id of ["similar", "gpt"] as SectionId[]) {
    if (sections[id].searcher) {
      sections[id].status = "idle";
      sections[id].note = "";
    }
  }

  if (!process.stdin.isTTY) {
    process.stderr.write("resume-picker: not a tty (must run in a terminal pane)\n");
    process.exit(2);
  }
  process.stdin.setRawMode(true);
  process.stdin.resume();
  process.stdin.on("data", onData);

  enterAltScreen();
  spinnerTimer = setInterval(() => {
    spinnerFrame++;
    if (ORDER.some((id) => sections[id].status === "searching") || (debounceTimer && query.trim())) render();
  }, 100);

  out.on("resize", render);
  render();
}

main();
