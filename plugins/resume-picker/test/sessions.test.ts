import { test, expect } from "bun:test";
import { ago, formatAgo, loadConfig, MetaCache, metaToResultLine } from "../src/sessions.ts";
import { isSentinel, sentinelLine } from "../src/types.ts";
import { makeSandbox, withEnv, writeDaily, writeCodex } from "./helpers.ts";
import { utimesSync } from "node:fs";

test("formatAgo is the canonical name and ago is its alias", () => {
  expect(formatAgo).toBe(ago);
  const now = Date.parse("2026-07-23T00:00:00.000Z");
  expect(formatAgo(now - 2 * 3600_000, now)).toBe("2h");
});

test("sentinel line is built and detected, and is never a real session", () => {
  const s = sentinelLine("gpt: press Tab to search deeper");
  expect(isSentinel(s)).toBe(true);
  expect(s.snippet).toBe("gpt: press Tab to search deeper");
  expect(s.shortId).toBe("");
  const real = metaToResultLine(
    { sessionId: "abcd1234", jsonlPath: "/x", origin: "daily", cwd: "/w", mtimeMs: 0, createdMs: 0, lastUserMs: 0, lastAgentMs: 0, summary: "" },
    "hi",
  );
  expect(isSentinel(real)).toBe(false);
});

test("ago formats compact durations", () => {
  const now = Date.parse("2026-07-23T00:00:00.000Z");
  expect(ago(0, now)).toBe("");
  expect(ago(now - 30_000, now)).toBe("30s");
  expect(ago(now - 5 * 60_000, now)).toBe("5m");
  expect(ago(now - 3 * 3600_000, now)).toBe("3h");
  expect(ago(now - 3 * 86400_000, now)).toBe("3d");
  expect(ago(now - 21 * 86400_000, now)).toBe("3w");
});

test("loadConfig discovers daily, cx and codex roots from env HOME", () => {
  const sb = makeSandbox();
  const restore = withEnv(sb);
  try {
    const cfg = loadConfig();
    const origins = cfg.roots.map((r) => r.origin).sort();
    expect(origins).toEqual(["codex", "cx", "daily"]);
    expect(cfg.cacheDir).toBe(sb.cacheDir);
  } finally {
    restore();
  }
});

test("MetaCache parses a real-format daily transcript", () => {
  const sb = makeSandbox();
  const restore = withEnv(sb);
  try {
    const cwd = "/Users/tester/proj";
    const p = writeDaily(sb, cwd, "aaaabbbb-1111-2222-3333-444455556666", ["find the codeword ZEBRAFISH here"], ["done"]);
    const cache = new MetaCache(loadConfig());
    const meta = cache.getMeta(p);
    expect(meta).toBeDefined();
    expect(meta!.origin).toBe("daily");
    expect(meta!.cwd).toBe(cwd);
    expect(meta!.summary).toContain("ZEBRAFISH");
    expect(meta!.lastUserMs).toBeGreaterThan(0);
    expect(meta!.lastAgentMs).toBeGreaterThan(0);
    expect(meta!.createdMs).toBeGreaterThan(0);
  } finally {
    restore();
  }
});

test("MetaCache parses a real-format codex transcript with payload.id", () => {
  const sb = makeSandbox();
  const restore = withEnv(sb);
  try {
    const p = writeCodex(sb, "/tmp/x", "codexses-9999-8888-7777-666655554444", "codex codeword MANATEE");
    const cache = new MetaCache(loadConfig());
    const meta = cache.getMeta(p);
    expect(meta).toBeDefined();
    expect(meta!.origin).toBe("codex");
    expect(meta!.sessionId).toBe("codexses-9999-8888-7777-666655554444");
    expect(meta!.summary).toContain("MANATEE");
  } finally {
    restore();
  }
});

test("MetaCache re-uses cache when mtime unchanged, re-parses on change", () => {
  const sb = makeSandbox();
  const restore = withEnv(sb);
  try {
    const p = writeDaily(sb, "/w", "cccccccc-0000-0000-0000-000000000000", ["one"], ["a"]);
    const cache = new MetaCache(loadConfig());
    const m1 = cache.getMeta(p)!;
    const m2 = cache.getMeta(p)!;
    expect(m2).toBe(m1); // same object => cache hit
    // Bump mtime into the future and confirm a fresh parse (different object).
    const future = new Date(Date.now() + 10_000);
    utimesSync(p, future, future);
    const m3 = cache.getMeta(p)!;
    expect(m3).not.toBe(m1);
  } finally {
    restore();
  }
});

test("metaToResultLine builds the one-line shape", () => {
  const now = Date.parse("2026-07-23T00:00:00.000Z");
  const line = metaToResultLine(
    {
      sessionId: "abcd1234-ffff-0000-1111-222233334444",
      jsonlPath: "/x.jsonl",
      origin: "daily",
      cwd: "/w",
      mtimeMs: 0,
      createdMs: now - 3 * 86400_000,
      lastUserMs: now - 3600_000,
      lastAgentMs: now - 1800_000,
      summary: "s",
    },
    "  matched   text ",
    now,
  );
  expect(line.shortId).toBe("abcd1234");
  expect(line.snippet).toBe("matched text");
  expect(line.lastUserAgo).toBe("1h");
  expect(line.createdAgo).toBe("3d");
  expect(line.resumeRef.origin).toBe("daily");
});
