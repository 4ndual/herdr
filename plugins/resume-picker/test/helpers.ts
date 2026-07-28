// Shared test helpers: build a sandboxed HOME with REAL-format transcript jsonl.
// No mocks — these are the actual line shapes the parsers see in production.

import { mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

export interface Sandbox {
  home: string;
  ccrDir: string;
  cacheDir: string;
  encode: (cwd: string) => string;
}

export function encode(cwd: string): string {
  return cwd.replace(/[^A-Za-z0-9]/g, "-");
}

/** Create an isolated HOME with claude/cx/codex project trees. */
export function makeSandbox(): Sandbox {
  const home = mkdtempSync(join(tmpdir(), "resume-picker-test-"));
  const ccrDir = join(home, ".claude-code-router", "profiles", "default-claude-code", "claude");
  const cacheDir = join(home, ".cache", "resume-picker");
  mkdirSync(join(home, ".claude", "projects"), { recursive: true });
  mkdirSync(join(ccrDir, "projects"), { recursive: true });
  mkdirSync(join(home, ".codex", "sessions"), { recursive: true });
  return { home, ccrDir, cacheDir, encode };
}

/** Apply a sandbox to process.env for a test body. Returns a restore fn. */
export function withEnv(sb: Sandbox): () => void {
  const saved = {
    HOME: process.env.HOME,
    CCR_DIR: process.env.CCR_DIR,
    RESUME_PICKER_CACHE_DIR: process.env.RESUME_PICKER_CACHE_DIR,
    XDG_CACHE_HOME: process.env.XDG_CACHE_HOME,
  };
  process.env.HOME = sb.home;
  process.env.CCR_DIR = sb.ccrDir;
  process.env.RESUME_PICKER_CACHE_DIR = sb.cacheDir;
  delete process.env.XDG_CACHE_HOME;
  return () => {
    for (const [k, v] of Object.entries(saved)) {
      if (v === undefined) delete (process.env as any)[k];
      else (process.env as any)[k] = v;
    }
  };
}

/** Write a real-format daily claude transcript. Returns its jsonl path. */
export function writeDaily(sb: Sandbox, cwd: string, id: string, userMsgs: string[], agentMsgs: string[]): string {
  const dir = join(sb.home, ".claude", "projects", encode(cwd));
  mkdirSync(dir, { recursive: true });
  const path = join(dir, `${id}.jsonl`);
  const lines: string[] = [];
  let t = Date.parse("2026-07-20T10:00:00.000Z");
  const stamp = () => new Date((t += 60000)).toISOString();
  for (let i = 0; i < Math.max(userMsgs.length, agentMsgs.length); i++) {
    if (userMsgs[i] !== undefined) {
      lines.push(
        JSON.stringify({
          type: "user",
          sessionId: id,
          cwd,
          timestamp: stamp(),
          message: { role: "user", content: userMsgs[i] },
        }),
      );
    }
    if (agentMsgs[i] !== undefined) {
      lines.push(
        JSON.stringify({
          type: "assistant",
          sessionId: id,
          cwd,
          timestamp: stamp(),
          message: { role: "assistant", content: [{ type: "text", text: agentMsgs[i] }] },
        }),
      );
    }
  }
  writeFileSync(path, lines.join("\n") + "\n");
  return path;
}

/** Write a real-format codex session transcript. Returns its jsonl path. */
export function writeCodex(sb: Sandbox, cwd: string, id: string, userMsg: string): string {
  const dir = join(sb.home, ".codex", "sessions", "2026", "07", "20");
  mkdirSync(dir, { recursive: true });
  const path = join(dir, `rollout-2026-07-20T10-00-00-${id}.jsonl`);
  const lines = [
    JSON.stringify({ timestamp: "2026-07-20T10:00:00.000Z", type: "session_meta", payload: { id, cwd } }),
    JSON.stringify({
      timestamp: "2026-07-20T10:01:00.000Z",
      type: "response_item",
      payload: { type: "message", role: "user", content: [{ type: "input_text", text: userMsg }] },
    }),
    JSON.stringify({
      timestamp: "2026-07-20T10:02:00.000Z",
      type: "response_item",
      payload: { type: "message", role: "assistant", content: [{ type: "output_text", text: "ok" }] },
    }),
  ];
  writeFileSync(path, lines.join("\n") + "\n");
  return path;
}
