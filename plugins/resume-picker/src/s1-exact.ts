// S1 exact/substring search.
//
// A natural-language query ("chat where i was upgrading hr and fixing the
// plugins") is NOT one literal string — searching it verbatim finds nothing. So
// we tokenize the query (drop stopwords), run ONE `rg -o -w -F` scan for the
// alternation of the significant terms across the corpus roots, and rank each
// session by how many DISTINCT query terms it hit (AND-preferred: all-terms
// sessions first, then partial). One scan keeps it perceived-instant (~0.25s on
// the 1.8G corpus); metadata comes from the mtime-keyed meta cache.

import type { SectionSearcher, ResultLine, SessionMeta } from "./types.ts";
import { sentinelLine, tokenizeQuery, isExcludedTranscript } from "./types.ts";
import { loadConfig, MetaCache, metaToResultLine } from "./sessions.ts";

// Per-file match cap. Must be high enough that a file dense in common terms
// ("hr", "plugins" appear thousands of times) still reveals its rarer terms
// before rg stops emitting for that file — a low cap undercounts distinct terms.
const PER_FILE_CAP = 500;
const TOP_N = 10; // render only the ~10 best, then a "… N more" line
// After counting term hits per file we parse metadata (which reads the whole
// transcript) only for this many top-ranked candidates — a common term like
// "hr" matches thousands of files, and parsing all of them would take seconds.
// Ranking by distinct-term count needs no metadata, so the shortlist is the only
// place we pay the parse cost. Generous enough that recency ordering within the
// top tier is faithful.
const SHORTLIST = 150;
const SNIPPET_WIDTH = 120; // chars of context around the match

/**
 * Factory so the TUI can share one MetaCache across sections/keystrokes.
 * Falls back to a fresh config+cache if none is injected.
 */
export function makeExactSearcher(cache?: MetaCache): SectionSearcher {
  const config = loadConfig();
  const metaCache = cache ?? new MetaCache(config);
  const rootPaths = config.roots.map((r) => r.path);

  return async function* exact(query: string, signal: AbortSignal): AsyncIterable<ResultLine> {
    const q = query.trim();
    if (!q || rootPaths.length === 0) return;

    // Tokenize; if the query is all-stopwords, fall back to the raw phrase so a
    // single meaningful token or an exotic string still searches.
    const terms = tokenizeQuery(q);
    const needles = terms.length > 0 ? terms : [q.toLowerCase()];

    const args = [
      "-o", // print only the matched token (not the whole giant jsonl line)
      "-w", // whole-word: keeps short terms like "hr" from matching "chrome"
      "-i",
      "-F", // fixed strings — terms are alphanumeric, no regex surprises
      "--no-heading",
      "-H", // always prefix the path so we can attribute the match
      "--no-messages",
      "--max-count",
      String(PER_FILE_CAP),
      "--glob",
      "!agent-*.jsonl", // never rank subagent/headless transcripts
    ];
    for (const n of needles) args.push("-e", n);
    args.push("--", ...rootPaths);

    const proc = Bun.spawn(["rg", ...args], { stdout: "pipe", stderr: "ignore" });

    const onAbort = () => {
      try {
        proc.kill();
      } catch {
        // already gone
      }
    };
    if (signal.aborted) {
      onAbort();
      return;
    }
    signal.addEventListener("abort", onAbort, { once: true });

    const needleSet = new Set(needles);
    // Distinct terms matched per RAW path. We deliberately do NOT resolve
    // metadata here — that reads the whole transcript, and a common term matches
    // thousands of files. Counting is metadata-free; parsing waits for the
    // shortlist below.
    const termsByPath = new Map<string, Set<string>>();
    let carry = "";

    try {
      const reader = proc.stdout.getReader();
      const decoder = new TextDecoder();
      outer: while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        carry += decoder.decode(value, { stream: true });
        let nl: number;
        while ((nl = carry.indexOf("\n")) !== -1) {
          const line = carry.slice(0, nl);
          carry = carry.slice(nl + 1);
          if (signal.aborted) break outer;
          if (!line) continue;

          // `-o -H` output is `<path>:<matched>`. Paths in the corpus contain no
          // colons and the matched token is our (colon-free) term, so the last
          // colon splits them unambiguously.
          const colon = line.lastIndexOf(":");
          if (colon <= 0) continue;
          const path = line.slice(0, colon);
          const term = line.slice(colon + 1).toLowerCase();
          if (!needleSet.has(term)) continue;
          if (isExcludedTranscript(path)) continue;

          let set = termsByPath.get(path);
          if (!set) {
            set = new Set();
            termsByPath.set(path, set);
          }
          set.add(term);
        }
      }
    } finally {
      signal.removeEventListener("abort", onAbort);
      onAbort();
    }
    if (signal.aborted) return;

    // Rank candidate PATHS by distinct-term count (AND-preferred) — no metadata
    // needed — then parse metadata only for the shortlist. Dedupe by session
    // realpath (symlink bridges) keeping the highest term count.
    const byCount = [...termsByPath.entries()].sort((a, b) => b[1].size - a[1].size);

    const recency = (m: SessionMeta) => Math.max(m.lastUserMs, m.lastAgentMs, m.createdMs);
    const bySession = new Map<string, { meta: SessionMeta; terms: Set<string> }>();
    for (const [path, terms] of byCount) {
      if (bySession.size >= SHORTLIST) break;
      if (signal.aborted) return;
      const meta = metaCache.getMeta(path);
      if (!meta) continue;
      // Dedupe by session id (a transcript can exist under several paths — cx
      // bridges, copies — all the same conversation).
      const existing = bySession.get(meta.sessionId);
      if (existing) {
        for (const t of terms) existing.terms.add(t);
      } else {
        bySession.set(meta.sessionId, { meta, terms: new Set(terms) });
      }
    }
    metaCache.flush();

    // A huge transcript mentions half the corpus's vocabulary somewhere (tool
    // output, pasted logs, cross-references), so distinct-term-count saturates
    // and can't tell the session that's ABOUT the query from one that merely
    // name-drops the terms. The discriminator is WHERE the terms appear: a
    // session whose own user prompts use the query words is what the user is
    // describing. We can't cheaply see every user turn, but the first and last
    // prompts (cached in meta) are a strong, instant proxy for the topic.
    const promptHits = (m: SessionMeta, terms: Set<string>): number => {
      const hay = `${m.summary} ${m.lastPrompt}`.toLowerCase();
      let n = 0;
      for (const t of terms) if (hay.includes(t)) n++;
      return n;
    };
    const ranked = [...bySession.values()].sort((a, b) => {
      // AND-preferred: most distinct query terms first. This is the recall gate
      // and, for a specific query, is already near-unique to the right session.
      if (b.terms.size !== a.terms.size) return b.terms.size - a.terms.size;
      // Within a tier (e.g. an ambiguous query many sessions match in full),
      // prefer the session whose own user prompts use the query words — that's
      // the one the query is describing — then the more recent.
      const pa = promptHits(a.meta, a.terms);
      const pb = promptHits(b.meta, b.terms);
      if (pb !== pa) return pb - pa;
      return recency(b.meta) - recency(a.meta);
    });

    const shown = ranked.slice(0, TOP_N);
    for (const h of shown) {
      if (signal.aborted) return;
      const snippet = extractSnippet(h.meta, [...h.terms]) || h.meta.summary || "(no preview)";
      yield metaToResultLine(h.meta, snippet);
    }
    const more = ranked.length - shown.length;
    if (more > 0) yield sentinelLine(`… ${more} more match${more === 1 ? "" : "es"} — refine your query`);
  };
}

/**
 * Build a readable snippet for a shown result. Prefer the session's first user
 * prompt (already cached, cheap) centered on a matched term; fall back to the
 * plain summary. Never re-reads the transcript — S1 stays instant.
 */
export function extractSnippet(meta: SessionMeta, terms: string[]): string {
  const hay = (meta.summary || meta.lastPrompt || "").replace(/\s+/g, " ").trim();
  if (!hay) return "";
  const low = hay.toLowerCase();
  let idx = -1;
  for (const t of terms) {
    const i = low.indexOf(t);
    if (i !== -1 && (idx === -1 || i < idx)) idx = i;
  }
  if (idx === -1) return hay.slice(0, SNIPPET_WIDTH);
  const half = Math.floor(SNIPPET_WIDTH / 2);
  const start = Math.max(0, idx - half);
  const end = Math.min(hay.length, idx + half);
  let s = hay.slice(start, end);
  if (start > 0) s = "…" + s;
  if (end < hay.length) s = s + "…";
  return s;
}
