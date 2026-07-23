// S1 exact/substring search: spawn `rg -i --fixed-strings --json` over the
// corpus roots, map each hit to a ResultLine via the mtime-keyed meta cache.
// Target <150ms perceived: per-file and global caps keep rg bounded, and we
// only parse meta for files that matched.

import type { SectionSearcher, ResultLine, SessionMeta } from "./types.ts";
import { sentinelLine } from "./types.ts";
import { loadConfig, MetaCache, metaToResultLine } from "./sessions.ts";

const PER_FILE_CAP = 3; // --max-count per file
const SCAN_CAP = 80; // stop scanning rg after this many distinct sessions matched
const TOP_N = 10; // F2: render only the ~10 most-recent, then a "… N more" line
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

    const proc = Bun.spawn(
      [
        "rg",
        "-i",
        "--fixed-strings",
        "--json",
        "--max-count",
        String(PER_FILE_CAP),
        "--no-messages",
        "--",
        q,
        ...rootPaths,
      ],
      { stdout: "pipe", stderr: "ignore" },
    );

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

    const seen = new Set<string>(); // sessionIds already collected (symlink dedupe)
    const hits: { meta: SessionMeta; snippet: string }[] = [];
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

          let ev: any;
          try {
            ev = JSON.parse(line);
          } catch {
            continue;
          }
          if (ev.type !== "match") continue;

          const path: string = ev.data?.path?.text ?? "";
          if (!path) continue;
          // Subagent sidechain transcripts carry no user conversation to resume —
          // skip them so they don't rank in as empty "(no preview)" rows.
          if (path.includes("/subagents/")) continue;
          const meta = metaCache.getMeta(path);
          if (!meta) continue;
          if (seen.has(meta.sessionId)) continue;
          seen.add(meta.sessionId);

          const lineText: string = ev.data?.lines?.text ?? "";
          // F1: never surface a raw JSON transcript line. extractSnippet returns
          // "" when it can't pull human text; fall back to the session summary.
          const snippet = extractSnippet(lineText, q) || meta.summary || "(no preview)";
          hits.push({ meta, snippet });

          if (hits.length >= SCAN_CAP) break outer;
        }
      }
    } finally {
      signal.removeEventListener("abort", onAbort);
      onAbort();
      metaCache.flush();
    }
    if (signal.aborted) return;

    // F2: rank by recency (last activity) and cap to TOP_N, then a "… N more" line.
    const recency = (m: SessionMeta) => Math.max(m.lastUserMs, m.lastAgentMs, m.createdMs);
    hits.sort((a, b) => recency(b.meta) - recency(a.meta));
    const shown = hits.slice(0, TOP_N);
    for (const h of shown) {
      if (signal.aborted) return;
      yield metaToResultLine(h.meta, h.snippet);
    }
    const more = hits.length - shown.length;
    if (more > 0) yield sentinelLine(`… ${more} more match${more === 1 ? "" : "es"} — refine your query`);
  };
}

/**
 * Turn a raw matched jsonl record into a readable snippet centered on the query.
 * Prefer parsing the record and pulling human text; fall back to the raw line.
 */
export function extractSnippet(lineText: string, query: string): string {
  let hay = "";
  try {
    const o = JSON.parse(lineText);
    hay = humanText(o);
  } catch {
    // Not JSON (rg matched a bare text line) — use it directly.
    hay = lineText;
  }
  hay = hay.replace(/\s+/g, " ").trim();
  // F1: a matched record with no extractable human text (tool output, metadata,
  // a summary object) must NOT render as raw JSON. Signal "no snippet" so the
  // caller substitutes the session summary.
  if (!hay || /^[[{]/.test(hay)) return "";

  const idx = hay.toLowerCase().indexOf(query.toLowerCase());
  if (idx === -1) return hay.slice(0, SNIPPET_WIDTH);

  const half = Math.floor((SNIPPET_WIDTH - query.length) / 2);
  const start = Math.max(0, idx - half);
  const end = Math.min(hay.length, idx + query.length + half);
  let s = hay.slice(start, end);
  if (start > 0) s = "…" + s;
  if (end < hay.length) s = s + "…";
  return s;
}

/** Best-effort extraction of human-readable text from a transcript record. */
function humanText(o: any): string {
  // claude/cx: message.content
  const content = o?.message?.content ?? o?.payload?.content ?? o?.content;
  const parts: string[] = [];
  if (typeof content === "string") {
    parts.push(content);
  } else if (Array.isArray(content)) {
    for (const b of content) {
      if (b && typeof b === "object") {
        if (typeof b.text === "string") parts.push(b.text);
        else if (typeof b.input === "object") parts.push(JSON.stringify(b.input));
      } else if (typeof b === "string") {
        parts.push(b);
      }
    }
  }
  if (typeof o?.content === "string") parts.push(o.content); // queue-operation content
  return parts.join(" ");
}
