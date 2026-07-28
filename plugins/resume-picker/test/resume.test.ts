import { test, expect } from "bun:test";
import { resolveResume, encodeCwd, sweepStaleBridges } from "../src/resume.ts";
import { makeSandbox, withEnv, writeDaily } from "./helpers.ts";
import { existsSync, realpathSync, mkdirSync, symlinkSync, writeFileSync, rmSync, lstatSync } from "node:fs";
import { join } from "node:path";
import type { ResumeRef } from "../src/types.ts";

/** Write a real-format cx-origin transcript under the CCR projects dir. */
function writeCx(sb: ReturnType<typeof makeSandbox>, cwd: string, id: string): string {
  const dir = join(sb.ccrDir, "projects", encodeCwd(cwd));
  mkdirSync(dir, { recursive: true });
  const path = join(dir, `${id}.jsonl`);
  writeFileSync(path, JSON.stringify({ type: "user", sessionId: id, cwd, message: { role: "user", content: "hi" } }) + "\n");
  return path;
}

test("encodeCwd matches the client project-dir encoding", () => {
  expect(encodeCwd("/Users/andual/.claude")).toBe("-Users-andual--claude");
});

test("cx-origin resumed from its own surviving cwd: cd + cx-resume, NO bridge, NO reaper", () => {
  const sb = makeSandbox();
  const restore = withEnv(sb);
  try {
    const cwd = join(sb.home, "proj");
    mkdirSync(cwd, { recursive: true }); // cwd exists
    const id = "cx000001-0000-0000-0000-000000000000";
    writeCx(sb, cwd, id);
    const ref: ResumeRef = { sessionId: id, jsonlPath: join(sb.ccrDir, "projects", encodeCwd(cwd), `${id}.jsonl`), origin: "cx", cwd };
    const r = resolveResume(ref);
    expect(r.command).toBe(`cd '${cwd}' && exec cx-resume '${id}' --dangerously-skip-permissions`);
    expect(r.command).not.toContain("--fork-session");
    expect(r.command).not.toContain("rm -f"); // real file, no reaper/trap that could delete it
  } finally {
    restore();
  }
});

test("P0: cx-origin whose cwd no longer exists — bridges into a known-good dir, never fails", () => {
  const sb = makeSandbox();
  const restore = withEnv(sb);
  try {
    // Original cwd lives under the sandbox but is DELETED after the transcript is written.
    const goneCwd = join(sb.home, "gone-tmp");
    const id = "cx000002-0000-0000-0000-000000000000";
    const realPath = writeCx(sb, goneCwd, id); // transcript at CCR/projects/encode(goneCwd)/
    const ref: ResumeRef = { sessionId: id, jsonlPath: realPath, origin: "cx", cwd: goneCwd };
    // goneCwd does not exist as a directory (only the CCR project dir encoding does).
    const r = resolveResume(ref);
    expect(r.command).toBeTruthy();
    expect(r.command).toContain("exec cx-resume");
    // Launches from a dir that exists (recreated goneCwd, or HOME) and the
    // transcript is findable there.
    const m = r.command!.match(/cd '([^']+)' &&/);
    expect(m).toBeTruthy();
    const wd = m![1];
    expect(existsSync(wd)).toBe(true);
    const bridged = join(sb.ccrDir, "projects", encodeCwd(wd), `${id}.jsonl`);
    expect(existsSync(bridged)).toBe(true);
    expect(realpathSync(bridged)).toBe(realpathSync(realPath));
  } finally {
    restore();
  }
});

test("daily from a different cwd: bridges + cd + cx --resume, with reaper", () => {
  const sb = makeSandbox();
  const restore = withEnv(sb);
  try {
    const cwd = join(sb.home, "proj");
    mkdirSync(cwd, { recursive: true });
    const id = "daily12-3456-7890-abcd-ef0123456789";
    const p = writeDaily(sb, cwd, id, ["hi"], ["ok"]);
    const ref: ResumeRef = { sessionId: id, jsonlPath: p, origin: "daily", cwd };
    const r = resolveResume(ref);
    expect(r.command).toContain(`cd '${cwd}' && exec cx --resume '${id}' --dangerously-skip-permissions`);
    expect(r.command).not.toContain("--fork-session");
    expect(r.command).toContain("rm -f"); // reaper + trap remove the bridge
    const bridged = join(sb.ccrDir, "projects", encodeCwd(cwd), `${id}.jsonl`);
    expect(existsSync(bridged)).toBe(true);
    expect(realpathSync(bridged)).toBe(realpathSync(p));
  } finally {
    restore();
  }
});

test("P0: a missing transcript file returns a clear reason, never a doomed exec", () => {
  const ref: ResumeRef = { sessionId: "id-123", jsonlPath: "/nope/does-not-exist.jsonl", origin: "cx", cwd: "/w" };
  const r = resolveResume(ref);
  expect(r.command).toBeNull();
  expect(r.reason).toContain("not found");
});

test("sweepStaleBridges removes CCR symlinks pointing into ~/.claude, keeps real files (F6)", () => {
  const sb = makeSandbox();
  const restore = withEnv(sb);
  try {
    const cwd = "/Users/tester/proj";
    const enc = encodeCwd(cwd);
    const dailyPath = writeDaily(sb, cwd, "stale111-2222-3333-4444-555555555555", ["hi"], ["ok"]);
    const ccrProj = join(sb.ccrDir, "projects", enc);
    mkdirSync(ccrProj, { recursive: true });
    const staleBridge = join(ccrProj, "stale111-2222-3333-4444-555555555555.jsonl");
    symlinkSync(dailyPath, staleBridge);
    const realCx = join(ccrProj, "cxreal00-0000-0000-0000-000000000000.jsonl");
    writeFileSync(realCx, "{}\n");

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
