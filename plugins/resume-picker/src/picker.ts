/**
 * resume-picker — the session picker herdr's resume-search overlay runs.
 *
 * HOST CONTRACT (src/app/resume_picker.rs, unchanged by this plugin):
 *
 *   - herdr spawns the wrapper as a hidden PTY with $HERDR_RESUME_RESULT_FILE in
 *     the env, and paints that PTY into a floating overlay.
 *   - The TUI must render to **stderr**: the wrapper redirects stdout into the
 *     result file, so anything printed there is treated as the answer.
 *   - On selection we print ONE shell command line to stdout and exit. herdr
 *     reads it on PaneDied and runs it in a new focused tab via `sh -lc`, so the
 *     line starts with `exec` (or ends in one) to replace that shell — the pane
 *     then dies with the agent, like a native agent pane.
 *   - Esc is handled by the host (it kills us); we also handle it locally so a
 *     hand run is not wedged.
 *
 * WHY THIS EXISTS: the previous picker was a separate standalone bun tool that
 * was never installed on any machine — `plugins/resume-picker/` was removed in
 * 401df7c7 and the button has toasted "not installed" ever since. This one ships
 * inside the repo, beside the plugin that opens it.
 *
 * WHAT IT SCANS (the point of the rewrite — the old one was Claude-only):
 *
 *   claude  ~/.claude/projects/<slug>/<uuid>.jsonl
 *   codex   ~/.codex/sessions/Y/M/D/rollout-<ts>-<uuid>.jsonl
 *   pi      ~/.pi/agent/sessions/<slug>/<ts>_<uuid>.jsonl
 *   + every fleet harness root: ~/harnesses/<name>/config/{projects,sessions}/…,
 *     whose runtime is declared in ~/harnesses/<name>/harness.json.
 *
 * A harness session is only resumable from the config root that owns it, so its
 * emitted command carries `env CLAUDE_CONFIG_DIR=… ` / `env CODEX_HOME=… `.
 * Claude resolves `--resume <id>` inside the CURRENT project dir, so the command
 * also `cd`s to the session's own cwd first — the host opens the tab with the
 * previously focused pane's cwd, which is almost never the right one.
 */

import { readdirSync, readFileSync, statSync, openSync, readSync, closeSync } from "node:fs";
import { join, basename } from "node:path";
import { homedir } from "node:os";
import {
  CLOSE_LABEL,
  closeX,
  disableMouse,
  enableMouse,
  isCloseClick,
  type CloseTarget,
} from "./overlay-chrome";

/** Cells that close the picker, registered by the render that drew them. */
let closeTargets: CloseTarget[] = [];

type Runtime = "claude" | "codex" | "pi";

type Session = {
  runtime: Runtime;
  /** Fleet harness that owns the config root, or "" for the default root. */
  harness: string;
  /** CLAUDE_CONFIG_DIR / CODEX_HOME to resume under, or "" for the default. */
  configRoot: string;
  file: string;
  /** Resume key: session uuid for claude/codex, the file path for pi. */
  id: string;
  mtimeMs: number;
  /** Filled lazily — only rows that actually get rendered are read. */
  detail?: { cwd: string; first: string; last: string };
};

const HOME = homedir();
const GLYPH: Record<Runtime, string> = { claude: "◆", codex: "▲", pi: "π" };

// ── discovery ───────────────────────────────────────────────────────────────
// Cheap by construction: directory walks + stat only. No file is opened here —
// with ~10k transcripts on this box, reading even the first line of each would
// cost seconds on every open. Message snippets are read lazily, per visible row.

function safeReaddir(dir: string): string[] {
  try {
    return readdirSync(dir);
  } catch {
    return [];
  }
}

function statMtime(file: string): number | null {
  try {
    const st = statSync(file);
    return st.isFile() ? st.mtimeMs : null;
  } catch {
    return null;
  }
}

/** claude layout: <root>/projects/<slug>/<uuid>.jsonl */
function scanClaudeRoot(root: string, harness: string, out: Session[]) {
  const projects = join(root, "projects");
  for (const slug of safeReaddir(projects)) {
    const dir = join(projects, slug);
    for (const name of safeReaddir(dir)) {
      if (!name.endsWith(".jsonl")) continue;
      const file = join(dir, name);
      const mtimeMs = statMtime(file);
      if (mtimeMs === null) continue;
      out.push({
        runtime: "claude",
        harness,
        configRoot: harness ? root : "",
        file,
        id: name.slice(0, -".jsonl".length),
        mtimeMs,
      });
    }
  }
}

/**
 * codex layout: <root>/sessions/YYYY/MM/DD/rollout-<iso>-<uuid>.jsonl
 * The uuid is taken from the FILENAME rather than the file's `session_meta`
 * header: on a forked rollout `payload.id` (this rollout) and
 * `payload.session_id` (its parent) differ, and the filename carries `id`, which
 * is the one `codex resume` wants. Reading the header would also mean opening
 * every file — see the note above.
 */
const CODEX_UUID = /-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.jsonl$/i;

function scanCodexRoot(root: string, harness: string, out: Session[]) {
  const sessions = join(root, "sessions");
  for (const y of safeReaddir(sessions)) {
    for (const m of safeReaddir(join(sessions, y))) {
      for (const d of safeReaddir(join(sessions, y, m))) {
        const dir = join(sessions, y, m, d);
        for (const name of safeReaddir(dir)) {
          const match = CODEX_UUID.exec(name);
          if (!match) continue;
          const file = join(dir, name);
          const mtimeMs = statMtime(file);
          if (mtimeMs === null) continue;
          out.push({
            runtime: "codex",
            harness,
            configRoot: harness ? root : "",
            file,
            id: match[1]!,
            mtimeMs,
          });
        }
      }
    }
  }
}

/** pi layout: ~/.pi/agent/sessions/<slug>/<ts>_<uuid>.jsonl, keyed by PATH. */
/// `sessions` is pi's session DIRECTORY (what `pi --session-dir` and
/// `PI_SESSION_DIR` name), not the `.pi` root — the override points straight at
/// it, so taking the root here would have made it unusable.
function scanPiSessions(sessions: string, out: Session[]) {
  for (const slug of safeReaddir(sessions)) {
    const dir = join(sessions, slug);
    for (const name of safeReaddir(dir)) {
      if (!name.endsWith(".jsonl")) continue;
      const file = join(dir, name);
      const mtimeMs = statMtime(file);
      if (mtimeMs === null) continue;
      out.push({ runtime: "pi", harness: "", configRoot: "", file, id: file, mtimeMs });
    }
  }
}

function discover(): Session[] {
  const out: Session[] = [];
  scanClaudeRoot(process.env.CLAUDE_CONFIG_DIR || join(HOME, ".claude"), "", out);
  scanCodexRoot(process.env.CODEX_HOME || join(HOME, ".codex"), "", out);
  // pi honors PI_SESSION_DIR the same way claude/codex honor their roots above;
  // without it a pi session under a non-default dir was invisible to the picker
  // while its sibling swap-runtime.ts already looked there.
  scanPiSessions(
    process.env.PI_SESSION_DIR || join(HOME, ".pi", "agent", "sessions"),
    out,
  );

  // Fleet harnesses. harness.json is authoritative for the runtime; a harness
  // without one is skipped rather than assumed to be claude (several exist).
  const harnesses = join(HOME, "harnesses");
  for (const name of safeReaddir(harnesses)) {
    const config = join(harnesses, name, "config");
    let runtime = "";
    try {
      runtime = JSON.parse(readFileSync(join(harnesses, name, "harness.json"), "utf8")).runtime ?? "";
    } catch {
      continue;
    }
    if (runtime === "claude") scanClaudeRoot(config, name, out);
    else if (runtime === "codex") scanCodexRoot(config, name, out);
  }

  out.sort((a, b) => b.mtimeMs - a.mtimeMs);
  return out;
}

// ── lazy per-session detail ─────────────────────────────────────────────────
// Head + tail chunk only: transcripts routinely reach tens of MB, and the first
// user message is always near the top while the last is always near the bottom.

const CHUNK = 96 * 1024;

function readEnds(file: string): { head: string; tail: string } {
  let fd: number | null = null;
  try {
    fd = openSync(file, "r");
    const size = statSync(file).size;
    const headLen = Math.min(size, CHUNK);
    const headBuf = Buffer.alloc(headLen);
    readSync(fd, headBuf, 0, headLen, 0);
    if (size <= CHUNK) return { head: headBuf.toString("utf8"), tail: "" };
    const tailLen = Math.min(size - CHUNK, CHUNK);
    const tailBuf = Buffer.alloc(tailLen);
    readSync(fd, tailBuf, 0, tailLen, size - tailLen);
    return { head: headBuf.toString("utf8"), tail: tailBuf.toString("utf8") };
  } catch {
    return { head: "", tail: "" };
  } finally {
    if (fd !== null) try { closeSync(fd); } catch { /* already gone */ }
  }
}

function jsonLines(chunk: string): any[] {
  const lines = chunk.split("\n");
  const out: any[] = [];
  for (const line of lines) {
    if (line.length < 2 || line[0] !== "{") continue; // partial first/last line
    try {
      out.push(JSON.parse(line));
    } catch {
      /* truncated record at a chunk boundary */
    }
  }
  return out;
}

/** Flatten any of the three content shapes (string | block array) to text. */
function contentText(content: any): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  const parts: string[] = [];
  for (const block of content) {
    if (typeof block === "string") parts.push(block);
    else if (block && typeof block.text === "string") parts.push(block.text);
  }
  return parts.join(" ");
}

/** User-visible prompts only: no tool results, no injected system reminders. */
function userTexts(records: any[], runtime: Runtime): string[] {
  const out: string[] = [];
  for (const rec of records) {
    let text = "";
    if (runtime === "claude") {
      if (rec.type !== "user" || rec.isSidechain) continue;
      if (Array.isArray(rec.message?.content) && rec.message.content.some((b: any) => b?.type === "tool_result")) continue;
      text = contentText(rec.message?.content);
    } else if (runtime === "codex") {
      const payload = rec.type === "response_item" ? rec.payload : rec;
      if (payload?.type !== "message" || payload.role !== "user") continue;
      text = contentText(payload.content);
    } else {
      if (rec.type !== "message" || rec.message?.role !== "user") continue;
      text = contentText(rec.message?.content);
    }
    text = text
      .replace(/<[^>]*>/g, " ") // codex's <cwd>/<environment_context> wrappers
      .replace(/\s+/g, " ")
      .trim();
    if (text) out.push(text);
  }
  return out;
}

function findCwd(records: any[], runtime: Runtime): string {
  for (const rec of records) {
    if (runtime === "codex") {
      const cwd = rec?.payload?.cwd ?? rec?.cwd;
      if (typeof cwd === "string" && cwd) return cwd;
    } else if (typeof rec?.cwd === "string" && rec.cwd) return rec.cwd;
  }
  // codex injects the working dir into its first user turn as <cwd>…</cwd>.
  for (const rec of records) {
    const raw = JSON.stringify(rec);
    const match = /<cwd>([^<]+)<\/cwd>/.exec(raw);
    if (match) return match[1]!.trim();
  }
  return "";
}

function detailOf(session: Session): { cwd: string; first: string; last: string } {
  if (session.detail) return session.detail;
  const { head, tail } = readEnds(session.file);
  const headRecords = jsonLines(head);
  const firsts = userTexts(headRecords, session.runtime);
  const lasts = userTexts(jsonLines(tail), session.runtime);
  const cwd = findCwd(headRecords, session.runtime);
  // Capped: only one line of each is ever rendered, and an uncapped snippet
  // would put a whole pasted file into the filter haystack.
  const cap = (text: string) => text.slice(0, 300);
  session.detail = {
    cwd,
    first: cap(firsts[0] ?? ""),
    last: cap((lasts.length ? lasts[lasts.length - 1] : firsts[firsts.length - 1]) ?? ""),
  };
  return session.detail;
}

// ── the emitted command ─────────────────────────────────────────────────────

function shq(value: string): string {
  return `'${value.replace(/'/g, `'\\''`)}'`;
}

function resumeCommand(session: Session): string {
  const detail = detailOf(session);
  const envPrefix = session.configRoot
    ? `env ${session.runtime === "claude" ? "CLAUDE_CONFIG_DIR" : "CODEX_HOME"}=${shq(session.configRoot)} `
    : "";
  let argv: string;
  if (session.runtime === "claude") argv = `claude --resume ${shq(session.id)} --dangerously-skip-permissions`;
  else if (session.runtime === "codex") argv = `codex resume ${shq(session.id)} --dangerously-bypass-approvals-and-sandbox`;
  else argv = `pi --session ${shq(session.id)}`; // pi has no verified bypass flag

  const exec = `exec ${envPrefix}${argv}`;
  if (!detail.cwd) return exec;
  // A missing cwd must NOT swallow the resume (`cd X && exec …` would exit the
  // shell and the tab would flash and die), so it degrades to a warning.
  return `cd ${shq(detail.cwd)} || printf 'resume-picker: %s is gone, continuing in %s\\n' ${shq(detail.cwd)} "$PWD"; ${exec}`;
}

// ── filtering ───────────────────────────────────────────────────────────────
// Case-insensitive substring match, ALL space-separated terms required, so
// "codex linktic" narrows by both and order does not matter.
//
// Deliberately NOT the usual scattered-subsequence "fuzzy" match: the haystack
// here includes whole user messages, and against a few hundred characters of
// prose a subsequence matches essentially everything. Measured on this box,
// `linktic codex` under subsequence matching selected an unrelated claude
// session in a demo harness. Unranked subsequence + long haystacks is not a
// fuzzy filter, it is a no-op.

function matches(haystack: string, needle: string): boolean {
  return haystack.includes(needle);
}

function haystackOf(session: Session): string {
  const detail = detailOf(session);
  return [session.runtime, session.harness, detail.cwd, detail.first, detail.last, session.file]
    .join(" ")
    .toLowerCase();
}

/**
 * Filtering searches message text, so the first keystroke has to pay for the
 * detail of every session — the list is otherwise only lazily read, per visible
 * row. Measured at ~200 sessions on this box it is well under a second, and it
 * happens once; the frame before it says so, because a silent freeze on the
 * first character reads as a hung picker.
 */
let indexed = false;
function indexAll() {
  if (indexed) return;
  indexed = true;
  write("\x1b[H\x1b[2J reading transcripts…\r\n");
  for (const session of all) detailOf(session);
}

// ── rendering ───────────────────────────────────────────────────────────────

const out = process.stderr;
const write = (text: string) => out.write(text);

function cols(): number {
  return (out as any).columns ?? 100;
}
function rows(): number {
  return (out as any).rows ?? 24;
}

function pad(text: string, width: number): string {
  if (width <= 0) return "";
  const clipped = [...text].slice(0, width).join("");
  return clipped + " ".repeat(Math.max(0, width - [...clipped].length));
}

function ago(mtimeMs: number): string {
  const secs = Math.max(0, (Date.now() - mtimeMs) / 1000);
  if (secs < 60) return `${Math.floor(secs)}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h`;
  return `${Math.floor(secs / 86400)}d`;
}

let all: Session[] = [];
let shown: Session[] = [];
let query = "";
let cursor = 0;
let top = 0;

function refilter() {
  const terms = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (terms.length) indexAll();
  shown = terms.length ? all.filter((s) => terms.every((t) => matches(haystackOf(s), t))) : all;
  cursor = 0;
  top = 0;
}

function render() {
  const width = cols();
  const height = rows();
  const listRows = Math.max(1, height - 3);
  if (cursor < top) top = cursor;
  if (cursor >= top + listRows) top = cursor - listRows + 1;

  const lines: string[] = [];
  // The clickable [x] rides the header row’s right edge: herdr is mouse-first,
  // so every overlay ships a close control a mouse can hit, not only Esc
  // (canon §3.1.7). The pad is computed from the VISIBLE length, since the
  // header carries SGR escapes that occupy no columns.
  const head = `\x1b[1m resume\x1b[0m  ${shown.length}/${all.length} sessions   \x1b[2m↑↓ move · enter resume (skip mode) · q / esc / [x] close\x1b[0m`;
  const headWidth = head.replace(/\x1b\[[0-9;]*m/g, "").length;
  const titleRight = closeX(width);
  closeTargets = [{ row: 0, x: titleRight, len: CLOSE_LABEL.length }];
  lines.push(
    `${head}${" ".repeat(Math.max(1, titleRight - headWidth))}\x1b[36m${CLOSE_LABEL}\x1b[0m`,
  );
  lines.push(` \x1b[36m/\x1b[0m ${query}\x1b[7m \x1b[0m`);

  for (let i = 0; i < listRows; i += 1) {
    const index = top + i;
    if (index >= shown.length) {
      lines.push("");
      continue;
    }
    const session = shown[index]!;
    const detail = detailOf(session); // only visible rows pay the read
    const selected = index === cursor;
    const where = detail.cwd || basename(session.file);
    const meta = `${GLYPH[session.runtime]} ${pad(session.harness || "-", 12)} ${pad(ago(session.mtimeMs), 4)} ${pad(where, 34)}`;
    const snippet = detail.last || detail.first || "(no user message)";
    const row = pad(` ${meta} ${snippet}`, width - 1);
    lines.push(selected ? `\x1b[7m${row}\x1b[0m` : row);
  }

  // One write per frame: a partially flushed screen is visible in the overlay.
  write(`\x1b[H\x1b[2J${lines.map((line) => `${line}\x1b[K`).join("\r\n")}`);
}

// ── input ───────────────────────────────────────────────────────────────────

function finish(command: string | null) {
  disableMouse(out as unknown as NodeJS.WriteStream);
  write("\x1b[H\x1b[2J");
  try {
    (process.stdin as any).setRawMode?.(false);
  } catch {
    /* not a tty */
  }
  if (command) process.stdout.write(`${command}\n`);
  process.exit(0);
}

/**
 * One chunk of input may contain several keys — a fast typist, a paste, or the
 * host replaying buffered bytes all arrive as one read, and an arrow key is
 * three bytes of that stream. So this walks the chunk and consumes escape
 * sequences IN PLACE rather than testing the chunk as a whole: the earlier
 * shape (`if chunk.startsWith("\x1b[")`) dropped the `\x1b` and then typed the
 * literal `[B` of an arrow key into the filter box whenever it was not alone.
 */
function onData(chunk: Buffer) {
  const text = chunk.toString("utf8");
  // A press on the [x] cancels, exactly like Esc: one exit, three doors.
  if (isCloseClick(text, closeTargets)) return finish(null);
  let index = 0;
  let dirty = false;

  while (index < text.length) {
    const ch = text[index]!;

    if (ch === "\x1b") {
      const kind = text[index + 1];
      if (kind !== "[" && kind !== "O") {
        // A lone ESC is cancel (the host normally intercepts it first).
        return finish(null);
      }
      let end = index + 2;
      while (end < text.length && !/[\x40-\x7e]/.test(text[end]!)) end += 1;
      const final = text[end] ?? "";
      const params = text.slice(index + 2, end);
      index = end + 1;
      const page = Math.max(1, rows() - 4);
      if (final === "A") cursor -= 1;
      else if (final === "B") cursor += 1;
      else if (final === "H" || (final === "~" && params === "1")) cursor = 0;
      else if (final === "F" || (final === "~" && params === "4")) cursor = shown.length - 1;
      else if (final === "~" && params === "5") cursor -= page;
      else if (final === "~" && params === "6") cursor += page;
      else continue; // mouse report / unhandled sequence: ignore, never type it
      dirty = true;
      continue;
    }

    index += 1;
    if (ch === "\r" || ch === "\n") {
      const session = shown[cursor];
      return finish(session ? resumeCommand(session) : null);
    }
    if (ch === "\x03" || ch === "\x04") return finish(null); // ^C / ^D
    if (ch === "\x7f" || ch === "\b") {
      query = query.slice(0, -1);
      refilter();
      dirty = true;
      continue;
    }
    if (ch === "\x0e") { // ^N
      cursor += 1;
      dirty = true;
      continue;
    }
    if (ch === "\x10") { // ^P
      cursor -= 1;
      dirty = true;
      continue;
    }
    if (ch === "\x15") { // ^U clears the filter
      query = "";
      refilter();
      dirty = true;
      continue;
    }
    if (ch < " ") continue;
    query += ch;
    refilter();
    dirty = true;
  }

  cursor = Math.min(Math.max(0, cursor), Math.max(0, shown.length - 1));
  if (dirty) render();
}

function main() {
  all = discover();
  shown = all;
  if (all.length === 0) {
    write("no claude, codex or pi sessions found\r\n");
    setTimeout(() => finish(null), 1200);
    return;
  }
  try {
    (process.stdin as any).setRawMode?.(true);
  } catch {
    /* not a tty — keys will still arrive line-buffered */
  }
  process.stdin.resume();
  enableMouse(out as unknown as NodeJS.WriteStream);
  // Hand the terminal back however we leave.
  process.on("exit", () => disableMouse(out as unknown as NodeJS.WriteStream));
  process.stdin.on("data", onData);
  process.on("SIGWINCH", render);
  render();
}

main();
