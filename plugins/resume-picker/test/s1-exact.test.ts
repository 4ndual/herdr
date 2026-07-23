import { test, expect } from "bun:test";
import { makeExactSearcher, extractSnippet } from "../src/s1-exact.ts";
import { loadConfig, MetaCache } from "../src/sessions.ts";
import { isSentinel } from "../src/types.ts";
import { makeSandbox, withEnv, writeDaily } from "./helpers.ts";

test("extractSnippet pulls human text centered on the query", () => {
  const rec = JSON.stringify({ type: "user", message: { role: "user", content: "please find the ZEBRAFISH codeword now" } });
  const s = extractSnippet(rec, "ZEBRAFISH");
  expect(s).toContain("ZEBRAFISH");
  expect(s).not.toContain("{"); // no raw json
});

test("F1: a match with no extractable human text never returns raw JSON", () => {
  // A record whose match is inside structured metadata (no message.content text)
  // must NOT leak the raw JSON line; extractSnippet signals "no snippet" ("").
  const rec = JSON.stringify({ type: "summary", leafUuid: "abc", meta: { KEYWORD42: true } });
  const s = extractSnippet(rec, "KEYWORD42");
  expect(s).toBe("");
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
