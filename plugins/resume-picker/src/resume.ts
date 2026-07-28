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

/** True if `p` is an existing directory. */
function dirExists(p: string): boolean {
  try {
    return lstatSync(p).isDirectory();
  } catch {
    return false;
  }
}

/**
 * Pick the working directory the resume will launch FROM. Both `cx-resume` and
 * `cx --resume` locate a transcript by encoding $PWD into the project-dir name,
 * so the launch cwd is load-bearing — the incident ("No conversation found")
 * was a resume fired from ~ for a session whose transcript lived under a
 * different cwd's encoding. We therefore always `cd` into a KNOWN-GOOD dir:
 *   - the session's own cwd when it still exists (or can be recreated), so the
 *     transcript is found at its natural encoding with no bridge; else
 *   - $HOME (always exists), with the transcript bridged into $HOME's encoding.
 * Never relies on wherever the pane happens to be.
 */
function launchDir(cwd: string): { wd: string; recreated: boolean } {
  if (dirExists(cwd)) return { wd: cwd, recreated: false };
  try {
    mkdirSync(cwd, { recursive: true });
    if (dirExists(cwd)) return { wd: cwd, recreated: true };
  } catch {
    // fall through to HOME
  }
  return { wd: process.env.HOME ?? homedir(), recreated: false };
}

/**
 * Ensure a transcript for the session is findable at `<projDir>/<id>.jsonl`.
 * Returns "linked" when it created a bridge symlink (caller must arm the reaper
 * to clean it up), or "real" when a genuine transcript already lives there (no
 * reaper — an EXIT-trap must never delete a real file). Throws on fs error.
 */
function ensureBridge(projDir: string, bridged: string, jsonlPath: string): "linked" | "real" {
  mkdirSync(projDir, { recursive: true });
  try {
    const st = lstatSync(bridged);
    if (!st.isSymbolicLink()) return "real"; // real transcript already here — resuming it is correct
    // A symlink: only reuse it if it already points at our target, else replace.
    try {
      if (realpathSync(bridged) === realpathSync(jsonlPath)) return "linked";
    } catch {
      // dangling — replace below
    }
    rmSync(bridged, { force: true });
  } catch {
    // not present — create below
  }
  symlinkSync(jsonlPath, bridged);
  return "linked";
}

/** Reaper + EXIT-trap shell prelude that removes the bridge symlink after boot (F6). */
function bridgeReaper(bridged: string): string {
  const grace = bridgeGraceS();
  const assign = `__rpb=${shq(bridged)}; `;
  const reaper =
    `( for i in $(seq 1 ${grace}); do sleep 1; ` +
    `if [ -e "$__rpb" ] && [ ! -L "$__rpb" ]; then break; fi; done; ` +
    `[ -L "$__rpb" ] && rm -f "$__rpb"; ) >/dev/null 2>&1 & `;
  const trap = `trap 'rm -f "$__rpb" 2>/dev/null' EXIT; `;
  return `${assign}${reaper}${trap}`;
}

/**
 * Resolve the resume command for a selection. The command ALWAYS `cd`s into a
 * known-good dir so the launcher's $PWD-based transcript lookup can never miss
 * (the P0 "resume must never fail" fix). A cx transcript resumed from its own
 * (surviving) cwd needs no bridge; a daily transcript — or a cx transcript whose
 * cwd is gone — is symlink-bridged into the launch dir's encoding under the CCR
 * root, with the F6 reaper/trap lifecycle so nothing lingers. Never falls
 * through to a launcher that would resume the wrong (or no) session.
 */
export function resolveResume(ref: ResumeRef): ResumeResolution {
  const id = ref.sessionId;

  if (ref.origin === "codex") {
    return { command: null, reason: "codex sessions can't be resumed via cx (display only)" };
  }
  if (!ref.jsonlPath || !existsSync(ref.jsonlPath)) {
    return { command: null, reason: `transcript file for ${id.slice(0, 8)} not found on disk` };
  }

  // Always de-pollute the CCR tree before any resume, cx or daily.
  sweepStaleBridges();

  const { wd } = launchDir(ref.cwd);
  const enc = encodeCwd(wd);
  const projDir = join(ccrDir(), "projects", enc);
  const bridged = join(projDir, `${id}.jsonl`);
  const cd = `cd ${shq(wd)} && `;

  // Guarantee the transcript is findable at encode(wd) under the CCR root, then
  // launch from wd. cx transcripts resumed from their own surviving cwd need no
  // bridge (they're already there as a real file); everything else gets a
  // reaped symlink bridge. cx-origin -> cx-resume (keeps GPT routing), daily ->
  // cx --resume (Andres wants daily sessions resumed as cx).
  sweepStaleBridges(bridged);
  let bridgeState: "linked" | "real";
  try {
    bridgeState = ensureBridge(projDir, bridged, ref.jsonlPath);
  } catch (e) {
    return { command: null, reason: `could not bridge transcript into cx: ${(e as Error).message}` };
  }
  const prelude = bridgeState === "linked" ? bridgeReaper(bridged) : "";
  const launcher =
    ref.origin === "cx"
      ? `exec cx-resume ${shq(id)} --dangerously-skip-permissions`
      : `exec cx --resume ${shq(id)} --dangerously-skip-permissions`;
  return { command: `${prelude}${cd}${launcher}` };
}
