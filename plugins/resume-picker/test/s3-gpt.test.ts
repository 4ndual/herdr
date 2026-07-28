// Colocated tests for S3 (gpt fallback). Real fixtures in a sandboxed HOME —
// never touches the live corpus or the real vault. The cx call is exercised
// with a REAL cheap invocation (no mock), guarded so the suite still passes if
// the gateway is down.
//
// Run: cd plugins/resume-picker && bun test

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import {
  agoFmt,
  buildPrompt,
  extractSnippet,
  parseHits,
  runCodex,
  searchGpt,
  GPT_TIMEOUT_MS,
} from "../src/s3-gpt.ts";
import type { ResultLine } from "../src/types.ts";

// A unique codeword no real transcript contains, so the gpt pick is unambiguous.
const CODEWORD = "zulqarnain-4471-flamboyant";
const ORIG_HOME = process.env.HOME;
let home: string;

/** Write a real-format claude transcript line by line into a sandbox projects dir. */
function writeClaudeSession(id: string, cwd: string, turns: Array<{ role: "user" | "assistant"; text: string; ts: string }>) {
  const encoded = cwd.replace(/[^a-zA-Z0-9]/g, "-");
  const dir = join(home, ".claude", "projects", encoded);
  mkdirSync(dir, { recursive: true });
  const lines = turns.map((t) => {
    if (t.role === "user") {
      return JSON.stringify({ type: "user", message: { role: "user", content: t.text }, sessionId: id, cwd, timestamp: t.ts, uuid: crypto.randomUUID() });
    }
    return JSON.stringify({ type: "assistant", message: { role: "assistant", content: [{ type: "text", text: t.text }] }, sessionId: id, cwd, timestamp: t.ts, uuid: crypto.randomUUID() });
  });
  writeFileSync(join(dir, `${id}.jsonl`), lines.join("\n") + "\n");
}

beforeAll(() => {
  home = mkdtempSync(join(tmpdir(), "resume-picker-s3-"));
  // Corpus reads $HOME; cx must keep the real home to find its node/gateway.
  process.env.CX_HOME = ORIG_HOME;
  process.env.HOME = home;
  process.env.RESUME_PICKER_CACHE_DIR = join(home, "cache");
  // Isolate from any real CCR profiles / codex sessions.
  process.env.CCR_DIR = join(home, "no-such-ccr");

  const cwd = join(home, "work", "herdr");
  writeClaudeSession(
    "aaaaaaaa-1111-2222-3333-444444444444",
    cwd,
    [
      { role: "user", text: `Let's discuss the ${CODEWORD} rollout plan for the wasm bridge`, ts: "2026-07-20T10:00:00.000Z" },
      { role: "assistant", text: "Sure, here is the plan.", ts: "2026-07-20T10:01:00.000Z" },
    ],
  );
  writeClaudeSession(
    "bbbbbbbb-5555-6666-7777-888888888888",
    cwd,
    [
      { role: "user", text: "Totally unrelated: how do I bake sourdough bread?", ts: "2026-07-19T09:00:00.000Z" },
      { role: "assistant", text: "Mix flour and water.", ts: "2026-07-19T09:01:00.000Z" },
    ],
  );
});

afterAll(() => {
  process.env.HOME = ORIG_HOME;
  try {
    rmSync(home, { recursive: true, force: true });
  } catch {}
});

describe("agoFmt", () => {
  const now = Date.parse("2026-07-23T00:00:00.000Z");
  test("returns empty for unknown", () => expect(agoFmt(0, now)).toBe(""));
  test("seconds/minutes/hours/days/weeks", () => {
    expect(agoFmt(now - 30_000, now)).toBe("30s");
    expect(agoFmt(now - 5 * 60_000, now)).toBe("5m");
    expect(agoFmt(now - 3 * 3_600_000, now)).toBe("3h");
    expect(agoFmt(now - 4 * 86_400_000, now)).toBe("4d");
    expect(agoFmt(now - 3 * 7 * 86_400_000, now)).toBe("3w");
  });
});

describe("parseHits", () => {
  test("parses HIT lines and ignores noise", () => {
    const out = [
      "Here are the results:",
      "HIT|aaaa-1111|matched the wasm bridge plan",
      "some junk",
      "HIT|bbbb-2222|another quote | with a pipe",
      "NONE",
    ].join("\n");
    const hits = parseHits(out);
    expect(hits.length).toBe(2);
    expect(hits[0]).toEqual({ sessionId: "aaaa-1111", snippet: "matched the wasm bridge plan" });
    expect(hits[1].sessionId).toBe("bbbb-2222");
    expect(hits[1].snippet).toBe("another quote | with a pipe");
  });
  test("NONE-only yields no hits", () => expect(parseHits("NONE").length).toBe(0));
});

describe("extractSnippet", () => {
  test("pulls a clean windowed snippet around the match", () => {
    const encoded = join(home, "work", "herdr").replace(/[^a-zA-Z0-9]/g, "-");
    const path = join(home, ".claude", "projects", encoded, "aaaaaaaa-1111-2222-3333-444444444444.jsonl");
    const snip = extractSnippet(path, CODEWORD);
    expect(snip.toLowerCase()).toContain(CODEWORD);
    expect(snip).not.toContain('"type":"user"'); // JSON stripped, real text returned
  });
});

describe("buildPrompt", () => {
  test("includes query, ids, and strict format instructions", () => {
    const prompt = buildPrompt("wasm bridge", [
      { path: "/x.jsonl", meta: { sessionId: "id-1", jsonlPath: "/x.jsonl", origin: "daily", cwd: "/w", mtimeMs: 1, createdMs: 1, lastUserMs: 1, lastAgentMs: 1, summary: "the wasm bridge chat" }, snippet: "matched bridge" },
    ]);
    expect(prompt).toContain("wasm bridge");
    expect(prompt).toContain("id=id-1");
    expect(prompt).toContain("HIT|");
    expect(prompt).toContain("NONE");
  });
});

describe("searchGpt (degradation paths)", () => {
  test("no candidates -> single degradation line, non-selectable", async () => {
    const lines = await collect(searchGpt("this-string-matches-nothing-anywhere-xyz", new AbortController().signal));
    expect(lines.length).toBe(1);
    expect(lines[0].sessionId).toBe("");
    expect(lines[0].snippet).toContain("gpt:");
  });

  test("aborted signal yields nothing", async () => {
    const ac = new AbortController();
    ac.abort();
    const lines = await collect(searchGpt(CODEWORD, ac.signal));
    expect(lines.length).toBe(0);
  });
});

describe("runCodex (real cheap invocation via the luna/Codex route)", () => {
  test("returns model text for a trivial prompt", async () => {
    const out = await runCodex("Reply with exactly one word: PONG", new AbortController().signal, GPT_TIMEOUT_MS);
    if (out === null) {
      console.warn("runCodex returned null (codex CLI unavailable?) — skipping content assertion");
      return;
    }
    expect(out.toUpperCase()).toContain("PONG");
  }, 45_000);
});

describe("searchGpt (real end-to-end pick)", () => {
  test("finds the codeword session and maps it to a real ResultLine", async () => {
    const lines = await collect(searchGpt(CODEWORD, new AbortController().signal));
    expect(lines.length).toBeGreaterThanOrEqual(1);
    const real = lines.find((l) => l.sessionId !== "");
    if (!real) {
      // Gateway down or model declined — degradation is acceptable, never a fake hit.
      expect(lines[0].sessionId).toBe("");
      console.warn("searchGpt produced only a degradation line (cx gateway down?)");
      return;
    }
    expect(real.sessionId).toBe("aaaaaaaa-1111-2222-3333-444444444444");
    expect(real.shortId).toBe("aaaaaaaa");
    expect(real.resumeRef.jsonlPath).toContain(".jsonl");
    expect(real.resumeRef.origin).toBe("daily");
  }, 60_000);
});

async function collect(it: AsyncIterable<ResultLine>): Promise<ResultLine[]> {
  const out: ResultLine[] = [];
  for await (const l of it) out.push(l);
  return out;
}
