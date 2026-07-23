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
