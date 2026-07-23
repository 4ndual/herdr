import { test, expect } from "bun:test";
import { resolveResume, encodeCwd, sweepStaleBridges } from "../src/resume.ts";
import { makeSandbox, withEnv, writeDaily } from "./helpers.ts";
import { existsSync, realpathSync, mkdirSync, symlinkSync } from "node:fs";
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
    // Command ends with the plain (non-fork) cx resume, prefixed by the F6
    // detached reaper + EXIT trap that clean up the bridge symlink.
    expect(r.command).toContain(`exec cx --resume '${id}' --dangerously-skip-permissions`);
    expect(r.command).not.toContain("--fork-session");
    expect(r.command).toContain("rm -f"); // reaper + trap remove the bridge
    // The bridge symlink now exists under the CCR projects dir and points at the daily jsonl.
    const bridged = join(sb.ccrDir, "projects", encodeCwd(cwd), `${id}.jsonl`);
    expect(existsSync(bridged)).toBe(true);
    expect(realpathSync(bridged)).toBe(realpathSync(p));
  } finally {
    restore();
  }
});

test("sweepStaleBridges removes CCR symlinks pointing into ~/.claude, keeps real files (F6)", () => {
  const sb = makeSandbox();
  const restore = withEnv(sb);
  try {
    const cwd = "/Users/tester/proj";
    const enc = encodeCwd(cwd);
    // A stale bridge from a PRIOR resume: a symlink in the CCR tree -> a daily jsonl.
    const dailyPath = writeDaily(sb, cwd, "stale111-2222-3333-4444-555555555555", ["hi"], ["ok"]);
    const ccrProj = join(sb.ccrDir, "projects", enc);
    mkdirSync(ccrProj, { recursive: true });
    const staleBridge = join(ccrProj, "stale111-2222-3333-4444-555555555555.jsonl");
    symlinkSync(dailyPath, staleBridge);
    // A genuine cx-origin transcript (real file) must be left untouched.
    const realCx = join(ccrProj, "cxreal00-0000-0000-0000-000000000000.jsonl");
    require("node:fs").writeFileSync(realCx, "{}\n");

    expect(existsSync(staleBridge)).toBe(true);
    const removed = sweepStaleBridges();
    expect(removed).toBeGreaterThanOrEqual(1);
    expect(existsSync(staleBridge)).toBe(false); // stale bridge swept
    expect(existsSync(realCx)).toBe(true); // real cx transcript kept
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
