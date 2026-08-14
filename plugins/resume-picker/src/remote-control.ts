/**
 * remote-control.ts — Remote Control flags for every claude launch a plugin makes.
 *
 * Every claude session these plugins start is Remote-Control-enabled by default,
 * so it can be picked up from the phone / claude.ai without first walking over
 * to the machine and typing `/remote-control` into the live REPL.
 *
 * Two escape hatches, because a flag that cannot be turned off breaks something
 * eventually:
 *
 *   HARNESS_RC=off   opt out, honoured at every call site. Test rigs set it so a
 *                    CI/e2e launch never registers a Remote Control session.
 *   version guard    a claude too old to know the flags gets none of them. The
 *                    launch must never die on an unknown option.
 *
 * ORDERING IS LOAD-BEARING. `--remote-control [name]` takes an OPTIONAL value,
 * so the argv word after it is swallowed as the session name unless it starts
 * with `-`. `rcFlags()` therefore always returns the two flags as an adjacent
 * pair ending in the prefix VALUE, which is what keeps a trailing positional
 * prompt (runtime-swap hands claude its handoff text that way) out of the
 * session name. Never split the pair, and never put `--remote-control` last.
 *
 * NOTE: runtime-swap-ui/scripts/remote-control.ts is a deliberate sibling copy,
 * for the same reason dash-common.sh has one: a plugin must work when it is the
 * only one installed, so it cannot import a file out of another plugin's
 * directory. Keep the two in sync.
 */

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const HOME = homedir();

/** Where the version probe is cached, keyed on the binary's own size+mtime. */
function cacheFile(): string {
  const base = process.env.XDG_CACHE_HOME || join(HOME, ".cache");
  return join(base, "herdr", "remote-control-support.json");
}

/**
 * The claude binary, or null. `command -v` rather than a PATH walk so a user's
 * own shim still wins, and an absolute path so the stat below is meaningful.
 */
function claudeBinary(): string | null {
  const probe = spawnSync("sh", ["-c", "command -v claude"], { encoding: "utf8" });
  const path = (probe.stdout || "").trim();
  return path && existsSync(path) ? path : null;
}

let supportMemo: boolean | null = null;

/**
 * Does the installed claude know these flags?
 *
 * `claude --help` costs ~0.5s warm and ~3s cold — too much to pay on a keypress
 * — so the answer is cached on disk keyed on the binary's size+mtime, which
 * makes `claude update` re-probe by itself. (The shell side keeps its own cache
 * file beside this one: the two runtimes cannot compute an identical stamp
 * without one of them shelling out to the other, and a stale cross-read would
 * be worse than a second probe.)
 */
export function rcSupported(): boolean {
  if (supportMemo !== null) return supportMemo;
  supportMemo = false;

  const binary = claudeBinary();
  if (!binary) return supportMemo;

  let stamp = "";
  try {
    const st = statSync(binary);
    stamp = `${binary}:${st.size}:${st.mtimeMs}`;
  } catch {
    /* an unstattable binary just means no cache, not no probe */
  }

  const file = cacheFile();
  if (stamp) {
    try {
      const cached = JSON.parse(readFileSync(file, "utf8"));
      if (cached?.stamp === stamp && typeof cached.supported === "boolean") {
        supportMemo = cached.supported;
        return supportMemo;
      }
    } catch {
      /* missing or malformed cache: fall through and probe */
    }
  }

  const help = spawnSync(binary, ["--help"], { encoding: "utf8" });
  supportMemo = (help.stdout || "").includes("--remote-control-session-name-prefix");

  if (stamp) {
    try {
      mkdirSync(join(file, ".."), { recursive: true });
      writeFileSync(file, JSON.stringify({ stamp, supported: supportMemo }), "utf8");
    } catch {
      /* an unwritable cache costs a probe per launch, nothing more */
    }
  }
  return supportMemo;
}

/**
 * A Remote Control session name is shown in the Claude app, so keep it to the
 * portable identifier charset and a glanceable length. An empty or
 * fully-stripped name degrades to "herdr" rather than an empty prefix.
 */
export function rcName(raw: string): string {
  const cleaned = (raw || "")
    .replace(/[^A-Za-z0-9._-]/g, "-")
    .replace(/-{2,}/g, "-")
    .replace(/^-|-$/g, "")
    .slice(0, 40);
  return cleaned || "herdr";
}

/**
 * The flags as argv words: `["--remote-control",
 * "--remote-control-session-name-prefix", "<name>"]`, or `[]` when Remote
 * Control must be omitted. Append them to a claude argv, never after a
 * positional.
 */
export function rcFlags(name: string): string[] {
  if (process.env.HARNESS_RC === "off") return [];
  if (!rcSupported()) return [];
  return ["--remote-control", "--remote-control-session-name-prefix", rcName(name)];
}
