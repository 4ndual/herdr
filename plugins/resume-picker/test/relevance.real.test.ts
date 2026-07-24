// Acceptance + eval-set relevance test against the REAL on-disk corpus
// (read-only — never writes, never resumes). Guarded: if the corpus or the
// specific expected sessions aren't present (another machine, pruned history),
// the case is skipped rather than failing. This encodes Andres's real failure:
// a natural-language query must surface the session it describes.

import { test, expect } from "bun:test";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";
import { makeExactSearcher } from "../src/s1-exact.ts";
import { loadConfig, MetaCache } from "../src/sessions.ts";
import { isSentinel } from "../src/types.ts";

const HOME = process.env.HOME ?? homedir();
const DAILY = join(HOME, ".claude", "projects", "-Users-andual");

/** [query, expected-session-id-prefix, max acceptable rank] */
const ACCEPTANCE: [string, string, number] = [
  "chat where i was upgrading hr and fixing the plugins",
  "0ccc55a3-99bf-401d-b625-32c4b8f3bf05",
  3,
];
const EVAL: [string, string, number][] = [
  ["recopilate info about the slap app yb labs forum", "bba5959c", 10],
  ["sync claude code config skill across devices", "ee8a733d", 10],
  ["discover how to orchestate the claude browser plugin", "37b41140", 10],
  ["linktic payment monthly informe cuenta de cobro", "8838bb65", 10],
  ["kill the hook or cron spamming claude processes", "d5d69f25", 10],
];

async function rankOf(cache: MetaCache, query: string, idPrefix: string): Promise<number> {
  const rows = [];
  for await (const r of makeExactSearcher(cache)(query, new AbortController().signal)) {
    if (!isSentinel(r)) rows.push(r);
  }
  return rows.findIndex((r) => r.sessionId.startsWith(idPrefix));
}

function corpusPresent(idPrefix: string): boolean {
  return existsSync(join(DAILY, `${idPrefix}.jsonl`));
}

test("ACCEPTANCE: natural-language query surfaces the hr-plugin-upgrade session near the top", async () => {
  const [q, id, maxRank] = ACCEPTANCE;
  if (!corpusPresent(id)) return; // skip off-machine
  const cache = new MetaCache(loadConfig());
  const rank = await rankOf(cache, q, id);
  expect(rank).toBeGreaterThanOrEqual(0);
  expect(rank).toBeLessThanOrEqual(maxRank);
}, 30_000);

for (const [q, id, maxRank] of EVAL) {
  test(`EVAL: "${q}" surfaces ${id} within top ${maxRank}`, async () => {
    if (!corpusPresent(id)) return; // skip off-machine
    const cache = new MetaCache(loadConfig());
    const rank = await rankOf(cache, q, id);
    expect(rank).toBeGreaterThanOrEqual(0);
    expect(rank).toBeLessThanOrEqual(maxRank);
  }, 30_000);
}
