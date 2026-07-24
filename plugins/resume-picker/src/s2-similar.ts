// "similar" section (S2): semantic search over the embedding index.
//
// Implements the SectionSearcher contract from types.ts. Embeds the query with
// the same local MiniLM model the indexer used, does an in-memory cosine top-k
// against the index records, applies a junk threshold, and maps hits to
// ResultLine via W1's MetaCache (so timestamps/ago fields stay consistent with
// S1). Streams results as it produces them and aborts promptly on a new
// keystroke. If no local model or index exists, yields a single visible
// degradation line — never fake results.

import { MetaCache, loadConfig, metaToResultLine } from "./sessions.ts";
import type { CorpusConfig, ResultLine, SectionSearcher } from "./types.ts";
import { sentinelLine } from "./types.ts";
import { Embedder, loadIndex, NoModelError, type IndexRecord } from "./s2-indexer.ts";

/** How many top hits to surface. */
const TOP_K = 10;
/**
 * Minimum cosine similarity to count as a real "similar" hit. MiniLM unrelated
 * pairs sit near 0; loosely related paraphrases land ~0.3-0.5. 0.25 keeps recall
 * while dropping junk. Env-overridable for tuning.
 */
const SIM_THRESHOLD = Number(process.env.RESUME_PICKER_SIM_THRESHOLD ?? "0.25");

function cosine(a: Float32Array, b: Float32Array): number {
  // Both operands are unit-normalized (indexer + embedder normalize), so the
  // dot product IS the cosine similarity.
  let s = 0;
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) s += a[i] * b[i];
  return s;
}

/** A degradation line the TUI renders verbatim (canonical sentinel, non-selectable). */
function degradationLine(text: string): ResultLine {
  return sentinelLine(text);
}

/** Build a SectionSearcher bound to a config (defaults to env-derived config). */
export function makeSimilarSearcher(config: CorpusConfig = loadConfig()): SectionSearcher {
  const embedder = new Embedder(config.cacheDir);
  const meta = new MetaCache(config);

  return async function* similar(query: string, signal: AbortSignal): AsyncIterable<ResultLine> {
    const q = query.trim();
    if (!q) return;

    // Load the index up front; if empty, tell the user (indexer may still be building).
    const index = loadIndex(config);
    if (index.size === 0) {
      yield degradationLine("similar: index not built yet — run resume-picker --reindex");
      return;
    }

    let qVec: Float32Array;
    try {
      qVec = await embedder.embed(q);
    } catch (e) {
      if (e instanceof NoModelError) {
        yield degradationLine("similar: no local model available");
      } else {
        yield degradationLine("similar: embedding failed");
      }
      return;
    }
    if (signal.aborted) return;

    // Cosine top-k over the in-memory records (13k * 384 dot products ~ a few ms).
    const scored: { rec: IndexRecord; score: number }[] = [];
    for (const rec of index.values()) {
      const score = cosine(qVec, rec.vec);
      if (score >= SIM_THRESHOLD) scored.push({ rec, score });
    }
    scored.sort((a, b) => b.score - a.score);
    if (signal.aborted) return;

    const now = Date.now();
    let emitted = 0;
    const seen = new Set<string>(); // dedupe a session that indexed under >1 path
    for (const { rec, score } of scored) {
      if (signal.aborted) return;
      if (emitted >= TOP_K) break;
      // Prefer W1's rich meta (fresh timestamps); fall back to the stored snippet.
      const m = meta.getMeta(rec.path);
      if (!m) continue; // file vanished since indexing — skip rather than show a dead row
      if (seen.has(m.sessionId)) continue;
      seen.add(m.sessionId);
      yield metaToResultLine(m, rec.snippet || m.summary, now);
      emitted++;
    }
    meta.flush();

    if (emitted === 0) {
      yield degradationLine("similar: no semantically related sessions");
    }
  };
}

/** Default export: the SectionSearcher the TUI wires in for S2. */
const similar: SectionSearcher = makeSimilarSearcher();
export default similar;
