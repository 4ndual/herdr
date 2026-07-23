// Incremental embedding index for the "similar" section (S2).
//
// Builds one dense vector per session from a per-session document (concatenated
// user messages + summary lines, capped). The index is rebuilt incrementally:
// only sessions whose jsonl mtime changed are re-embedded; a full scan happens
// only on the first build. Runnable standalone (`bun s2-indexer.ts`) and
// importable by s2-similar.ts.
//
// Model: all-MiniLM-L6-v2 ONNX (384-dim), already on disk in the local model
// store, executed in-process via @huggingface/transformers (onnxruntime-node).
// No server, no network, no pull. If the model can't be resolved, the indexer
// and searcher surface a typed error so S2 degrades to a visible line instead of
// faking results.

import {
  readdirSync,
  statSync,
  realpathSync,
  readFileSync,
  writeFileSync,
  existsSync,
  mkdirSync,
  symlinkSync,
  renameSync,
  rmSync,
} from "node:fs";
import { join, basename } from "node:path";
import { homedir } from "node:os";
import { loadConfig } from "./sessions.ts";
import type { CorpusConfig, SessionOrigin } from "./types.ts";

// ---------------------------------------------------------------------------
// Tunables (named constants, env-overridable where it matters)
// ---------------------------------------------------------------------------

/** Model output dimensionality — all-MiniLM-L6-v2. */
export const EMBED_DIM = 384;
/** Local model id used both as the store folder name and transformers.js id. */
const MODEL_ID = "all-MiniLM-L6-v2";
/** HF cache folder that ships the ONNX weights + tokenizer. */
const MODEL_HF_DIR = "models--Qdrant--all-MiniLM-L6-v2-onnx";
/** Total characters of session text retained for the doc (chunked before embed). */
const DOC_CHAR_CAP = 6000;
/** Max user messages concatenated into a doc (newest sessions have thousands). */
const DOC_MAX_MESSAGES = 80;
/** Per-message char cap — long pasted/injected prompts get their head only. */
const MSG_CHAR_CAP = 500;
/**
 * MiniLM only attends to ~256 tokens (~1000 chars). Embedding the whole doc head
 * would capture only the first (often boilerplate) message, so we split the doc
 * into windows, embed each, and mean-pool — representing the entire session.
 */
const CHUNK_CHARS = 900;
const MAX_CHUNKS = 6;
/** Snippet length stored per record for the result preview. */
const SNIPPET_CHARS = 160;
/** Index format version — bump to force a full rebuild on shape changes. */
const INDEX_VERSION = 2;

/**
 * Markers of agent-injected / boilerplate turns that carry no session topic
 * (skill preambles, cron prompts, goal-observer system text). Dropping these
 * keeps the embedding about what the human was actually working on.
 */
const BOILERPLATE_MARKERS = [
  "you are ",
  "base directory for this skill",
  "run the /",
  "$(date",
  "read-only research",
  "read-only, do not edit",
  "you are naming the main",
  "you are the goal observer",
  "adversarial hardening iteration",
];

/** Thrown when no usable local embedding model can be resolved. */
export class NoModelError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "NoModelError";
  }
}

// ---------------------------------------------------------------------------
// Model resolution + embedder
// ---------------------------------------------------------------------------

/** Root of the local model store (`~/Models` by default, `MODELS_DIR` override). */
function modelsRoot(): string {
  return process.env.MODELS_DIR ?? join(process.env.HOME ?? homedir(), "Models");
}

/**
 * Locate the MiniLM ONNX snapshot in the local HF cache and assemble it into the
 * directory layout transformers.js expects (`<root>/<id>/onnx/model.onnx` plus
 * config + tokenizer files), under the picker cache dir. Idempotent. Returns the
 * localModelPath to hand to transformers.js. Throws NoModelError if absent.
 */
export function resolveLocalModel(cacheDir: string): { localModelPath: string; modelId: string } {
  const hubDir = join(modelsRoot(), "llm", "hf", "hub", MODEL_HF_DIR, "snapshots");
  let snapshot: string | undefined;
  try {
    for (const s of readdirSync(hubDir)) {
      const dir = join(hubDir, s);
      if (existsSync(join(dir, "model.onnx")) && existsSync(join(dir, "tokenizer.json"))) {
        snapshot = dir;
        break;
      }
    }
  } catch {
    // hub dir missing — falls through to NoModelError below.
  }
  if (!snapshot) {
    throw new NoModelError(
      `all-MiniLM-L6-v2 ONNX not found under ${hubDir} (expected model.onnx + tokenizer.json)`,
    );
  }

  const localModelPath = join(cacheDir, "model");
  const modelDir = join(localModelPath, MODEL_ID);
  const onnxDir = join(modelDir, "onnx");
  mkdirSync(onnxDir, { recursive: true });

  // Symlink the weights + tokenizer from the shared store (no copy), and write
  // the minimal BERT config transformers.js needs (the Qdrant repo omits them).
  linkForce(join(snapshot, "model.onnx"), join(onnxDir, "model.onnx"));
  linkForce(join(snapshot, "tokenizer.json"), join(modelDir, "tokenizer.json"));
  writeFileSync(
    join(modelDir, "config.json"),
    JSON.stringify({ model_type: "bert", hidden_size: EMBED_DIM, architectures: ["BertModel"] }),
  );
  writeFileSync(
    join(modelDir, "tokenizer_config.json"),
    JSON.stringify({ model_max_length: 256, do_lower_case: true, tokenizer_class: "BertTokenizer" }),
  );

  return { localModelPath, modelId: MODEL_ID };
}

function linkForce(target: string, link: string): void {
  try {
    rmSync(link, { force: true });
  } catch {
    // ignore — symlink below will surface a real failure
  }
  symlinkSync(target, link);
}

/** Lazy in-process embedder. Loads the ONNX pipeline once, reuses it. */
export class Embedder {
  private extractor: any = null;
  private readonly cacheDir: string;

  constructor(cacheDir: string) {
    this.cacheDir = cacheDir;
  }

  private async ensure(): Promise<any> {
    if (this.extractor) return this.extractor;
    const { localModelPath, modelId } = resolveLocalModel(this.cacheDir);
    // Import lazily so a missing dep / model degrades instead of crashing import.
    const tf = await import("@huggingface/transformers");
    tf.env.allowRemoteModels = false;
    tf.env.localModelPath = localModelPath;
    this.extractor = await tf.pipeline("feature-extraction", modelId, { dtype: "fp32" });
    return this.extractor;
  }

  /** Embed one string → unit-normalized Float32Array of length EMBED_DIM. */
  async embed(text: string): Promise<Float32Array> {
    const extractor = await this.ensure();
    const out = await extractor(text || " ", { pooling: "mean", normalize: true });
    return out.data as Float32Array;
  }

  /**
   * Embed a long document faithfully: split into ~CHUNK_CHARS windows (each
   * within MiniLM's token budget), embed each, mean-pool, and renormalize. This
   * represents the whole session instead of just its first message. Single-chunk
   * docs go straight through embed().
   */
  async embedDoc(text: string): Promise<Float32Array> {
    const chunks = chunkText(text, CHUNK_CHARS, MAX_CHUNKS);
    if (chunks.length <= 1) return this.embed(chunks[0] ?? text);
    const acc = new Float32Array(EMBED_DIM);
    for (const c of chunks) {
      const v = await this.embed(c);
      for (let i = 0; i < EMBED_DIM; i++) acc[i] += v[i];
    }
    let norm = 0;
    for (let i = 0; i < EMBED_DIM; i++) norm += acc[i] * acc[i];
    norm = Math.sqrt(norm) || 1;
    for (let i = 0; i < EMBED_DIM; i++) acc[i] /= norm;
    return acc;
  }
}

/** Split text into up to `max` windows of ~`size` chars, breaking on whitespace. */
function chunkText(text: string, size: number, max: number): string[] {
  const out: string[] = [];
  let i = 0;
  while (i < text.length && out.length < max) {
    let end = Math.min(i + size, text.length);
    if (end < text.length) {
      const sp = text.lastIndexOf(" ", end);
      if (sp > i + size / 2) end = sp;
    }
    out.push(text.slice(i, end).trim());
    i = end;
  }
  return out.filter((c) => c.length > 0);
}

// ---------------------------------------------------------------------------
// Index record format
// ---------------------------------------------------------------------------

export interface IndexRecord {
  sessionId: string;
  /** realpath of the transcript (canonical after symlink dedupe). */
  path: string;
  origin: SessionOrigin;
  mtimeMs: number;
  /** Display snippet (first meaningful user text), pre-trimmed. */
  snippet: string;
  /** Unit-normalized embedding. */
  vec: Float32Array;
}

interface IndexHeader {
  version: number;
  dim: number;
  model: string;
  count: number;
  builtAt: number;
}

/** Directory that holds the index (`<cacheDir>/index`). */
export function indexDir(config: CorpusConfig): string {
  return join(config.cacheDir, "index");
}

function recordsPath(config: CorpusConfig): string {
  return join(indexDir(config), "records.ndjson");
}

/** Serialize a Float32Array as base64 (little-endian, host order = LE on arm64). */
function encodeVec(v: Float32Array): string {
  return Buffer.from(v.buffer, v.byteOffset, v.byteLength).toString("base64");
}

function decodeVec(b64: string): Float32Array {
  const buf = Buffer.from(b64, "base64");
  return new Float32Array(buf.buffer, buf.byteOffset, buf.byteLength / 4);
}

/** Load existing records keyed by realpath. Empty map on any read/version miss. */
export function loadIndex(config: CorpusConfig): Map<string, IndexRecord> {
  const map = new Map<string, IndexRecord>();
  const file = recordsPath(config);
  let raw: string;
  try {
    raw = readFileSync(file, "utf8");
  } catch {
    return map;
  }
  const lines = raw.split("\n");
  if (!lines.length) return map;
  let header: IndexHeader | undefined;
  try {
    header = JSON.parse(lines[0]) as IndexHeader;
  } catch {
    return map;
  }
  if (!header || header.version !== INDEX_VERSION || header.dim !== EMBED_DIM) return map;
  for (let i = 1; i < lines.length; i++) {
    const line = lines[i];
    if (!line) continue;
    try {
      const o = JSON.parse(line);
      map.set(o.path, {
        sessionId: o.sessionId,
        path: o.path,
        origin: o.origin,
        mtimeMs: o.mtimeMs,
        snippet: o.snippet,
        vec: decodeVec(o.vec),
      });
    } catch {
      // skip corrupt line
    }
  }
  return map;
}

function writeIndex(config: CorpusConfig, records: Iterable<IndexRecord>): number {
  const dir = indexDir(config);
  mkdirSync(dir, { recursive: true });
  const arr = [...records];
  const header: IndexHeader = {
    version: INDEX_VERSION,
    dim: EMBED_DIM,
    model: MODEL_ID,
    count: arr.length,
    builtAt: Date.now(),
  };
  const out: string[] = [JSON.stringify(header)];
  for (const r of arr) {
    out.push(
      JSON.stringify({
        sessionId: r.sessionId,
        path: r.path,
        origin: r.origin,
        mtimeMs: r.mtimeMs,
        snippet: r.snippet,
        vec: encodeVec(r.vec),
      }),
    );
  }
  // Atomic write: serialize to a temp file then rename over the target so a
  // concurrent reader never sees a half-written index.
  const target = recordsPath(config);
  const tmp = target + ".tmp";
  writeFileSync(tmp, out.join("\n"));
  renameSync(tmp, target);
  return arr.length;
}

// ---------------------------------------------------------------------------
// Corpus discovery + document extraction
// ---------------------------------------------------------------------------

interface Discovered {
  path: string; // realpath
  origin: SessionOrigin;
  mtimeMs: number;
}

/** Walk a root collecting *.jsonl files with their realpath + mtime. */
function walkJsonl(root: string, origin: SessionOrigin, seen: Set<string>, out: Discovered[]): void {
  let entries: string[];
  try {
    entries = readdirSync(root);
  } catch {
    return;
  }
  for (const name of entries) {
    const full = join(root, name);
    let st;
    try {
      st = statSync(full); // follows symlinks; dir vs file
    } catch {
      continue;
    }
    if (st.isDirectory()) {
      walkJsonl(full, origin, seen, out);
    } else if (st.isFile() && name.endsWith(".jsonl")) {
      let real: string;
      try {
        real = realpathSync(full);
      } catch {
        continue;
      }
      if (seen.has(real)) continue; // symlink dedupe
      seen.add(real);
      out.push({ path: real, origin, mtimeMs: st.mtimeMs });
    }
  }
}

/** All transcripts across configured roots, deduped by realpath. */
export function discoverSessions(config: CorpusConfig): Discovered[] {
  const out: Discovered[] = [];
  const seen = new Set<string>();
  for (const r of config.roots) walkJsonl(r.path, r.origin, seen, out);
  return out;
}

/** Build the per-session embedding document + a display snippet. */
export function buildDoc(real: string, origin: SessionOrigin): { doc: string; snippet: string } {
  let content: string;
  try {
    content = readFileSync(real, "utf8");
  } catch {
    return { doc: "", snippet: "" };
  }
  const messages: string[] = [];
  let snippet = "";

  for (const line of content.split("\n")) {
    if (!line || messages.length >= DOC_MAX_MESSAGES) continue;
    let o: any;
    try {
      o = JSON.parse(line);
    } catch {
      continue;
    }

    let text = "";
    if (origin === "codex") {
      const p = o.payload;
      if (o.type === "response_item" && p?.type === "message" && p.role === "user") {
        text = codexText(p.content);
      }
    } else {
      if (o.type === "user" && o.message && !o.toolUseResult) {
        text = claudeText(o.message.content);
      } else if (o.type === "summary" && typeof o.summary === "string") {
        text = o.summary;
      }
    }
    text = text.replace(/\s+/g, " ").trim();
    if (!text || text.startsWith("<")) continue;
    if (isBoilerplate(text)) continue;
    // First surviving human turn is the display snippet.
    if (!snippet) snippet = text.slice(0, SNIPPET_CHARS);
    // Keep only the head of very long turns (pasted logs/prompts dilute the topic).
    messages.push(text.slice(0, MSG_CHAR_CAP));
  }

  const doc = messages.join(" • ").slice(0, DOC_CHAR_CAP);
  return { doc, snippet };
}

/** Heuristic: does this turn look like injected agent/system boilerplate? */
function isBoilerplate(text: string): boolean {
  const head = text.slice(0, 60).toLowerCase();
  return BOILERPLATE_MARKERS.some((m) => head.includes(m));
}

function claudeText(content: unknown): string {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    const parts: string[] = [];
    for (const b of content) {
      if (b && typeof b === "object" && (b as any).type === "text" && typeof (b as any).text === "string") {
        parts.push((b as any).text);
      }
    }
    return parts.join(" ");
  }
  return "";
}

function codexText(content: unknown): string {
  if (Array.isArray(content)) {
    const parts: string[] = [];
    for (const b of content) {
      if (b && typeof b === "object") {
        const bb = b as any;
        if ((bb.type === "input_text" || bb.type === "text") && typeof bb.text === "string") {
          parts.push(bb.text);
        }
      }
    }
    return parts.join(" ");
  }
  return "";
}

// ---------------------------------------------------------------------------
// Incremental build
// ---------------------------------------------------------------------------

export interface BuildStats {
  total: number;
  embedded: number; // newly (re)embedded this run
  reused: number; // carried over unchanged
  removed: number; // dropped (file gone)
  ms: number;
}

/**
 * Rebuild the index incrementally. Only sessions whose mtime changed (or are
 * new) are re-embedded; unchanged sessions carry their vector over; records for
 * vanished files are dropped. Full embed happens only when the index is empty.
 */
export async function buildIndex(
  config: CorpusConfig = loadConfig(),
  opts: { onProgress?: (done: number, total: number) => void } = {},
): Promise<BuildStats> {
  const t0 = Date.now();
  const prior = loadIndex(config);
  const embedder = new Embedder(config.cacheDir);
  const discovered = discoverSessions(config);
  const next = new Map<string, IndexRecord>();

  let embedded = 0;
  let reused = 0;
  let done = 0;

  for (const d of discovered) {
    const old = prior.get(d.path);
    if (old && old.mtimeMs === d.mtimeMs) {
      next.set(d.path, old);
      reused++;
    } else {
      const { doc, snippet } = buildDoc(d.path, d.origin);
      if (doc) {
        const vec = await embedder.embedDoc(doc);
        next.set(d.path, {
          sessionId: basename(d.path, ".jsonl"),
          path: d.path,
          origin: d.origin,
          mtimeMs: d.mtimeMs,
          snippet,
          vec,
        });
        embedded++;
      }
    }
    done++;
    opts.onProgress?.(done, discovered.length);
  }

  const removed = prior.size - reused;
  const total = writeIndex(config, next.values());
  return { total, embedded, reused, removed: Math.max(0, removed), ms: Date.now() - t0 };
}

// ---------------------------------------------------------------------------
// Standalone entrypoint: `bun s2-indexer.ts [--reindex]`
// ---------------------------------------------------------------------------

if (import.meta.main) {
  // Be a good citizen: drop priority so a background index never starves herdr.
  try {
    const os = await import("node:os");
    (os as any).setPriority?.(19);
  } catch {
    /* setPriority may be unavailable; ignore */
  }
  const config = loadConfig();
  if (process.argv.includes("--reindex")) {
    // Force a full rebuild by clearing the prior index first.
    try {
      rmSync(indexDir(config), { recursive: true, force: true });
    } catch {
      /* ignore */
    }
  }
  let last = 0;
  const stats = await buildIndex(config, {
    onProgress: (done, total) => {
      if (done - last >= 500 || done === total) {
        last = done;
        process.stderr.write(`\rindexing ${done}/${total}`);
      }
    },
  });
  process.stderr.write("\n");
  console.log(
    `index: ${stats.total} sessions | embedded ${stats.embedded} | reused ${stats.reused} | removed ${stats.removed} | ${stats.ms}ms`,
  );
}
