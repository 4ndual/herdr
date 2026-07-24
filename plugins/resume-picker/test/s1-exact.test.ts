import { test, expect } from "bun:test";
import { makeExactSearcher, extractSnippet } from "../src/s1-exact.ts";
import { loadConfig, MetaCache } from "../src/sessions.ts";
import { isSentinel } from "../src/types.ts";
import { makeSandbox, withEnv, writeDaily } from "./helpers.ts";
import type { SessionMeta } from "../src/types.ts";

function fakeMeta(summary: string): SessionMeta {
  return {
    sessionId: "id", jsonlPath: "/p/id.jsonl", origin: "daily", cwd: "/p",
    mtimeMs: 0, createdMs: 0, lastUserMs: 0, lastAgentMs: 0,
    summary, lastPrompt: "", checkpoints: [], chainParentId: "",
  };
}

test("extractSnippet centers the session summary on a matched term", () => {
  const meta = fakeMeta("please find the zebrafish codeword now");
  const s = extractSnippet(meta, ["zebrafish"]);
  expect(s).toContain("zebrafish");
  expect(s).not.toContain("{"); // human text only, never raw json
});

test("extractSnippet falls back to the summary head when no term is present", () => {
  const meta = fakeMeta("a summary that does not contain the search token at all");
  const s = extractSnippet(meta, ["missingterm"]);
  expect(s).toContain("a summary");
});

test("F2: S1 caps at 10 rows by recency and appends a non-selectable '… N more' line", async () => {
  const sb = makeSandbox();
  const restore = withEnv(sb);
  try {
    // 14 sessions all matching the query, with increasing recency by index.
    for (let i = 0; i < 14; i++) {
      const id = `${String(i).padStart(8, "0")}-aaaa-bbbb-cccc-dddddddddddd`;
      writeDaily(sb, `/Users/tester/p${i}`, id, [`shared MANYHITS token number ${i}`], ["ok"]);
    }
    const search = makeExactSearcher(new MetaCache(loadConfig()));
    const results = [];
    for await (const r of search("MANYHITS", new AbortController().signal)) results.push(r);

    const selectable = results.filter((r) => !isSentinel(r));
    const sentinels = results.filter((r) => isSentinel(r));
    expect(selectable.length).toBe(10); // capped at TOP_N
    expect(sentinels.length).toBe(1);
    expect(sentinels[0].snippet).toContain("more");
  } finally {
    restore();
  }
});

test("makeExactSearcher finds a codeword via real rg over a sandbox corpus", async () => {
  const sb = makeSandbox();
  const restore = withEnv(sb);
  try {
    writeDaily(sb, "/Users/tester/a", "11111111-aaaa-bbbb-cccc-dddddddddddd", ["contains the codeword QUOKKA88 somewhere"], ["ok"]);
    writeDaily(sb, "/Users/tester/b", "22222222-aaaa-bbbb-cccc-dddddddddddd", ["unrelated content"], ["ok"]);

    const cache = new MetaCache(loadConfig());
    const search = makeExactSearcher(cache);
    const results = [];
    for await (const r of search("QUOKKA88", new AbortController().signal)) results.push(r);

    expect(results.length).toBe(1);
    expect(results[0].sessionId).toBe("11111111-aaaa-bbbb-cccc-dddddddddddd");
    expect(results[0].snippet).toContain("QUOKKA88");
    expect(results[0].resumeRef.origin).toBe("daily");
  } finally {
    restore();
  }
});

test("makeExactSearcher yields nothing for an empty query", async () => {
  const sb = makeSandbox();
  const restore = withEnv(sb);
  try {
    const search = makeExactSearcher(new MetaCache(loadConfig()));
    const results = [];
    for await (const r of search("   ", new AbortController().signal)) results.push(r);
    expect(results.length).toBe(0);
  } finally {
    restore();
  }
});

test("makeExactSearcher stops promptly when aborted before start", async () => {
  const sb = makeSandbox();
  const restore = withEnv(sb);
  try {
    writeDaily(sb, "/w", "33333333-aaaa-bbbb-cccc-dddddddddddd", ["QUOKKA88 here too"], ["ok"]);
    const ctl = new AbortController();
    ctl.abort();
    const search = makeExactSearcher(new MetaCache(loadConfig()));
    const results = [];
    for await (const r of search("QUOKKA88", ctl.signal)) results.push(r);
    expect(results.length).toBe(0);
  } finally {
    restore();
  }
});
