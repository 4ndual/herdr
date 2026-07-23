// S3 — the "gpt" fallback section.
//
// This is an ESCALATION, not a per-keystroke search: the picker only fires it
// when S1+S2 came back empty/weak or the user pressed "search deeper" (Tab).
// It follows the search-session skill's methodology: cheaply pre-grep a
// candidate set of transcripts, hand those candidates + the query to a model
// via ONE non-interactive `cx -p` call, and ask the model to pick the sessions
// that are genuinely about the query and quote the matching snippet in a strict
// parseable format. We then map each pick back to a ResultLine using the real
// session metadata from sessions.ts — never inventing a session.
//
// Failure is visible, never silent and never faked: an empty candidate set, a
// timeout, a model error, or an unparseable reply all surface as a single
// degradation ResultLine (empty sessionId marks it non-selectable for the TUI).
// If the AbortSignal fires (the query changed) we stop and yield nothing.

import { readFileSync } from "node:fs";
import { basename } from "node:path";
import { spawn } from "node:child_process";
import type { CorpusConfig, ResultLine, ResumeRef, SectionSearcher, SessionMeta } from "./types.ts";
import { loadConfig, MetaCache } from "./sessions.ts";

/** Wall-clock budget for the single cx call before we give up. */
export const GPT_TIMEOUT_MS = 30_000;
/** Cap on transcripts handed to the model — keeps the prompt small and the call cheap. */
export const MAX_CANDIDATES = 30;
/** A degradation line carries an empty sessionId so the TUI renders it non-selectable. */
const DEGRADED_ID = "";

/**
 * A degradation ResultLine. `sessionId === ""` is the agreed sentinel with W1's
 * picker: the row is shown (so the user sees why gpt produced nothing) but is
 * skipped by keyboard/mouse selection.
 */
function degraded(message: string): ResultLine {
  const resumeRef: ResumeRef = { sessionId: DEGRADED_ID, jsonlPath: "", origin: "daily", cwd: "" };
  return {
    snippet: message,
    sessionId: DEGRADED_ID,
    shortId: "",
    lastUserAgo: "",
    lastAgentAgo: "",
    createdAgo: "",
    resumeRef,
  };
}

/** Compact "ago" like 2h / 3d / 5w. Empty string when the timestamp is unknown. */
export function agoFmt(fromMs: number, now = Date.now()): string {
  if (!fromMs) return "";
  const s = Math.max(0, Math.floor((now - fromMs) / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h`;
  const d = Math.floor(h / 24);
  if (d < 7) return `${d}d`;
  const w = Math.floor(d / 7);
  if (w < 52) return `${w}w`;
  return `${Math.floor(d / 365)}y`;
}

function metaToResultLine(meta: SessionMeta, snippet: string, now = Date.now()): ResultLine {
  const resumeRef: ResumeRef = {
    sessionId: meta.sessionId,
    jsonlPath: meta.jsonlPath,
    origin: meta.origin,
    cwd: meta.cwd,
  };
  return {
    snippet: snippet.trim().slice(0, 200) || meta.summary.slice(0, 200),
    sessionId: meta.sessionId,
    shortId: meta.sessionId.slice(0, 8),
    lastUserAgo: agoFmt(meta.lastUserMs, now),
    lastAgentAgo: agoFmt(meta.lastAgentMs, now),
    createdAgo: agoFmt(meta.createdMs, now),
    resumeRef,
  };
}

interface Candidate {
  path: string;
  meta: SessionMeta;
  snippet: string;
}

/**
 * Cheap pre-grep: list transcripts whose raw text contains the query, resolve
 * each to real metadata (dedup by realpath via the cache), and pull one clean
 * matched snippet. Capped at MAX_CANDIDATES. Runs `rg` once per corpus root.
 */
async function gatherCandidates(
  query: string,
  config: CorpusConfig,
  cache: MetaCache,
  signal: AbortSignal,
): Promise<Candidate[]> {
  const files = await rgFiles(query, config, signal);
  const seen = new Set<string>();
  const out: Candidate[] = [];
  for (const f of files) {
    if (signal.aborted) return out;
    const meta = cache.getMeta(f);
    if (!meta) continue;
    if (seen.has(meta.jsonlPath)) continue; // dedup symlink bridges by realpath
    seen.add(meta.jsonlPath);
    out.push({ path: meta.jsonlPath, meta, snippet: extractSnippet(meta.jsonlPath, query) });
    if (out.length >= MAX_CANDIDATES) break;
  }
  return out;
}

/**
 * Files matching the query. Prefers user-message lines (search-session
 * methodology: cuts tool-output/boilerplate noise); falls back to any line when
 * that yields nothing. Excludes subagent transcripts.
 */
async function rgFiles(query: string, config: CorpusConfig, signal: AbortSignal): Promise<string[]> {
  const roots = config.roots.map((r) => r.path);
  if (roots.length === 0) return [];
  const filtered = (list: string[]) => list.filter((p) => !p.includes("subagents"));

  // Pass 1: the full phrase on a user-message line (search-session methodology).
  const phrase = escapeRe(query);
  const userScoped = filtered(
    await runRg(["-l", "-i", "--glob", "*.jsonl", "-e", `"type":"user".*${phrase}`, "-e", `${phrase}.*"type":"user"`, ...roots], signal),
  );
  if (userScoped.length > 0) return userScoped;
  if (signal.aborted) return [];

  // Pass 2: S3 is a FALLBACK — when the exact phrase isn't present, broaden to an
  // OR of the query's significant terms so sessions that S1's exact match missed
  // still surface as candidates for the model to judge semantically. User-scoped.
  const terms = significantTerms(query);
  if (terms.length > 0) {
    const args = ["-l", "-i", "--glob", "*.jsonl"];
    for (const t of terms) {
      args.push("-e", `"type":"user".*${escapeRe(t)}`, "-e", `${escapeRe(t)}.*"type":"user"`);
    }
    const orScoped = filtered(await runRg([...args, ...roots], signal));
    if (orScoped.length > 0) return orScoped;
    if (signal.aborted) return [];
  }

  // Pass 3: last resort — the full phrase anywhere (tool output, assistant text).
  return filtered(await runRg(["-l", "-i", "-F", "--glob", "*.jsonl", query, ...roots], signal));
}

/** Query words worth OR-searching: length>=3, stopwords dropped, capped. */
function significantTerms(query: string): string[] {
  const stop = new Set(["the", "and", "for", "with", "that", "this", "was", "how", "why", "what", "when", "from", "into", "about"]);
  const seen = new Set<string>();
  const out: string[] = [];
  for (const w of query.toLowerCase().split(/[^a-z0-9]+/)) {
    if (w.length < 3 || stop.has(w) || seen.has(w)) continue;
    seen.add(w);
    out.push(w);
    if (out.length >= 6) break;
  }
  return out;
}

function escapeRe(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** Spawn rg, resolve to matched file paths. Killed if the signal aborts. */
function runRg(args: string[], signal: AbortSignal): Promise<string[]> {
  return new Promise((resolve) => {
    if (signal.aborted) return resolve([]);
    const proc = spawn("rg", args, { stdio: ["ignore", "pipe", "ignore"] });
    let buf = "";
    const onAbort = () => proc.kill("SIGKILL");
    signal.addEventListener("abort", onAbort, { once: true });
    proc.stdout.on("data", (d) => (buf += d));
    proc.on("error", () => {
      signal.removeEventListener("abort", onAbort);
      resolve([]);
    });
    proc.on("close", () => {
      signal.removeEventListener("abort", onAbort);
      resolve(buf.split("\n").filter(Boolean));
    });
  });
}

/**
 * One clean matched snippet from a transcript: first line containing the query,
 * with plain text pulled out of the JSON (falls back to a raw window). Cheap
 * because it only runs on the <=30 candidate files, never per keystroke.
 */
export function extractSnippet(path: string, query: string): string {
  let content: string;
  try {
    content = readFileSync(path, "utf8");
  } catch {
    return "";
  }
  // Try the full phrase first, then any significant term (OR-candidates may not
  // contain the exact phrase). First line that matches any needle wins.
  const needles = [query.toLowerCase(), ...significantTerms(query)];
  for (const line of content.split("\n")) {
    if (!line) continue;
    const low = line.toLowerCase();
    const needle = needles.find((n) => low.includes(n));
    if (!needle) continue;
    let text = "";
    try {
      const o = JSON.parse(line) as any;
      text = pullText(o?.message?.content) || pullText(o?.payload?.content) || "";
    } catch {
      // not JSON — fall through to raw window
    }
    if (!text) text = line;
    return windowAround(text, needle);
  }
  return "";
}

function pullText(content: unknown): string {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    const parts: string[] = [];
    for (const b of content) {
      if (b && typeof b === "object" && typeof (b as any).text === "string") parts.push((b as any).text);
    }
    return parts.join(" ");
  }
  return "";
}

/** ~120-char window centered on the first case-insensitive match. */
function windowAround(text: string, query: string): string {
  const clean = text.replace(/\s+/g, " ").trim();
  const idx = clean.toLowerCase().indexOf(query.toLowerCase());
  if (idx < 0) return clean.slice(0, 120);
  const start = Math.max(0, idx - 40);
  const end = Math.min(clean.length, idx + query.length + 80);
  return (start > 0 ? "…" : "") + clean.slice(start, end) + (end < clean.length ? "…" : "");
}

/** Build the strict-format prompt handed to the model. */
export function buildPrompt(query: string, candidates: Candidate[]): string {
  const lines = candidates.map((c, i) => {
    const summary = c.meta.summary.replace(/\s+/g, " ").slice(0, 140);
    const snip = c.snippet.replace(/\s+/g, " ").slice(0, 160);
    return `[${i + 1}] id=${c.meta.sessionId}\n    summary: ${summary}\n    match: ${snip}`;
  });
  return [
    `You are ranking Claude/Codex chat sessions for a resume-search picker.`,
    `The user is looking for the session about: "${query}".`,
    ``,
    `Candidates (already text-matched the query — judge TRUE topical relevance):`,
    ...lines,
    ``,
    `Return ONLY lines in this exact format, one per genuinely relevant session,`,
    `most relevant first:`,
    `HIT|<full-session-id>|<a short quote (<=100 chars) showing why it matches>`,
    `Use only ids from the candidate list. If none are genuinely relevant, output`,
    `the single line: NONE`,
    `Output nothing else — no preamble, no markdown, no numbering.`,
  ].join("\n");
}

interface Hit {
  sessionId: string;
  snippet: string;
}

/** Parse the model reply into hits. Tolerant of stray lines; ignores non-HIT text. */
export function parseHits(out: string): Hit[] {
  const hits: Hit[] = [];
  for (const raw of out.split("\n")) {
    const line = raw.trim();
    if (!line.startsWith("HIT|")) continue;
    const rest = line.slice(4);
    const sep = rest.indexOf("|");
    if (sep < 0) continue;
    const sessionId = rest.slice(0, sep).trim();
    const snippet = rest.slice(sep + 1).trim();
    if (sessionId) hits.push({ sessionId, snippet });
  }
  return hits;
}

/**
 * Run the single cx headless call. Prompt goes via stdin (avoids argv limits and
 * the -p+--system-prompt drop regression). Resolves the raw stdout, or null on
 * timeout / abort / spawn error. cx already defaults to skip-permissions and a
 * one-shot with -p; we pass no system prompt.
 */
export function runCx(prompt: string, signal: AbortSignal, timeoutMs = GPT_TIMEOUT_MS): Promise<string | null> {
  return new Promise((resolve) => {
    if (signal.aborted) return resolve(null);
    // The cx launcher resolves its node/gateway paths from $HOME. Tests sandbox
    // $HOME to point the corpus at fixtures, so honor CX_HOME to run cx against
    // the real home; unset in production, where HOME is already correct.
    const cxHome = process.env.CX_HOME;
    const env = cxHome ? { ...process.env, HOME: cxHome } : process.env;
    const proc = spawn("cx", ["-p"], { stdio: ["pipe", "pipe", "ignore"], env });
    let out = "";
    let done = false;
    const finish = (val: string | null) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      signal.removeEventListener("abort", onAbort);
      resolve(val);
    };
    // Bun/node async spawn ignores an option timeout — kill manually.
    const timer = setTimeout(() => {
      proc.kill("SIGKILL");
      finish(null);
    }, timeoutMs);
    const onAbort = () => {
      proc.kill("SIGKILL");
      finish(null);
    };
    signal.addEventListener("abort", onAbort, { once: true });
    proc.stdout.on("data", (d) => (out += d));
    proc.on("error", () => finish(null));
    proc.on("close", () => finish(signal.aborted ? null : out));
    proc.stdin.on("error", () => {}); // ignore EPIPE if the child dies early
    proc.stdin.write(prompt);
    proc.stdin.end();
  });
}

/**
 * The S3 SectionSearcher. Fired on demand by the picker (empty/weak local
 * results or explicit "search deeper"). Yields real ResultLines mapped from the
 * model's picks, or exactly one degradation line, or nothing if aborted.
 */
export const searchGpt: SectionSearcher = async function* (query: string, signal: AbortSignal) {
  const q = query.trim();
  if (!q) return;

  const config = loadConfig();
  const cache = new MetaCache(config);

  const candidates = await gatherCandidates(q, config, cache, signal);
  cache.flush();
  if (signal.aborted) return;
  if (candidates.length === 0) {
    yield degraded("gpt: no matching transcripts");
    return;
  }

  const prompt = buildPrompt(q, candidates);
  const out = await runCx(prompt, signal);
  if (signal.aborted) return; // query changed mid-flight — discard silently
  if (out === null) {
    yield degraded("gpt: timed out");
    return;
  }

  const hits = parseHits(out);
  if (hits.length === 0) {
    yield degraded("gpt: no relevant sessions");
    return;
  }

  const byId = new Map(candidates.map((c) => [c.meta.sessionId, c]));
  const now = Date.now();
  let yielded = 0;
  for (const hit of hits) {
    if (signal.aborted) return;
    const cand = byId.get(hit.sessionId) ?? findByPrefix(byId, hit.sessionId);
    if (!cand) continue; // model returned an id not in the candidate set — never fabricate
    yield metaToResultLine(cand.meta, hit.snippet || cand.snippet, now);
    yielded++;
  }
  if (yielded === 0) yield degraded("gpt: no relevant sessions");
};

/** Match a possibly-truncated id (model echoing a short form) against candidates. */
function findByPrefix(byId: Map<string, Candidate>, id: string): Candidate | undefined {
  if (id.length < 6) return undefined;
  for (const [full, cand] of byId) {
    if (full.startsWith(id) || id.startsWith(full)) return cand;
  }
  return undefined;
}
