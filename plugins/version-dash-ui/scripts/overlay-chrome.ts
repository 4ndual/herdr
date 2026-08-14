/**
 * overlay-chrome.ts — the bits every bun overlay in this repo owes the user.
 *
 * The UX canon (plugin-ux-standard.md §3.1) says an overlay ships three doors to
 * the same exit — Esc, `q`, and a **mouse-clickable `[x]`** in the top right —
 * because herdr is a mouse-first TUI and a keyboard-only exit is not an exit.
 * The host draws the border and the title; the overlay draws the close control
 * on its own first row and resolves the click itself, which means turning mouse
 * reporting ON (the host does not forward clicks to a PTY that never asked for
 * them) and parsing SGR reports.
 *
 * SIBLING COPIES: button-dash-ui/scripts/overlay-chrome.ts and
 * resume-picker/src/overlay-chrome.ts are byte-identical copies of this file.
 * A plugin has to work when it is the only one installed, so it cannot import
 * out of a neighbour plugin directory — the same rule dash-common.sh follows.
 * Keep the three in sync; the whole point of a close button is that it behaves
 * identically everywhere, or the user stops trusting it.
 */

export const CLOSE_LABEL = "[x]";

/** Enable SGR mouse reporting. Call once, at startup. */
export function enableMouse(out: NodeJS.WriteStream): void {
  out.write("\x1b[?1000h\x1b[?1006h");
}

/** Disable it again. Call on every exit path, or the host terminal keeps it. */
export function disableMouse(out: NodeJS.WriteStream): void {
  out.write("\x1b[?1006l\x1b[?1000l");
}

/** Column of the first cell of the close control at this width. */
export function closeX(width: number): number {
  return Math.max(0, width - CLOSE_LABEL.length);
}

/**
 * Was this SGR mouse report a press on the close control?
 *
 * Reports look like `\x1b[<0;12;1M` — button;column;row, 1-based, `M` press and
 * `m` release. Only a PRESS counts, so a click started elsewhere and released
 * over the control does not close.
 */
export function isCloseClick(text: string, width: number, row = 0): boolean {
  for (const match of text.matchAll(/\x1b\[<(\d+);(\d+);(\d+)([Mm])/g)) {
    const [, button, col, line, kind] = match;
    if (kind !== "M" || Number(button) !== 0) continue;
    const x = Number(col) - 1;
    const y = Number(line) - 1;
    if (y !== row) continue;
    const start = closeX(width);
    if (x >= start && x < start + CLOSE_LABEL.length) return true;
  }
  return false;
}

/** True when the chunk holds any key that means "close". */
export function isCloseKey(text: string): boolean {
  // A bare Esc closes; an Esc that starts a CSI (arrow, mouse report) does not.
  if (text === "\x1b") return true;
  return /[q\x03\x04]/.test(text.replace(/\x1b\[[<0-9;]*[A-Za-z]/g, ""));
}
