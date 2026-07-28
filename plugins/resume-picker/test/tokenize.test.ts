import { test, expect } from "bun:test";
import { tokenizeQuery, isExcludedTranscript } from "../src/types.ts";
import { sampleDoc } from "../src/s2-indexer.ts";

test("tokenizeQuery drops stopwords and keeps significant terms", () => {
  const t = tokenizeQuery("chat where i was upgrading hr and fixing the plugins");
  expect(t).toEqual(["upgrading", "hr", "fixing", "plugins"]);
});

test("tokenizeQuery keeps short-but-meaningful terms like 'hr'", () => {
  expect(tokenizeQuery("upgrading hr")).toContain("hr");
});

test("tokenizeQuery dedupes and caps", () => {
  expect(tokenizeQuery("wasm wasm wasm bridge")).toEqual(["wasm", "bridge"]);
  expect(tokenizeQuery("aa bb cc dd ee ff gg hh ii jj", 3)).toHaveLength(3);
});

test("tokenizeQuery returns [] for an all-stopword query", () => {
  expect(tokenizeQuery("where is the chat i was in")).toEqual([]);
});

test("isExcludedTranscript flags subagent + agent- transcripts, keeps real sessions", () => {
  expect(isExcludedTranscript("/x/proj/subagents/agent-abc.jsonl")).toBe(true);
  expect(isExcludedTranscript("/x/proj/agent-abc.jsonl")).toBe(true);
  expect(isExcludedTranscript("/x/proj/0ccc55a3-1234.jsonl")).toBe(false);
});

test("sampleDoc keeps everything for a short session", () => {
  const msgs = ["first thing", "second thing", "third thing"];
  const doc = sampleDoc(msgs);
  expect(doc).toContain("first thing");
  expect(doc).toContain("third thing");
});

test("sampleDoc represents BOTH the opening and the recent tail of a long session", () => {
  // A long session whose OPENING is about one topic and whose RECENT tail is
  // about another. The bug being guarded: the tail (recent work) must survive
  // into the embedding doc, not be truncated away by the front-biased char cap.
  const msgs: string[] = [];
  msgs.push("OPENING_TOPIC_ZEBRA " + "x".repeat(480));
  for (let i = 0; i < 400; i++) msgs.push("filler message about middle stuff number " + i + " " + "y".repeat(200));
  msgs.push("RECENT_TOPIC_QUOKKA " + "z".repeat(480));
  const doc = sampleDoc(msgs);
  expect(doc).toContain("OPENING_TOPIC_ZEBRA");
  expect(doc).toContain("RECENT_TOPIC_QUOKKA");
});
