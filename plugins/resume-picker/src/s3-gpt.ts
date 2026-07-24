// S3 — the "gpt" fallback section.
//
// This is an ESCALATION, not a per-keystroke search: the picker only fires it
// when S1+S2 came back empty/weak or the user pressed "search deeper" (Tab).
// It follows the search-session skill's methodology: cheaply pre-grep a
// candidate set of transcripts, hand those candidates + the query to a model
// via ONE non-interactive `codex exec` call (the CODEX-backed "luna" route —
// fast/cheap GPT on the Codex Pro subscription, NOT the Claude subscription),
// and ask the model to pick the sessions
// that are genuinely about the query and quote the matching snippet in a strict
// parseable format. We then map each pick back to a ResultLine using the real
// session metadata from sessions.ts — never inventing a session.
//
// Failure is visible, never silent and never faked: an empty candidate set, a
// timeout, a model error, or an unparseable reply all surface as a single
// degradation ResultLine (empty sessionId marks it non-selectable for the TUI).
// If the AbortSignal fires (the query changed) we stop and yield nothing.

import { readFileSync, rmSync } from "node:fs";
import { basename, join } from "node:path";
import { tmpdir, homedir } from "node:os";
import { spawn } from "node:child_process";
import type { CorpusConfig, ResultLine, ResumeRef, SectionSearcher, SessionMeta } from "./types.ts";
import { isExcludedTranscript } from "./types.ts";
import { loadConfig, MetaCache, buildChain } from "./sessions.ts";
import { loadIndex } from "./s2-indexer.ts";

/** Per-luna-call wall-clock cap (also bounded by the remaining budget). */
export const GPT_TIMEOUT_MS = 30_000;
/**
 * Total S3 budget measured from when the searcher starts (the debounce settle).
 * S3 is the deliberately-slow "thinking" section: it keeps re-querying luna for
 * MORE, different results until this elapses. A new keystroke aborts and the
 * budget restarts on the next settle.
 */
export const S3_BUDGET_MS = 30_000;
/** Cap on transcripts handed to the model per round — keeps each prompt cheap. */
export const MAX_CANDIDATES = 30;
/** Extra recent sessions mixed into later rounds so S3 can surface a fresh
 * conversation by TOPIC even when it shares few literal words with the query. */
const RECENT_ANGLE = 20;
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
    firstPrompt: "",
    lastPrompt: "",
    chain: [],
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
    firstPrompt: meta.summary,
    lastPrompt: meta.lastPrompt,
    chain: buildChain(meta, now),
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
  const filtered = (list: string[]) => list.filter((p) => !isExcludedTranscript(p));

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

/**
 * Load a condensed version of the search-session skill's methodology to prime
 * luna on HOW to investigate the corpus (topic = first+last user message; reason
 * about concept, not keywords; corpus layout). Reads the real skill file so the
 * model gets the actual instructions; falls back to an embedded summary.
 */
function methodology(): string {
  const path = join(process.env.HOME ?? homedir(), ".claude", "skills", "search-session", "SKILL.md");
  try {
    const raw = readFileSync(path, "utf8");
    // Keep it bounded — the Storage Layout + strategy sections are what matter.
    return raw.slice(0, 2500);
  } catch {
    return [
      "Claude/Codex sessions are JSONL transcripts. A session's TOPIC is captured",
      "by its first user message (what it started as) and last user message (where",
      "it ended up). To find a conversation, reason about what the user MEANS and",
      "match it to a session's topic even when the wording differs — do not rely on",
      "literal keyword overlap.",
    ].join("\n");
  }
}

/**
 * Build the reasoning prompt handed to luna. It is a CONCEPTUAL task, not a
 * keyword match: luna is told to reason about what the user is describing and
 * pick candidates whose topic matches that concept. `round`/`excludeIds` drive
 * the continuous find-more loop — later rounds push for DIFFERENT sessions from
 * another angle and forbid the ids already surfaced.
 */
export function buildPrompt(
  query: string,
  candidates: Candidate[],
  opts: { excludeIds?: string[]; round?: number } = {},
): string {
  const { excludeIds = [], round = 0 } = opts;
  const lines = candidates.map((c, i) => {
    const first = (c.meta.summary || "").replace(/\s+/g, " ").slice(0, 160);
    const last = (c.meta.lastPrompt || "").replace(/\s+/g, " ").slice(0, 160);
    const snip = c.snippet.replace(/\s+/g, " ").slice(0, 140);
    return `[${i + 1}] id=${c.meta.sessionId}\n    started: ${first}\n    ended: ${last}\n    match: ${snip}`;
  });
  const header =
    round === 0
      ? [
          `You are the deep "search harder" brain of a resume-search picker. Literal`,
          `keyword search and embedding search already ran; you are the fallback that`,
          `THINKS. Reason about what the user is actually describing and find the`,
          `session(s) whose TOPIC matches that concept — not mere word overlap.`,
        ]
      : [
          `Keep searching HARDER, from a DIFFERENT angle. You already surfaced some`,
          `sessions; now find OTHER, genuinely-relevant ones the user might mean.`,
          `Do NOT repeat any of these already-found ids: ${excludeIds.join(", ") || "(none)"}.`,
        ];
  return [
    `# How Claude/Codex session search works`,
    methodology(),
    ``,
    `# Task`,
    ...header,
    ``,
    `The user is trying to recall the conversation about: "${query}".`,
    `Think about the MEANING/intent, then judge each candidate by its started/ended`,
    `topic. A candidate matched some text but may be irrelevant — use judgement.`,
    ``,
    `Candidates:`,
    ...lines,
    ``,
    `Return ONLY lines in this exact format, most relevant first, one per session`,
    `you genuinely believe matches the user's concept:`,
    `HIT|<full-session-id>|<short reason (<=100 chars) tying it to the concept>`,
    `Use only ids from the candidate list above. If none genuinely fit, output the`,
    `single line: NONE`,
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
 * Resolve the CODEX_HOME the codex CLI should authenticate against. In
 * production $HOME is real so `~/.codex` is correct. Tests sandbox $HOME to point
 * the corpus at fixtures, so honor CODEX_HOME (explicit) or CX_HOME (the real
 * home, from which we derive `<home>/.codex`).
 */
function codexHome(): string | undefined {
  if (process.env.CODEX_HOME) return process.env.CODEX_HOME;
  if (process.env.CX_HOME) return join(process.env.CX_HOME, ".codex");
  return undefined; // inherit — codex defaults to $HOME/.codex
}

/**
 * Run the single S3 model call through the CODEX-backed "luna" route — the fast,
 * cheap GPT worker on the Codex Pro subscription (via the local `codex` CLI),
 * NOT the Claude subscription. `codex exec` is the non-interactive entry: the
 * prompt goes on stdin, the model's final answer is written to a temp file via
 * `--output-last-message` (clean, no event noise), and we read that back.
 * read-only sandbox + skip-git-repo-check keep it a pure text call that never
 * runs shell commands. Resolves the model text, or null on timeout/abort/error.
 */
export function runCodex(prompt: string, signal: AbortSignal, timeoutMs = GPT_TIMEOUT_MS): Promise<string | null> {
  return new Promise((resolve) => {
    if (signal.aborted) return resolve(null);
    const home = codexHome();
    const env = home ? { ...process.env, CODEX_HOME: home } : process.env;
    const lastMsgFile = join(tmpdir(), `resume-picker-s3-${process.pid}-${Date.now()}.txt`);
    const proc = spawn(
      "codex",
      ["exec", "--skip-git-repo-check", "-s", "read-only", "--color", "never", "-o", lastMsgFile, "-"],
      { stdio: ["pipe", "ignore", "ignore"], env },
    );
    let done = false;
    const finish = (val: string | null) => {
      if (done) return;
      done = true;
      clearTimeout(timer);
      signal.removeEventListener("abort", onAbort);
      try {
        rmSync(lastMsgFile, { force: true });
      } catch {}
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
    proc.on("error", () => finish(null));
    proc.on("close", () => {
      if (signal.aborted) return finish(null);
      let text: string | null = null;
      try {
        text = readFileSync(lastMsgFile, "utf8");
      } catch {
        text = null;
      }
      finish(text);
    });
    proc.stdin.on("error", () => {}); // ignore EPIPE if the child dies early
    proc.stdin.write(prompt);
    proc.stdin.end();
  });
}

/**
 * The RECENCY angle: the most-recently-active sessions from the S2 index (mtime
 * order — no FS walk). Mixed into later loop rounds so S3 can surface a fresh
 * conversation the user means even when it shares little literal text with the
 * query. Empty when the index isn't built yet.
 */
function recentCandidates(config: CorpusConfig, cache: MetaCache, limit: number): Candidate[] {
  let records: { path: string; mtimeMs: number }[] = [];
  try {
    records = [...loadIndex(config).values()].map((r) => ({ path: r.path, mtimeMs: r.mtimeMs }));
  } catch {
    return [];
  }
  records.sort((a, b) => b.mtimeMs - a.mtimeMs);
  const out: Candidate[] = [];
  for (const r of records) {
    if (out.length >= limit) break;
    if (isExcludedTranscript(r.path)) continue;
    const meta = cache.getMeta(r.path);
    if (!meta) continue;
    out.push({ path: meta.jsonlPath, meta, snippet: meta.summary.slice(0, 140) });
  }
  return out;
}

/** Merge candidate lists, deduped by session id, preserving order. */
function mergeCandidates(...lists: Candidate[][]): Candidate[] {
  const seen = new Set<string>();
  const out: Candidate[] = [];
  for (const list of lists)
    for (const c of list) {
      if (seen.has(c.meta.sessionId)) continue;
      seen.add(c.meta.sessionId);
      out.push(c);
    }
  return out;
}

/**
 * The S3 SectionSearcher — a CONTINUOUS conceptual finder. Fired on demand
 * (Tab "search deeper", or auto when S1+S2 came back empty). It repeatedly asks
 * luna to reason over candidate topics and surface relevant sessions, each round
 * pushing for MORE/different ones, until the S3_BUDGET_MS window (from start =
 * the debounce settle) closes or the query changes (abort). NEW results (deduped
 * by session id within S3) are yielded in arrival order so they append at the
 * bottom of the section without reshuffling. Yields exactly one degradation line
 * only if the whole run surfaced nothing; never fabricates a session.
 */
export const searchGpt: SectionSearcher = async function* (query: string, signal: AbortSignal) {
  const q = query.trim();
  if (!q) return;
  const deadline = Date.now() + S3_BUDGET_MS;

  const config = loadConfig();
  const cache = new MetaCache(config);

  const termCandidates = await gatherCandidates(q, config, cache, signal);
  if (signal.aborted) {
    cache.flush();
    return;
  }
  // No literal foothold at all: fast, honest degradation (don't burn the budget
  // asking luna about unrelated recent sessions).
  if (termCandidates.length === 0) {
    cache.flush();
    yield degraded("gpt: no matching transcripts");
    return;
  }

  const shown = new Set<string>(); // session ids already surfaced in S3 (no dupes)
  const now = Date.now();
  let round = 0;
  let emptyRounds = 0;
  let anyTimeout = false;

  while (Date.now() < deadline && !signal.aborted && emptyRounds < 2) {
    // Round 0: the literal-recall pool. Later rounds add the recency angle and
    // exclude everything already shown, so luna hunts genuinely-new sessions.
    const pool =
      round === 0
        ? termCandidates
        : mergeCandidates(termCandidates, recentCandidates(config, cache, RECENT_ANGLE)).filter(
            (c) => !shown.has(c.meta.sessionId),
          );
    if (pool.length === 0) break;

    const prompt = buildPrompt(q, pool.slice(0, MAX_CANDIDATES + RECENT_ANGLE), {
      excludeIds: [...shown],
      round,
    });
    const remaining = deadline - Date.now();
    if (remaining < 2_000) break; // not enough time for a useful call
    const out = await runCodex(prompt, signal, Math.min(GPT_TIMEOUT_MS, remaining));
    if (signal.aborted) {
      cache.flush();
      return;
    }
    if (out === null) {
      anyTimeout = true;
      emptyRounds++;
      round++;
      continue;
    }

    const byId = new Map(pool.map((c) => [c.meta.sessionId, c]));
    let newThisRound = 0;
    for (const hit of parseHits(out)) {
      if (signal.aborted) {
        cache.flush();
        return;
      }
      const cand = byId.get(hit.sessionId) ?? findByPrefix(byId, hit.sessionId);
      if (!cand) continue; // id not in the pool — never fabricate
      if (shown.has(cand.meta.sessionId)) continue; // no dupes within S3
      shown.add(cand.meta.sessionId);
      yield metaToResultLine(cand.meta, hit.snippet || cand.snippet, now);
      newThisRound++;
    }
    emptyRounds = newThisRound === 0 ? emptyRounds + 1 : 0;
    round++;
  }

  cache.flush();
  if (shown.size === 0) yield degraded(anyTimeout ? "gpt: timed out" : "gpt: no relevant sessions");
};

/** Canonical searcher export the picker's plugin loader binds to (`mod.search`). */
export const search = searchGpt;

/** Match a possibly-truncated id (model echoing a short form) against candidates. */
function findByPrefix(byId: Map<string, Candidate>, id: string): Candidate | undefined {
  if (id.length < 6) return undefined;
  for (const [full, cand] of byId) {
    if (full.startsWith(id) || id.startsWith(full)) return cand;
  }
  return undefined;
}
