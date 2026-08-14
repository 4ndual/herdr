/**
 * notes-overlay.ts — the latest release notes, as an OVERLAY.
 *
 * Why (wrong-surface defect): clicking the `latest X.Y.Z` token used to
 * `pane split` a real pane and run a pager in it. That is the wrong surface for
 * a transient read: it takes over half the workspace, it survives until the user
 * remembers to close it, it has no Esc contract, and it leaves a stray pane in
 * the layout the next restore will faithfully bring back. The UX canon says a
 * secondary surface is an overlay pane — Esc closes it, it needs no cleanup, and
 * the host paints the border and title around it.
 *
 * Chrome follows the canonical overlay (the agent-launcher popup): a title on
 * the host-drawn border, content, and a footer hint row naming every key. The
 * host owns the frame; this program owns the inside of it.
 *
 * Everything read here is LOCAL — the collector caches the notes markdown on
 * each successful manifest fetch — so the overlay opens instantly and offline.
 */

import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

const STATE_DIR =
  process.env.HERDR_PLUGIN_STATE_DIR ??
  join(
    process.env.XDG_STATE_HOME ?? `${process.env.HOME}/.local/state`,
    "herdr/plugins/builtin.version-dash",
  );

const CANDIDATES = [join(STATE_DIR, "latest_notes.md"), join(STATE_DIR, "releases_notes.md")];

const MISSING = `# No release notes cached yet

The collector caches release notes on each successful fetch of the channel
manifest. It has not completed one yet, or the fetch failed.

Check:
  version-dash-collector            # run it once by hand
  ${STATE_DIR}/../../version-dash-collector.log
`;

function load(): string {
  for (const path of CANDIDATES) {
    try {
      if (existsSync(path)) {
        const text = readFileSync(path, "utf8");
        if (text.trim()) return text;
      }
    } catch {
      /* fall through to the next candidate */
    }
  }
  return MISSING;
}

const RESET = "\x1b[0m";
const DIM = "\x1b[90m";
const BOLD = "\x1b[1m";
const ACCENT = "\x1b[36m";

const out = process.stdout;
const cols = () => (out as any).columns ?? 80;
const rows = () => (out as any).rows ?? 24;

/** Minimal markdown styling: headings and list bullets, nothing that reflows. */
function style(line: string): string {
  if (/^#{1,6}\s/.test(line)) return `${BOLD}${ACCENT}${line.replace(/^#+\s*/, "")}${RESET}`;
  if (/^\s*[-*]\s/.test(line)) return line.replace(/^(\s*)[-*]\s/, `$1${ACCENT}•${RESET} `);
  if (/^\s*```/.test(line)) return `${DIM}${line}${RESET}`;
  return line;
}

const source = load().replace(/\r/g, "").split("\n");
let top = 0;

function render() {
  const width = cols();
  const height = rows();
  // One row for the footer hint; the host draws the border and the title.
  const view = Math.max(1, height - 1);
  const maxTop = Math.max(0, source.length - view);
  if (top > maxTop) top = maxTop;

  const body: string[] = [];
  for (let i = 0; i < view; i += 1) {
    const line = source[top + i] ?? "";
    body.push(style(line.slice(0, width - 1)));
  }
  const position = source.length > view ? `${top + 1}-${Math.min(top + view, source.length)}/${source.length}  ` : "";
  const hint = `${DIM} ${position}↑↓ scroll · pgup/pgdn page · esc close${RESET}`;
  out.write(`\x1b[H\x1b[2J${body.join("\r\n")}\r\n${hint}`);
}

function close() {
  out.write("\x1b[H\x1b[2J");
  try {
    (process.stdin as any).setRawMode?.(false);
  } catch {
    /* not a tty */
  }
  process.exit(0);
}

function onData(chunk: Buffer) {
  const text = chunk.toString("utf8");
  const page = Math.max(1, rows() - 2);
  let index = 0;
  let dirty = false;
  while (index < text.length) {
    const ch = text[index]!;
    if (ch === "\x1b") {
      const kind = text[index + 1];
      if (kind !== "[" && kind !== "O") return close(); // bare Esc
      let end = index + 2;
      while (end < text.length && !/[\x40-\x7e]/.test(text[end]!)) end += 1;
      const final = text[end] ?? "";
      const params = text.slice(index + 2, end);
      index = end + 1;
      if (final === "A") top -= 1;
      else if (final === "B") top += 1;
      else if (final === "~" && params === "5") top -= page;
      else if (final === "~" && params === "6") top += page;
      else if (final === "H") top = 0;
      else if (final === "F") top = source.length;
      else continue;
      dirty = true;
      continue;
    }
    index += 1;
    if (ch === "q" || ch === "\x03" || ch === "\x04") return close();
    if (ch === " ") {
      top += page;
      dirty = true;
    }
  }
  top = Math.max(0, top);
  if (dirty) render();
}

try {
  (process.stdin as any).setRawMode?.(true);
} catch {
  /* not a tty — render once; the host can still close us */
}
process.stdin.resume();
process.stdin.on("data", onData);
process.on("SIGWINCH", render);
render();
