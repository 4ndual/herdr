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
import { sentinelLine, tokenizeQuery, isExcludedTranscript, recencyFactor } from "./types.ts";
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

    // Session-id short-circuit: if the query contains a full UUID or a standalone
    // 8+ hex prefix, the user is naming a specific session ("find this session
    // 0ccc55a3… in what folder"). Its own id rarely appears in its transcript
    // CONTENT, so content search misses it — resolve it directly by transcript
    // filename and surface it first. Emitted ids are skipped by content ranking.
    const shownIds = new Set<string>();
    const idMatch = q.match(/\b[0-9a-f]{8}(?:-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}|[0-9a-f-]{4,})?\b/i);
    if (idMatch) {
      const prefix = idMatch[0].toLowerCase();
      for (const root of rootPaths) {
        if (signal.aborted) return;
        const glob = new Bun.Glob(`**/${prefix}*.jsonl`);
        for (const rel of glob.scanSync({ cwd: root, onlyFiles: true })) {
          const full = `${root}/${rel}`;
          if (isExcludedTranscript(full)) continue;
          const meta = metaCache.getMeta(full);
          if (!meta || shownIds.has(meta.sessionId)) continue;
          shownIds.add(meta.sessionId);
          yield metaToResultLine(meta, meta.summary || meta.lastPrompt || "(matched by session id)");
        }
      }
      metaCache.flush();
      // A bare id query has no other useful terms to content-search.
      if (/^[0-9a-f-]{8,}$/i.test(q)) return;
    }

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
    // Per-term MATCH COUNTS per RAW path. We deliberately do NOT resolve
    // metadata here — that reads the whole transcript, and a common term matches
    // thousands of files. Counting is metadata-free; parsing waits for the
    // shortlist below. Counts (not just presence) let a session that is densely
    // about a rare term outrank one that merely name-drops several common ones.
    const termsByPath = new Map<string, Map<string, number>>();
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

          let counts = termsByPath.get(path);
          if (!counts) {
            counts = new Map();
            termsByPath.set(path, counts);
          }
          counts.set(term, (counts.get(term) ?? 0) + 1);
        }
      }
    } finally {
      signal.removeEventListener("abort", onAbort);
      onAbort();
    }
    if (signal.aborted) return;

    // Document frequency per term across all matched paths: how many distinct
    // transcripts contain each term. A term matching few files is distinctive
    // ("bitwarden", "cmix"); one matching most is generic ("fixing", "plugin").
    const df = new Map<string, number>();
    for (const counts of termsByPath.values())
      for (const t of counts.keys()) df.set(t, (df.get(t) ?? 0) + 1);
    const N = Math.max(1, termsByPath.size);
    // Smoothed IDF: rare term → large weight, near-universal term → ~log(2).
    const idf = (t: string): number => Math.log(1 + N / (df.get(t) ?? 1));

    // Shortlist paths by a metadata-free proxy of relevance (sum of idf over the
    // distinct terms present) so the parse budget goes to the strongest
    // candidates — a path dense in a rare term must not be dropped before we can
    // score it. Then parse metadata only for the shortlist.
    const pathScore = (counts: Map<string, number>): number => {
      let s = 0;
      for (const t of counts.keys()) s += idf(t);
      return s;
    };
    const byRelevance = [...termsByPath.entries()].sort((a, b) => pathScore(b[1]) - pathScore(a[1]));

    const recency = (m: SessionMeta) => Math.max(m.lastUserMs, m.lastAgentMs, m.createdMs);
    // Aggregate matched paths into sessions, summing per-term counts (a
    // conversation can live under several paths — cx bridges, copies).
    const bySession = new Map<string, { meta: SessionMeta; counts: Map<string, number> }>();
    for (const [path, counts] of byRelevance) {
      if (bySession.size >= SHORTLIST) break;
      if (signal.aborted) return;
      const meta = metaCache.getMeta(path);
      if (!meta) continue;
      const existing = bySession.get(meta.sessionId);
      if (existing) {
        for (const [t, c] of counts) existing.counts.set(t, (existing.counts.get(t) ?? 0) + c);
      } else {
        bySession.set(meta.sessionId, { meta, counts: new Map(counts) });
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
    const promptHits = (m: SessionMeta, terms: Iterable<string>): number => {
      const hay = `${m.summary} ${m.lastPrompt}`.toLowerCase();
      let n = 0;
      for (const t of terms) if (hay.includes(t)) n++;
      return n;
    };
    // Relevance = Σ over matched terms of idf(term)·(1+ln(count)). This is the
    // core discriminator: a session densely about a rare term (bitwarden ×98,
    // cmix ×2267) beats one that name-drops several common terms once each,
    // while matching more DISTINCT terms still adds up — AND-preference falls out
    // naturally instead of being a hard ×100 tier that saturates. Recency folds
    // in multiplicatively (relevance × decay, per the ranking spec) and a mild
    // prompt-topic boost rewards sessions whose own first/last prompt uses the
    // query words (a strong proxy for "this conversation is ABOUT the query").
    const now = Date.now();
    const relevance = (counts: Map<string, number>): number => {
      let s = 0;
      for (const [t, c] of counts) s += idf(t) * (1 + Math.log(c));
      return s;
    };
    const score = (h: { meta: SessionMeta; counts: Map<string, number> }): number =>
      relevance(h.counts) * (1 + 0.15 * promptHits(h.meta, h.counts.keys())) * recencyFactor(recency(h.meta), now);
    const ranked = [...bySession.values()].sort((a, b) => score(b) - score(a));

    const shown = ranked.filter((h) => !shownIds.has(h.meta.sessionId)).slice(0, TOP_N);
    for (const h of shown) {
      if (signal.aborted) return;
      const snippet = extractSnippet(h.meta, [...h.counts.keys()]) || h.meta.summary || "(no preview)";
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
