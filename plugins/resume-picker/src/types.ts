// Canonical shared types for the resume-search picker.
//
// W1 owns this file. W2 (similar-index) and W3 (gpt) consume it. Keep it
// dependency-free so every module can import it without pulling in runtime code.

/** Which launcher originally owned a transcript — decides how we resume it. */
export type SessionOrigin =
  | "cx" // transcript lives under the CCR profile projects dir; resume via cx-resume
  | "daily" // transcript lives under ~/.claude; bridge into CCR dir then cx --resume
  | "codex"; // ~/.codex/sessions; display-only unless codex resume is trivial

/** Which section produced a result line (for rendering + selection grouping). */
export type SectionId = "exact" | "similar" | "gpt";

/**
 * Everything resume.ts needs to exec into a resumed session in the same pane.
 * Carried on every ResultLine so selection never has to re-discover the session.
 */
export interface ResumeRef {
  sessionId: string;
  /** realpath of the transcript jsonl this session was matched from. */
  jsonlPath: string;
  origin: SessionOrigin;
  /** cwd the session ran in — used to encode the CCR bridge project dir. */
  cwd: string;
}

/**
 * One rendered result row. Ago fields are pre-formatted compact strings
 * ("2h", "3d", "5w") so the TUI renders one line without doing time math.
 *
 * DEGRADATION SENTINEL (canonical contract, agreed across W1/W2/W3): a section
 * may yield a status/degradation line as a ResultLine with `sessionId === ""`
 * (empty `shortId`, empty `resumeRef` fields), carrying the human message in
 * `snippet` (e.g. "gpt: press Tab to search deeper", "similar: no local model").
 * Renderers MUST display such rows but MUST skip them in keyboard/mouse
 * selection — they are never resumable. Use `isSentinel(line)` to test.
 */
export interface ResultLine {
  /** Matched text, truncated by the searcher to a sane width; TUI truncates further to pane width. */
  snippet: string;
  sessionId: string;
  /** First 8 chars of sessionId, for the compact one-line format. */
  shortId: string;
  /** Ago since last USER message (e.g. "2h"). Empty string if unknown. */
  lastUserAgo: string;
  /** Ago since last agent activity (assistant/tool). Empty string if unknown. */
  lastAgentAgo: string;
  /** Ago since session created (first line mtime/timestamp). Empty string if unknown. */
  createdAgo: string;
  resumeRef: ResumeRef;
  /**
   * First and last USER prompt of the conversation, so the picker can show a
   * detail area under the selected row for recognition (F3). Empty when unknown
   * or for sentinel rows.
   */
  firstPrompt: string;
  lastPrompt: string;
  /**
   * Compaction/continuation chain for this conversation (F5). One ResultLine is
   * the LATEST segment (the resume target); `chain` lists earlier checkpoints /
   * segments the user can expand (→ / Tab) to recognise the conversation and,
   * where the runtime allows, resume from that point. Empty for a plain
   * single-segment session. Ordered newest-first.
   */
  chain: ChainSegment[];
}

/**
 * One earlier point in a conversation's compaction/continuation chain (F5).
 * A `checkpoint` is a compact_boundary WITHIN one session id (resuming the id
 * restores its latest state — the checkpoint is a recognition aid + boundary
 * marker). A `segment` is a distinct prior session id linked cross-file
 * (resumable in its own right).
 */
export interface ChainSegment {
  kind: "checkpoint" | "segment";
  /** Session id to resume for a `segment`; the parent id for a `checkpoint`. */
  sessionId: string;
  shortId: string;
  /** Ago since this checkpoint/segment (e.g. "2h"). */
  ago: string;
  /** Human label, e.g. "compacted 59k→3k tokens" or the segment's first prompt. */
  label: string;
  /** Resume target for a selectable `segment`; null for an informational checkpoint. */
  resumeRef: ResumeRef | null;
}

/** True if a ResultLine is a degradation/status sentinel (not selectable). */
export function isSentinel(line: ResultLine): boolean {
  return line.sessionId === "";
}

/**
 * Stopwords dropped from a natural-language query before per-term search. Covers
 * common English function words plus search-meta words users type when
 * describing a conversation ("chat", "session", "find", "where"). Shared by S1
 * (exact) and S3 (gpt) so both tokenize a query the same way.
 */
export const QUERY_STOPWORDS: ReadonlySet<string> = new Set([
  "a", "an", "and", "are", "as", "at", "be", "been", "but", "by", "can",
  "chat", "conversation", "could", "did", "do", "does", "find", "for", "from",
  "get", "got", "had", "has", "have", "how", "i", "if", "im", "in", "into",
  "is", "it", "its", "just", "me", "my", "no", "not", "of", "on", "or", "our",
  "out", "session", "so", "some", "that", "the", "their", "them", "then",
  "there", "they", "this", "to", "up", "us", "want", "was", "we", "were",
  "what", "when", "where", "which", "while", "who", "why", "will", "with",
  "would", "you", "your",
]);

/**
 * Tokenize a natural-language query into the significant terms to search for.
 * Lowercases, splits on non-alphanumerics, drops stopwords and 1-char tokens,
 * dedupes preserving order, and caps the count so a rambling query still yields
 * a bounded rg call. Returns [] when the query is empty or all-stopwords — the
 * caller then falls back to the raw trimmed query as a single literal term.
 */
export function tokenizeQuery(query: string, cap = 8): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  for (const raw of query.toLowerCase().split(/[^a-z0-9]+/)) {
    const w = raw.trim();
    if (w.length < 2 || QUERY_STOPWORDS.has(w) || seen.has(w)) continue;
    seen.add(w);
    out.push(w);
    if (out.length >= cap) break;
  }
  return out;
}

/**
 * Recency multiplier for ranking: the session's last-activity time decays a
 * relevance score so that, among comparably-relevant results, the most recent
 * (and actively-growing) conversation wins — that's almost always what the user
 * is reaching for. Bounded in [floor, 1] with a gentle half-life so a strong
 * older match is nudged, never buried. `lastMs` is epoch ms of last activity;
 * 0/unknown yields the floor.
 */
export function recencyFactor(
  lastMs: number,
  now = Date.now(),
  halfLifeDays = 21,
  floor = 0.6,
): number {
  if (!lastMs) return floor;
  const ageDays = Math.max(0, (now - lastMs) / 86_400_000);
  const decay = Math.pow(0.5, ageDays / halfLifeDays);
  return floor + (1 - floor) * decay;
}

/**
 * A transcript we never want to rank as a resumable session: subagent /
 * headless sidechain files (`agent-*.jsonl`, or anything under a `subagents/`
 * dir). They carry no user-driven conversation to resume, so S1/S2/S3 all skip
 * them. Kept here so every section applies the identical rule.
 */
export function isExcludedTranscript(path: string): boolean {
  if (path.includes("/subagents/")) return true;
  const base = path.slice(path.lastIndexOf("/") + 1);
  return base.startsWith("agent-");
}

/** Build a degradation/status sentinel ResultLine carrying `message` in snippet. */
export function sentinelLine(message: string): ResultLine {
  return {
    snippet: message,
    sessionId: "",
    shortId: "",
    lastUserAgo: "",
    lastAgentAgo: "",
    createdAgo: "",
    resumeRef: { sessionId: "", jsonlPath: "", origin: "daily", cwd: "" },
    firstPrompt: "",
    lastPrompt: "",
    chain: [],
  };
}

/**
 * A section searcher: given a query and an AbortSignal, yields ResultLines as
 * it finds them. Sections stream independently — the TUI renders each line the
 * moment it is yielded and never blocks a fast section on a slow one. A new
 * keystroke aborts the signal; a well-behaved searcher stops promptly.
 *
 * S2 (src/s2-similar.ts) and S3 (src/s3-gpt.ts) each export a value of this type.
 * If the module is absent or throws, the TUI shows a visible degradation line —
 * searchers must never yield fake results.
 */
export type SectionSearcher = (
  query: string,
  signal: AbortSignal,
) => AsyncIterable<ResultLine>;

/** Per-session metadata cache entry, keyed by jsonl mtime in sessions.ts. */
export interface SessionMeta {
  sessionId: string;
  /** realpath of the transcript (canonical after symlink-dedupe). */
  jsonlPath: string;
  origin: SessionOrigin;
  cwd: string;
  /** jsonl mtime in ms — the cache key; re-parse only when this changes. */
  mtimeMs: number;
  /** Epoch ms of session creation (first entry / file birth). */
  createdMs: number;
  /** Epoch ms of the last USER message, or 0 if none seen. */
  lastUserMs: number;
  /** Epoch ms of the last agent activity (assistant/tool), or 0 if none. */
  lastAgentMs: number;
  /** First user prompt / summary line, for similar-index docs and previews. */
  summary: string;
  /** Last USER prompt text — pairs with `summary` (first prompt) for F3 recognition. */
  lastPrompt: string;
  /**
   * Compaction checkpoints found in this transcript (compact_boundary records),
   * newest-first. Drives the F5 expand view. Empty for uncompacted sessions.
   */
  checkpoints: Checkpoint[];
  /**
   * If this transcript continues a prior session id (cross-file resume/fork link),
   * the parent session id; empty otherwise. Reserved for cross-file chains — the
   * current Claude Code format compacts in-file, so this is usually empty.
   */
  chainParentId: string;
}

/** A compaction boundary within a single transcript. */
export interface Checkpoint {
  /** Epoch ms of the compaction. */
  ms: number;
  /** Tokens before compaction (from compactMetadata), 0 if unknown. */
  preTokens: number;
  /** Tokens after compaction, 0 if unknown. */
  postTokens: number;
  /** "manual" | "auto" | "" — the compaction trigger. */
  trigger: string;
}

/**
 * Corpus roots discovered from HOME/env. Each root is a directory containing a
 * `projects/<encoded-cwd>/<id>.jsonl` (claude/cx) or session jsonls (codex).
 */
export interface CorpusRoot {
  /** Absolute path to the root that holds transcripts. */
  path: string;
  origin: SessionOrigin;
}

/** Resolved, env-derived configuration shared across modules. */
export interface CorpusConfig {
  roots: CorpusRoot[];
  /** ~/.cache/resume-picker or $RESUME_PICKER_CACHE_DIR. */
  cacheDir: string;
}
