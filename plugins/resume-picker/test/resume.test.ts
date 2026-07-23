import { test, expect } from "bun:test";
import { resolveResume, encodeCwd } from "../src/resume.ts";
import { makeSandbox, withEnv, writeDaily } from "./helpers.ts";
import { existsSync, realpathSync } from "node:fs";
import { join } from "node:path";
import type { ResumeRef } from "../src/types.ts";

test("encodeCwd matches the client project-dir encoding", () => {
  expect(encodeCwd("/Users/andual/.claude")).toBe("-Users-andual--claude");
});

test("cx-origin resolves to plain cx-resume with skip-permissions (no fork)", () => {
  const ref: ResumeRef = { sessionId: "id-123", jsonlPath: "/x.jsonl", origin: "cx", cwd: "/w" };
  const r = resolveResume(ref);
  expect(r.command).toBe("exec cx-resume 'id-123' --dangerously-skip-permissions");
  expect(r.command).not.toContain("--fork-session");
});

test("daily resolves to cx --resume and bridges the transcript into the CCR dir", () => {
  const sb = makeSandbox();
  const restore = withEnv(sb);
  try {
    const cwd = "/Users/tester/proj";
    const id = "daily12-3456-7890-abcd-ef0123456789";
    const p = writeDaily(sb, cwd, id, ["hi"], ["ok"]);
    const ref: ResumeRef = { sessionId: id, jsonlPath: p, origin: "daily", cwd };
    const r = resolveResume(ref);
    expect(r.command).toBe(`exec cx --resume '${id}' --dangerously-skip-permissions`);
    // The bridge symlink now exists under the CCR projects dir and points at the daily jsonl.
    const bridged = join(sb.ccrDir, "projects", encodeCwd(cwd), `${id}.jsonl`);
    expect(existsSync(bridged)).toBe(true);
    expect(realpathSync(bridged)).toBe(realpathSync(p));
  } finally {
    restore();
  }
});

test("codex is not resumable and returns a reason", () => {
  const ref: ResumeRef = { sessionId: "cx-id", jsonlPath: "/x.jsonl", origin: "codex", cwd: "/w" };
  const r = resolveResume(ref);
  expect(r.command).toBeNull();
  expect(r.reason).toBeTruthy();
});
