/**
 * reference-overlay.ts — the button dash's full reference list, as an OVERLAY.
 *
 * Why this exists (defect D4): the list used to expand *inside* the bottom-band
 * panel, below the compact grid. The band has ~4 slack rows and the registry has
 * 11 buttons, so 7 of them — and every description — were simply clipped off the
 * screen with no scroll affordance. The UX canon (plugin-ux-standard.md §3) says
 * a list that does not fit the band is a secondary surface, and a secondary
 * surface is an overlay pane: it closes on Esc, it needs no cleanup, and it can
 * be as tall as it likes.
 *
 * Data comes from `reference.json` beside the plugin manifest. That file is the
 * shared contract with the wasm guest, whose `reference_json_matches_buttons`
 * test fails if the two ever disagree — so this overlay cannot drift away from
 * the strip it documents.
 *
 * Contract with the host: this is an ordinary PTY program in an overlay pane.
 * herdr paints the chrome; we own the screen and the keys. Esc / q / Enter close
 * (the host also kills the pane), and we draw on stdout because — unlike the
 * resume picker — an overlay pane has no result-file protocol.
 */

import { readFileSync } from "node:fs";
import { join, dirname } from "node:path";
import {
  CLOSE_LABEL,
  closeX,
  disableMouse,
  enableMouse,
  isCloseClick,
  isCloseKey,
  type CloseTarget,
} from "./overlay-chrome";

/** Cells that close the overlay, registered by the render that drew them. */
let closeTargets: CloseTarget[] = [];

type Button = {
  id: string;
  label: string;
  emoji: string;
  shortcut: string;
  color: string;
  description: string;
};

const ROOT = process.env.HERDR_PLUGIN_ROOT ?? join(dirname(new URL(import.meta.url).pathname), "..");
const buttons: Button[] = JSON.parse(readFileSync(join(ROOT, "reference.json"), "utf8")).buttons;

// Palette roles → the closest ANSI colour. The host styles guest spans by role
// (src/ui.rs plugin_span_style); an overlay is a plain PTY, so it approximates
// the same role→colour mapping itself rather than inventing a second palette.
const ROLE: Record<string, string> = {
  green: "\x1b[32m",
  success: "\x1b[32m",
  mauve: "\x1b[35m",
  magenta: "\x1b[35m",
  muted: "\x1b[90m",
  subtext: "\x1b[37m",
  teal: "\x1b[36m",
  cyan: "\x1b[36m",
  peach: "\x1b[38;5;216m",
  warning: "\x1b[33m",
  yellow: "\x1b[33m",
  blue: "\x1b[34m",
  accent: "\x1b[1;34m",
  red: "\x1b[31m",
  danger: "\x1b[31m",
};
const RESET = "\x1b[0m";
const DIM = "\x1b[90m";

const out = process.stdout;
const cols = () => (out as any).columns ?? 80;

/**
 * Terminal cell width, the same rule the wasm guest's `cell_width` uses: an
 * emoji occupies two columns, a variation selector none, everything else one.
 * `String.length` and even `[...s].length` both get this wrong — 🗄️ is two code
 * points and two cells, ⌃⌥F is three of each — and padding by the wrong count is
 * exactly what makes a "tidy" column drift by a space per emoji row.
 */
function width(text: string): number {
  let n = 0;
  for (const ch of text) {
    const cp = ch.codePointAt(0)!;
    if (cp === 0xfe0f || cp === 0xfe0e) continue; // variation selector: zero width
    n += cp >= 0x1f300 || (cp >= 0x2600 && cp <= 0x27bf) ? 2 : 1;
  }
  return n;
}

const pad = (text: string, to: number) => text + " ".repeat(Math.max(0, to - width(text)));

function render() {
  const term = cols();
  const lines: string[] = [];
  closeTargets = [];
  // Title row carries a close control at the right, and the footer carries one
  // whose position is known exactly — see overlay-chrome for why both.
  const title = " Button dash — every button";
  const titleRight = closeX(term);
  lines.push(
    `\x1b[1m${title}\x1b[0m${" ".repeat(Math.max(1, titleRight - title.length))}${ROLE.teal}${CLOSE_LABEL}${RESET}`,
  );
  closeTargets.push({ row: 0, x: titleRight, len: CLOSE_LABEL.length });
  lines.push("");
  // Column widths from the data, so a longer label never wraps mid-row.
  const labelW = Math.max(...buttons.map((b) => width(b.label)));
  const keyW = Math.max(...buttons.map((b) => width(b.shortcut)));
  for (const b of buttons) {
    const color = ROLE[b.color] ?? "";
    const label = pad(b.label, labelW);
    const key = pad(b.shortcut, keyW);
    // description is the only part allowed to be clipped, and it is clipped at
    // the RIGHT because its first words carry the meaning (canon §5.5).
    const head = ` ${pad(b.emoji, 2)} ${label}  ${key}  `;
    const room = Math.max(10, term - width(head) - 1);
    const desc = [...b.description].slice(0, room).join("");
    lines.push(`${color}${head}${RESET}${DIM}${desc}${RESET}`);
  }
  // Footer names every key that does anything, and carries the close control
  // that is always where it says it is: this row was just printed, so its index
  // and the control's columns are known rather than assumed.
  lines.push("");
  const footerRow = lines.length;
  lines.push(`${ROLE.teal} ${CLOSE_LABEL}${RESET}${DIM} close · ↵ / q / esc also close${RESET}`);
  closeTargets.push({ row: footerRow, x: 1, len: CLOSE_LABEL.length });
  out.write(`\x1b[H\x1b[2J${lines.join("\r\n")}`);
}

function close() {
  disableMouse(out);
  out.write("\x1b[H\x1b[2J");
  try {
    (process.stdin as any).setRawMode?.(false);
  } catch {
    /* not a tty */
  }
  process.exit(0);
}

try {
  (process.stdin as any).setRawMode?.(true);
} catch {
  /* not a tty — still render once, the host will close us */
}
process.stdin.resume();
enableMouse(out);
process.on("exit", () => disableMouse(out));
process.stdin.on("data", (chunk: Buffer) => {
  const text = chunk.toString("utf8");
  // A press on the [x], or any of the close keys. Everything else is ignored
  // rather than typed into a surface that has no input.
  if (isCloseClick(text, closeTargets) || isCloseKey(text) || /[\r\n]/.test(text)) close();
});
process.on("SIGWINCH", render);
render();
