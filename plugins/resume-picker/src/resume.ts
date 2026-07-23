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

import { existsSync, mkdirSync, symlinkSync } from "node:fs";
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

/** Shell-quote a single argument (single-quote wrapping). */
function shq(s: string): string {
  return "'" + s.replace(/'/g, "'\\''") + "'";
}

/**
 * Resolve the resume command for a selection. For daily transcripts this has a
 * side effect: it symlink-bridges the jsonl into the CCR projects dir (the
 * original stays read-only) so `cx --resume` can find it.
 */
export function resolveResume(ref: ResumeRef): ResumeResolution {
  const id = ref.sessionId;

  if (ref.origin === "codex") {
    return { command: null, reason: "codex sessions can't be resumed via cx (display only)" };
  }

  if (ref.origin === "cx") {
    // cx-resume routes a CCR-owned transcript back up as cx.
    return { command: `exec cx-resume ${shq(id)} --dangerously-skip-permissions` };
  }

  // Daily claude: bridge the transcript into the CCR dir, then force cx.
  const enc = encodeCwd(ref.cwd);
  const projDir = join(ccrDir(), "projects", enc);
  const bridged = join(projDir, `${id}.jsonl`);
  try {
    if (!existsSync(bridged)) {
      mkdirSync(projDir, { recursive: true });
      symlinkSync(ref.jsonlPath, bridged);
    }
  } catch (e) {
    return { command: null, reason: `could not bridge transcript into cx: ${(e as Error).message}` };
  }
  return { command: `exec cx --resume ${shq(id)} --dangerously-skip-permissions` };
}
