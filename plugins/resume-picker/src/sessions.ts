// Corpus discovery + per-session metadata cache.
//
// Roots are derived entirely from HOME/env (no hardcoded paths). Metadata is
// parsed lazily per jsonl and cached keyed by the file's mtime, so we never
// re-parse a whole transcript per keystroke — S1 only asks for meta on the ~30
// files that actually matched.

import { readdirSync, statSync, realpathSync, readFileSync, existsSync, mkdirSync, writeFileSync } from "node:fs";
import { join, basename, dirname } from "node:path";
import { homedir } from "node:os";
import type { Checkpoint, ChainSegment, CorpusConfig, CorpusRoot, ResultLine, SessionMeta, SessionOrigin } from "./types.ts";

/** Resolve corpus roots + cache dir from env/HOME. CCR dir override honored. */
export function loadConfig(): CorpusConfig {
  const home = process.env.HOME ?? homedir();
  const roots: CorpusRoot[] = [];

  // Daily claude.
  const dailyProjects = join(home, ".claude", "projects");
  if (existsSync(dailyProjects)) roots.push({ path: dailyProjects, origin: "daily" });

  // cx / CCR profiles. CCR_DIR points at a single profile's claude dir; when set
  // we use its projects dir, otherwise enumerate every profile under profiles/.
  const ccrDir = process.env.CCR_DIR;
  if (ccrDir) {
    const p = join(ccrDir, "projects");
    if (existsSync(p)) roots.push({ path: p, origin: "cx" });
  } else {
    const profilesRoot = join(home, ".claude-code-router", "profiles");
    if (existsSync(profilesRoot)) {
      for (const profile of safeReaddir(profilesRoot)) {
        const p = join(profilesRoot, profile, "claude", "projects");
        if (existsSync(p)) roots.push({ path: p, origin: "cx" });
      }
    }
  }

  // Codex.
  const codexSessions = join(home, ".codex", "sessions");
  if (existsSync(codexSessions)) roots.push({ path: codexSessions, origin: "codex" });

  const cacheDir =
    process.env.RESUME_PICKER_CACHE_DIR ??
    join(process.env.XDG_CACHE_HOME ?? join(home, ".cache"), "resume-picker");

  return { roots, cacheDir };
}

function safeReaddir(dir: string): string[] {
  try {
    return readdirSync(dir);
  } catch {
    return [];
  }
}

/** realpath a dir, falling back to the input when it can't be resolved. */
function realDir(p: string): string {
  try {
    return realpathSync(p);
  } catch {
    return p;
  }
}

/**
 * Origin of a transcript path given the configured roots (longest match wins).
 * Both sides are realpath-normalized so macOS symlinked roots (e.g. /tmp ->
 * /private/tmp) still match; callers pass an already-realpath'd file path.
 */
export function originForPath(path: string, config: CorpusConfig): SessionOrigin {
  let best: { origin: SessionOrigin; len: number } | undefined;
  for (const r of config.roots) {
    const rp = realDir(r.path);
    if ((path === rp || path.startsWith(rp + "/")) && (!best || rp.length > best.len)) {
      best = { origin: r.origin, len: rp.length };
    }
  }
  return best?.origin ?? "daily";
}

// ---------------------------------------------------------------------------
// Metadata cache
// ---------------------------------------------------------------------------

const CACHE_FILE = "meta-cache.json";

interface CacheShape {
  version: number;
  entries: Record<string, SessionMeta>; // keyed by realpath
}

const CACHE_VERSION = 1;

export class MetaCache {
  private config: CorpusConfig;
  private cachePath: string;
  private entries: Map<string, SessionMeta>;
  private dirty = false;

  constructor(config: CorpusConfig) {
    this.config = config;
    this.cachePath = join(config.cacheDir, CACHE_FILE);
    this.entries = new Map();
    this.load();
  }

  private load(): void {
    try {
      const raw = readFileSync(this.cachePath, "utf8");
      const parsed = JSON.parse(raw) as CacheShape;
      if (parsed.version === CACHE_VERSION) {
        for (const [k, v] of Object.entries(parsed.entries)) this.entries.set(k, v);
      }
    } catch {
      // No cache yet or unreadable — start empty.
    }
  }

  /** Persist to disk (best-effort). Call once after a batch of getMeta calls. */
  flush(): void {
    if (!this.dirty) return;
    try {
      if (!existsSync(this.config.cacheDir)) mkdirSync(this.config.cacheDir, { recursive: true });
      const shape: CacheShape = { version: CACHE_VERSION, entries: Object.fromEntries(this.entries) };
      writeFileSync(this.cachePath, JSON.stringify(shape));
      this.dirty = false;
    } catch {
      // Cache is an optimization; never fail the picker on a write error.
    }
  }

  /**
   * Metadata for a transcript. Deduped by realpath (a cx symlink bridging a
   * daily jsonl resolves to the same entry). Re-parses only when mtime changed.
   * Returns undefined if the file can't be stat'd.
   */
  getMeta(path: string): SessionMeta | undefined {
    let real: string;
    let mtimeMs: number;
    let birthMs: number;
    try {
      real = realpathSync(path);
      const st = statSync(real);
      mtimeMs = st.mtimeMs;
      birthMs = st.birthtimeMs || st.ctimeMs;
    } catch {
      return undefined;
    }

    const cached = this.entries.get(real);
    if (cached && cached.mtimeMs === mtimeMs) return cached;

    const meta = this.parse(real, mtimeMs, birthMs);
    if (meta) {
      this.entries.set(real, meta);
      this.dirty = true;
    }
    return meta;
  }

  /** All currently-cached entries (for the similar-index to consume). */
  all(): SessionMeta[] {
    return [...this.entries.values()];
  }

  private parse(real: string, mtimeMs: number, birthMs: number): SessionMeta | undefined {
    const origin = originForPath(real, this.config);
    let content: string;
    try {
      content = readFileSync(real, "utf8");
    } catch {
      return undefined;
    }

    let sessionId = basename(real, ".jsonl");
    let cwd = "";
    let createdMs = 0;
    let lastUserMs = 0;
    let lastAgentMs = 0;
    let summary = "";
    let lastPrompt = "";
    let chainParentId = "";
    const checkpoints: Checkpoint[] = [];

    const lines = content.split("\n");
    for (const line of lines) {
      if (!line) continue;
      let o: any;
      try {
        o = JSON.parse(line);
      } catch {
        continue;
      }

      if (origin === "codex") {
        const ts = tsMs(o.timestamp);
        if (ts && !createdMs) createdMs = ts;
        const p = o.payload;
        if (o.type === "session_meta" && p) {
          if (p.id) sessionId = p.id;
          if (p.cwd) cwd = p.cwd;
        } else if (o.type === "response_item" && p?.type === "message") {
          const text = codexText(p.content);
          if (p.role === "user") {
            if (ts) lastUserMs = ts;
            if (!summary && text) summary = text;
            if (text) lastPrompt = text;
          } else if (p.role === "assistant") {
            if (ts) lastAgentMs = ts;
          }
        }
        continue;
      }

      // claude / cx transcript.
      const ts = tsMs(o.timestamp);
      if (ts && !createdMs) createdMs = ts;
      if (!cwd && typeof o.cwd === "string") cwd = o.cwd;
      // Compaction boundary: one link in the conversation's chain (F5).
      if (o.type === "system" && o.subtype === "compact_boundary") {
        const cm = o.compactMetadata ?? {};
        checkpoints.push({
          ms: ts || 0,
          preTokens: Number(cm.preTokens) || 0,
          postTokens: Number(cm.postTokens) || 0,
          trigger: typeof cm.trigger === "string" ? cm.trigger : "",
        });
        continue;
      }
      if (o.type === "user" && o.message) {
        // Skip tool-result user turns (they carry toolUseResult, no human text).
        if (!o.toolUseResult) {
          const t = claudeText(o.message.content);
          if (t && !t.startsWith("<")) {
            if (ts) lastUserMs = ts;
            if (!summary) summary = t;
            lastPrompt = t;
          }
        }
      } else if (o.type === "assistant" && ts) {
        lastAgentMs = ts;
      }
    }

    if (!cwd) cwd = dirname(real); // fallback so resume has a cwd to encode
    if (!createdMs) createdMs = birthMs;
    // Newest-first so the expand view reads latest → oldest.
    checkpoints.reverse();

    return {
      sessionId,
      jsonlPath: real,
      origin,
      cwd,
      mtimeMs,
      createdMs,
      lastUserMs,
      lastAgentMs,
      summary: summary.slice(0, 200),
      lastPrompt: lastPrompt.slice(0, 200),
      checkpoints,
      chainParentId,
    };
  }
}

/**
 * Compact ago string from an epoch-ms timestamp: "2h", "3d", "5w". "" if 0.
 * Exported canonically as `formatAgo` so W2/W3 format the three sections
 * identically; `ago` is kept as the short internal alias.
 */
export function formatAgo(ms: number, now = Date.now()): string {
  if (!ms) return "";
  const s = Math.max(0, Math.floor((now - ms) / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h`;
  const d = Math.floor(h / 24);
  if (d < 7) return `${d}d`;
  const w = Math.floor(d / 7);
  if (w < 5) return `${w}w`;
  const mo = Math.floor(d / 30);
  if (mo < 12) return `${mo}mo`;
  return `${Math.floor(d / 365)}y`;
}

/** Short internal alias for {@link formatAgo}. */
export const ago = formatAgo;

/**
 * Compact token count for a checkpoint label: 59605 -> "59k".
 */
function tok(n: number): string {
  if (!n) return "?";
  if (n >= 1000) return `${Math.round(n / 1000)}k`;
  return String(n);
}

/** Build the F5 chain (expand view) for a session from its compaction checkpoints. */
export function buildChain(meta: SessionMeta, now = Date.now()): ChainSegment[] {
  const out: ChainSegment[] = [];
  for (const cp of meta.checkpoints ?? []) {
    const trig = cp.trigger ? `${cp.trigger} ` : "";
    out.push({
      kind: "checkpoint",
      sessionId: meta.sessionId,
      shortId: meta.sessionId.slice(0, 8),
      ago: ago(cp.ms, now),
      label: `${trig}compaction ${tok(cp.preTokens)}→${tok(cp.postTokens)} tokens`,
      resumeRef: null, // resuming the session id restores its latest state
    });
  }
  return out;
}

/** Build a ResultLine from session metadata + a matched snippet. */
export function metaToResultLine(
  meta: SessionMeta,
  snippet: string,
  now = Date.now(),
): ResultLine {
  return {
    snippet: snippet.replace(/\s+/g, " ").trim(),
    sessionId: meta.sessionId,
    shortId: meta.sessionId.slice(0, 8),
    lastUserAgo: ago(meta.lastUserMs, now),
    lastAgentAgo: ago(meta.lastAgentMs, now),
    createdAgo: ago(meta.createdMs, now),
    resumeRef: {
      sessionId: meta.sessionId,
      jsonlPath: meta.jsonlPath,
      origin: meta.origin,
      cwd: meta.cwd,
    },
    firstPrompt: meta.summary,
    lastPrompt: meta.lastPrompt,
    chain: buildChain(meta, now),
  };
}

function tsMs(ts: unknown): number {
  if (typeof ts !== "string") return 0;
  const n = Date.parse(ts);
  return Number.isNaN(n) ? 0 : n;
}

/** Extract plain text from a claude message.content (string or content blocks). */
function claudeText(content: unknown): string {
  if (typeof content === "string") return content.trim();
  if (Array.isArray(content)) {
    for (const block of content) {
      if (block && typeof block === "object" && (block as any).type === "text" && typeof (block as any).text === "string") {
        const t = (block as any).text.trim();
        if (t) return t;
      }
    }
  }
  return "";
}

/** Extract text from a codex response_item message content array. */
function codexText(content: unknown): string {
  if (Array.isArray(content)) {
    for (const block of content) {
      if (block && typeof block === "object") {
        const b = block as any;
        if ((b.type === "input_text" || b.type === "output_text" || b.type === "text") && typeof b.text === "string") {
          const t = b.text.trim();
          if (t && !t.startsWith("<")) return t;
        }
      }
    }
  }
  return "";
}
