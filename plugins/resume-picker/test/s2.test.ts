// Colocated tests for the S2 similarity index.
//
// Real end-to-end: a sandbox HOME with real-format transcript fixtures, the real
// local MiniLM model (no mocks, no fake embeddings). If the local model is not
// present on this machine, the model-dependent tests are skipped with a notice
// rather than faked.

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync, existsSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

import { loadConfig } from "../src/sessions.ts";
import {
  buildDoc,
  buildIndex,
  loadIndex,
  resolveLocalModel,
  discoverSessions,
  EMBED_DIM,
} from "../src/s2-indexer.ts";
import { makeSimilarSearcher } from "../src/s2-similar.ts";

// --- sandbox HOME with real-format fixtures ------------------------------------

let sandbox: string;
let prevHome: string | undefined;
let prevCache: string | undefined;
let prevModels: string | undefined;
/** The real HOME (captured before we sandbox it) so we can point at the real model store. */
const realHome = process.env.HOME ?? "";

/** Write a claude-format transcript: user turns + optional summary + assistant. */
function writeClaudeSession(home: string, encCwd: string, id: string, userMsgs: string[], summary?: string) {
  const dir = join(home, ".claude", "projects", encCwd);
  mkdirSync(dir, { recursive: true });
  const now = Date.now();
  const lines: string[] = [];
  if (summary) lines.push(JSON.stringify({ type: "summary", summary }));
  userMsgs.forEach((text, i) => {
    lines.push(
      JSON.stringify({
        type: "user",
        message: { role: "user", content: text },
        uuid: `${id}-u${i}`,
        timestamp: new Date(now - (userMsgs.length - i) * 60000).toISOString(),
        cwd: "/Users/tester/work",
      }),
    );
    lines.push(
      JSON.stringify({
        type: "assistant",
        message: { role: "assistant", content: [{ type: "text", text: "ok" }] },
        timestamp: new Date(now - (userMsgs.length - i) * 60000 + 1000).toISOString(),
      }),
    );
  });
  writeFileSync(join(dir, `${id}.jsonl`), lines.join("\n"));
  return join(dir, `${id}.jsonl`);
}

/** Is the real local model available on this machine? */
function modelAvailable(cacheDir: string): boolean {
  try {
    resolveLocalModel(cacheDir);
    return true;
  } catch {
    return false;
  }
}

beforeAll(() => {
  sandbox = mkdtempSync(join(tmpdir(), "resume-picker-s2-"));
  prevHome = process.env.HOME;
  prevCache = process.env.RESUME_PICKER_CACHE_DIR;
  prevModels = process.env.MODELS_DIR;
  process.env.HOME = sandbox;
  process.env.RESUME_PICKER_CACHE_DIR = join(sandbox, "cache");
  // Sandbox the corpus HOME but keep the real model store — the model is a real
  // local asset, not a fixture. MODELS_DIR overrides the HOME-relative default.
  if (realHome) process.env.MODELS_DIR = join(realHome, "Models");

  const enc = "-Users-tester-work";
  writeClaudeSession(
    sandbox,
    enc,
    "aaaaaaaa-1111-2222-3333-444444444444",
    [
      "we decided KEEP-FULL-BRIDGE for the wave-1 wasm plugin integration",
      "the full ghostty bridge stays, we don't trim the surface capabilities",
    ],
    "wasm bridge keep-full decision",
  );
  writeClaudeSession(
    sandbox,
    enc,
    "bbbbbbbb-1111-2222-3333-444444444444",
    ["how do I roast coffee beans at home and grind them for espresso"],
    "coffee roasting notes",
  );
  writeClaudeSession(
    sandbox,
    enc,
    "cccccccc-1111-2222-3333-444444444444",
    ["debugging a rust panic in the pane runtime terminal parser and viewport"],
    "rust panic pane runtime",
  );
});

afterAll(() => {
  if (prevHome === undefined) delete process.env.HOME;
  else process.env.HOME = prevHome;
  if (prevCache === undefined) delete process.env.RESUME_PICKER_CACHE_DIR;
  else process.env.RESUME_PICKER_CACHE_DIR = prevCache;
  if (prevModels === undefined) delete process.env.MODELS_DIR;
  else process.env.MODELS_DIR = prevModels;
  try {
    rmSync(sandbox, { recursive: true, force: true });
  } catch {
    /* ignore */
  }
});

// --- pure, model-free tests ----------------------------------------------------

describe("discovery + doc building (no model needed)", () => {
  test("discovers all fixtures deduped by realpath", () => {
    const config = loadConfig();
    const found = discoverSessions(config);
    expect(found.length).toBe(3);
    // all under the daily root
    expect(found.every((f) => f.origin === "daily")).toBe(true);
  });

  test("buildDoc concatenates user messages + summary, drops nothing meaningful", () => {
    const config = loadConfig();
    const found = discoverSessions(config).sort((a, b) => a.path.localeCompare(b.path));
    const { doc, snippet } = buildDoc(found[0].path, "daily");
    expect(doc.toLowerCase()).toContain("keep-full-bridge");
    expect(doc.toLowerCase()).toContain("surface capabilities");
    // summary line is included too
    expect(doc.toLowerCase()).toContain("keep-full decision");
    expect(snippet.length).toBeGreaterThan(0);
  });
});

// --- model-dependent, real-embedding tests -------------------------------------

describe("real embedding index", () => {
  const cacheDir = join(process.env.HOME ?? "", "cache");
  const have = () => modelAvailable(join(sandbox, "cache"));

  test("full build embeds every session with EMBED_DIM vectors", async () => {
    if (!have()) {
      console.warn("SKIP: local MiniLM model not present — real-embedding test skipped");
      return;
    }
    const config = loadConfig();
    const stats = await buildIndex(config);
    expect(stats.total).toBe(3);
    expect(stats.embedded).toBe(3);
    const index = loadIndex(config);
    expect(index.size).toBe(3);
    for (const rec of index.values()) {
      expect(rec.vec.length).toBe(EMBED_DIM);
    }
  });

  test("incremental rebuild reuses unchanged sessions (0 re-embeds)", async () => {
    if (!have()) return;
    const config = loadConfig();
    await buildIndex(config); // ensure built
    const stats = await buildIndex(config); // nothing changed
    expect(stats.reused).toBe(3);
    expect(stats.embedded).toBe(0);
  });

  test("similar search ranks the semantic paraphrase first, drops junk", async () => {
    if (!have()) return;
    const config = loadConfig();
    await buildIndex(config);
    const search = makeSimilarSearcher(config);
    const ctrl = new AbortController();
    const results = [];
    // paraphrase of the bridge session that shares NO distinctive substring
    for await (const line of search("retain the entire integration layer for phase one", ctrl.signal)) {
      results.push(line);
    }
    expect(results.length).toBeGreaterThan(0);
    // top hit is the bridge session, not coffee/rust
    expect(results[0].sessionId.startsWith("aaaaaaaa")).toBe(true);
    // coffee session must not appear (below threshold / not top)
    expect(results.some((r) => r.sessionId.startsWith("bbbbbbbb"))).toBe(false);
  });

  test("abort before scan yields nothing", async () => {
    if (!have()) return;
    const config = loadConfig();
    await buildIndex(config);
    const search = makeSimilarSearcher(config);
    const ctrl = new AbortController();
    ctrl.abort();
    const results = [];
    for await (const line of search("anything", ctrl.signal)) results.push(line);
    expect(results.length).toBe(0);
  });
});

// --- degradation path (no index) -----------------------------------------------

describe("degradation", () => {
  test("empty index yields a visible not-built line, never fake results", async () => {
    // Point at an empty cache dir so loadIndex returns nothing.
    const emptyCache = mkdtempSync(join(tmpdir(), "resume-picker-empty-"));
    const config = { ...loadConfig(), cacheDir: emptyCache };
    const search = makeSimilarSearcher(config);
    const ctrl = new AbortController();
    const results = [];
    for await (const line of search("query", ctrl.signal)) results.push(line);
    expect(results.length).toBe(1);
    expect(results[0].snippet.toLowerCase()).toContain("similar:");
    expect(results[0].resumeRef.sessionId).toBe("");
    rmSync(emptyCache, { recursive: true, force: true });
  });
});
