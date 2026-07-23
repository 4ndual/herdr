// Selection -> resume command resolution.
//
// The picker uses the fzf model: it prints the resolved shell command to stdout
// and exits; a thin wrapper (`exec sh -c "$cmd"`) then replaces the pane's
// process with the resumed session, so the resume runs in the SAME pane.
//
// Resolution mirrors plugins/button-dash-ui/scripts/resume-in-cx.sh but does a
// PLAIN resume (no --fork-session): Andres wants "resumes", not forks.
//   * cx-origin transcript  -> `exec cx-resume <id> --dangerously-skip-permissions`
//   * daily-claude transcript -> symlink-bridge the jsonl into the CCR projects
//     dir (step 4 of resume-in-cx.sh), then `cx --resume <id> --dangerously-skip-permissions`
//   * codex -> not resumable via cx; return null so the caller shows a toast.
//
// --dangerously-skip-permissions is confirmed correct by reading ~/bin/cx and
// ~/bin/cx-resume: both default to it and de-dupe when the caller also passes it,
// and both forward it through the CCR launcher to claude.

import { existsSync, lstatSync, mkdirSync, readdirSync, realpathSync, rmSync, symlinkSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";
import type { ResumeRef } from "./types.ts";

export interface ResumeResolution {
  /** Shell command the wrapper should `exec`. Null when not resumable. */
  command: string | null;
  /** Human message when command is null (codex, or bridge failure). */
  reason?: string;
}

/** Encode a cwd to the client's project-dir name (matches cx-resume exactly). */
export function encodeCwd(cwd: string): string {
  return cwd.replace(/[^A-Za-z0-9]/g, "-");
}

function ccrDir(): string {
  const home = process.env.HOME ?? homedir();
  return process.env.CCR_DIR ?? join(home, ".claude-code-router", "profiles", "default-claude-code", "claude");
}

/** The daily-claude projects root that bridge symlinks point INTO. */
function dailyProjectsRoot(): string {
  const home = process.env.HOME ?? homedir();
  return join(home, ".claude", "projects");
}

/** Grace (seconds) the reaper waits after boot before removing the bridge symlink. */
function bridgeGraceS(): number {
  const n = Number(process.env.RESUME_PICKER_BRIDGE_GRACE_S);
  return Number.isFinite(n) && n > 0 ? Math.floor(n) : 60;
}

/** Shell-quote a single argument (single-quote wrapping). */
function shq(s: string): string {
  return "'" + s.replace(/'/g, "'\\''") + "'";
}

/**
 * INCIDENT FIX (F6): remove stale bridge symlinks that a previous resume left in
 * the CCR projects tree. A lingering symlink whose target lives under
 * ~/.claude/projects makes cx-resume mis-dispatch the ORIGINAL daily pane as a
 * cx session on the next hr restart. A bridge is only needed transiently while a
 * resume boots, so on every run we sweep every such symlink out of the CCR tree
 * (except `keep`, the bridge this run is about to (re)create). Best-effort;
 * never throws.
 */
export function sweepStaleBridges(keep?: string): number {
  const projectsRoot = join(ccrDir(), "projects");
  const daily = (() => {
    try {
      return realpathSync(dailyProjectsRoot());
    } catch {
      return dailyProjectsRoot();
    }
  })();
  let removed = 0;
  let dirs: string[];
  try {
    dirs = readdirSync(projectsRoot);
  } catch {
    return 0; // no CCR projects dir yet
  }
  for (const d of dirs) {
    const dir = join(projectsRoot, d);
    let entries: string[];
    try {
      entries = readdirSync(dir);
    } catch {
      continue;
    }
    for (const e of entries) {
      const p = join(dir, e);
      if (p === keep) continue;
      let target: string;
      try {
        if (!lstatSync(p).isSymbolicLink()) continue;
        target = realpathSync(p);
      } catch {
        // Dangling symlink (target gone) — still a stale bridge; remove it.
        try {
          rmSync(p, { force: true });
          removed++;
        } catch {}
        continue;
      }
      if (target === daily || target.startsWith(daily + "/")) {
        try {
          rmSync(p, { force: true });
          removed++;
        } catch {}
      }
    }
  }
  return removed;
}

/**
 * Resolve the resume command for a selection. For daily transcripts this has a
 * side effect: it sweeps stale bridges, then symlink-bridges the jsonl into the
 * CCR projects dir (the original stays read-only) so `cx --resume` can find it,
 * and arms a detached reaper that removes THIS bridge once the resumed session
 * has booted — so the original id never persists under the CCR root (F6).
 */
export function resolveResume(ref: ResumeRef): ResumeResolution {
  const id = ref.sessionId;

  if (ref.origin === "codex") {
    return { command: null, reason: "codex sessions can't be resumed via cx (display only)" };
  }

  // Always de-pollute the CCR tree before any resume, cx or daily.
  sweepStaleBridges();

  if (ref.origin === "cx") {
    // cx-resume routes a CCR-owned transcript back up as cx.
    return { command: `exec cx-resume ${shq(id)} --dangerously-skip-permissions` };
  }

  // Daily claude: bridge the transcript into the CCR dir, then force cx.
  const enc = encodeCwd(ref.cwd);
  const projDir = join(ccrDir(), "projects", enc);
  const bridged = join(projDir, `${id}.jsonl`);
  // Sweep again but keep the bridge we're about to use, then (re)create it.
  sweepStaleBridges(bridged);
  try {
    mkdirSync(projDir, { recursive: true });
    if (!existsSync(bridged)) symlinkSync(ref.jsonlPath, bridged);
  } catch (e) {
    return { command: null, reason: `could not bridge transcript into cx: ${(e as Error).message}` };
  }

  // Detached reaper: after the session boots (a real, non-symlink transcript
  // appears at the bridge path, or the grace elapses) remove the lingering
  // bridge symlink so it can't pollute a later restart. trap-cleanup on the
  // wrapper's exit also removes it if the resume fails to boot.
  // Assign the bridge path to a shell var once so neither the reaper subshell nor
  // the EXIT trap has to nest quotes around it (a nested single-quoted path breaks
  // the trap string). __rpb = resume-picker bridge.
  const grace = bridgeGraceS();
  const assign = `__rpb=${shq(bridged)}; `;
  const reaper =
    `( for i in $(seq 1 ${grace}); do sleep 1; ` +
    `if [ -e "$__rpb" ] && [ ! -L "$__rpb" ]; then break; fi; done; ` +
    `[ -L "$__rpb" ] && rm -f "$__rpb"; ) >/dev/null 2>&1 & `;
  const trap = `trap 'rm -f "$__rpb" 2>/dev/null' EXIT; `;
  return { command: `${assign}${reaper}${trap}exec cx --resume ${shq(id)} --dangerously-skip-permissions` };
}
