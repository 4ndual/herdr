#!/usr/bin/env bun
/**
 * runtime-swap engine — reopen a pane's live agent session under a different
 * runtime, replacing the pane in place.
 *
 * Conversion follows jcode's import model (crates/jcode-base/src/import.rs):
 * one-way import, never format translation. We parse the source transcript,
 * flatten every message to plain text, drop provider-native reasoning, render
 * tool traffic as summaries, keep the tail under jcode's caps, and hand that to
 * the target runtime. Three native fast paths short-circuit the fallback:
 *
 *   claude -> codex   codex's own app-server importer (externalAgentConfig/import)
 *                     materializes a real rollout, so `codex resume <id>` is a
 *                     true native resume.
 *   * -> jcode        jcode's `--resume` accepts a raw foreign session id and
 *                     runs its own importer — but only for sessions under the
 *                     DEFAULT config root, which both native importers require.
 *   same runtime      never offered; the picker hides the current runtime.
 *
 * Placement uses the pre-0.7.5 `agent start` form (wire `agent.spawn`), which
 * spawns argv directly as a new pane inside the SAME tab; the old pane is closed
 * only after the spawn is confirmed, so the split collapses back to one pane.
 *
 * Usage:
 *   bun swap-runtime.ts --to <claude|codex|pi|jcode> [--pane <ID>] [--dry-run] [--json]
 */

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { basename, join } from "node:path";

// ── jcode's import caps (import.rs:35-37) ──────────────────────────────────
const MAX_MESSAGES = 160;
const MAX_TEXT_BYTES = 256 * 1024;
const MAX_BLOCK_BYTES = 32 * 1024;
/** Inline prompt budget. The full history always lands in a file as well. */
const MAX_INLINE_BYTES = 96 * 1024;

const RUNTIMES = ["claude", "codex", "pi", "jcode"] as const;
type Runtime = (typeof RUNTIMES)[number];

const HOME = homedir();
const HERDR = process.env.HERDR_BIN_PATH || "herdr";

// ── cli ────────────────────────────────────────────────────────────────────

type Args = { to?: string; pane?: string; dryRun: boolean; json: boolean };

function parseArgs(argv: string[]): Args {
  const args: Args = { dryRun: false, json: false };
  for (let i = 0; i < argv.length; i++) {
    switch (argv[i]) {
      case "--to": args.to = argv[++i]; break;
      case "--pane": args.pane = argv[++i]; break;
      case "--dry-run": args.dryRun = true; break;
      case "--json": args.json = true; break;
      default: throw new Error(`unknown option: ${argv[i]}`);
    }
  }
  return args;
}

// ── herdr api ──────────────────────────────────────────────────────────────

function herdr(...args: string[]): any {
  const out = spawnSync(HERDR, args, { encoding: "utf8" });
  if (out.error) throw new Error(`${HERDR} ${args[0]}: ${out.error.message}`);
  const text = (out.stdout || "").trim();
  if (!text) throw new Error(`${HERDR} ${args.join(" ")}: no response (${(out.stderr || "").trim()})`);
  let body: any;
  try {
    body = JSON.parse(text.split("\n").pop() as string);
  } catch {
    throw new Error(`${HERDR} ${args.join(" ")}: unparsable response: ${text.slice(0, 200)}`);
  }
  if (body.error) throw new Error(`${args.join(" ")} failed: ${body.error.code}: ${body.error.message}`);
  return body.result;
}

function focusedPaneId(): string {
  const panes = herdr("pane", "list").panes as any[];
  const focused = panes.find((pane) => pane.focused) ?? panes[0];
  if (!focused) throw new Error("no panes");
  return focused.pane_id;
}

type Source = {
  paneId: string;
  tabId: string;
  cwd: string;
  runtime: Runtime;
  sessionKind: "id" | "path";
  sessionValue: string;
  label: string;
  /** Config-root vars the source pane runs under (harness roots, mostly). */
  env: Record<string, string>;
};

/**
 * Which environment variable pins each runtime's config root. Panes inherit
 * these from the herdr server, so a pane can be running inside a harness root
 * the swap process itself knows nothing about — which is exactly what makes
 * trust seeding and transcript discovery miss if we only look at our own env.
 */
const CONFIG_ROOT_VAR: Record<Runtime, string> = {
  claude: "CLAUDE_CONFIG_DIR",
  codex: "CODEX_HOME",
  pi: "PI_SESSION_DIR",
  jcode: "JCODE_HOME",
};

/** The source pane's own environment, read from its foreground process. */
function paneProcessEnv(paneId: string): Record<string, string> {
  if (process.platform !== "linux") return {};
  let pid: number | undefined;
  try {
    const info = herdr("pane", "process-info", "--pane", paneId).process_info;
    pid = info?.foreground_process_group_id;
  } catch {
    return {};
  }
  if (!pid) return {};
  const env: Record<string, string> = {};
  const keep = (key: string, value: string) => {
    if (Object.values(CONFIG_ROOT_VAR).includes(key)) env[key] = value;
  };
  try {
    for (const entry of readFileSync(`/proc/${pid}/environ`, "utf8").split("\0")) {
      const split = entry.indexOf("=");
      if (split <= 0) continue;
      keep(entry.slice(0, split), entry.slice(split + 1));
    }
    return env;
  } catch {
    /* the process may have exited, or there is no /proc — macOS. */
  }
  // macOS has no /proc. Without this the swap silently dropped the pane's
  // config root on every Mac: the catch above returned {}, the new runtime
  // launched against the DEFAULT root, and the session it was supposed to carry
  // over simply was not there. `ps -Eww` prints the environment after the
  // command, space separated.
  try {
    const out = Bun.spawnSync(["ps", "-Eww", "-o", "command=", "-p", String(pid)]);
    if (out.exitCode === 0) {
      for (const token of new TextDecoder().decode(out.stdout).split(/\s+/)) {
        const split = token.indexOf("=");
        if (split <= 0) continue;
        keep(token.slice(0, split), token.slice(split + 1));
      }
    }
  } catch {
    /* ps is unavailable or refused; the caller falls back to the default root */
  }
  return env;
}

function readSource(paneId: string): Source {
  const pane = herdr("pane", "get", paneId).pane;
  const agent = normalizeRuntime(pane.agent_session?.agent || pane.agent || pane.display_agent);
  if (!agent) {
    throw new Error(`pane ${paneId} is not running a supported agent (claude, codex, pi, jcode)`);
  }
  const cwd = pane.cwd || pane.foreground_cwd || HOME;
  const paneEnv = paneProcessEnv(paneId);
  const session = pane.agent_session;
  // herdr only knows a session id when the runtime's integration hook reports
  // one, which is not guaranteed per runtime or per machine. Fall back to the
  // newest transcript this runtime wrote for this cwd — the same recovery
  // `fork-conversation.sh` already does for claude, generalized to all roots.
  const fallback = session?.value ? null : newestTranscriptForCwd(agent, cwd);
  if (!session?.value && !fallback) {
    throw new Error(
      `pane ${paneId} has no live ${agent} session yet — let the agent exchange at least one message first`,
    );
  }
  return {
    paneId: pane.pane_id,
    tabId: pane.tab_id,
    cwd,
    runtime: agent,
    sessionKind: session?.value ? (session.kind === "path" ? "path" : "id") : "path",
    sessionValue: session?.value ?? (fallback as string),
    label: pane.label || pane.title || agent,
    env: paneEnv,
  };
}

/**
 * The config root the reopened runtime will use: whatever the source pane runs
 * under, else our own environment. Returning it (rather than guessing) lets the
 * swap seed trust in the right file and pin the same root on the new pane.
 */
function targetConfigRoot(target: Runtime, source: Source): string | null {
  const key = CONFIG_ROOT_VAR[target];
  return source.env[key] ?? process.env[key] ?? null;
}

/**
 * The session id, whether herdr handed us one or we recovered a transcript
 * path. Every supported runtime embeds the uuid in the transcript filename:
 * claude `<uuid>.jsonl`, codex `rollout-<ts>-<uuid>.jsonl`, pi `<ts>_<uuid>.jsonl`.
 */
function sessionId(source: Source): string {
  if (source.sessionKind === "id") return source.sessionValue;
  const name = basename(source.sessionValue).replace(/\.jsonl$/, "");
  const uuid = name.match(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i);
  return uuid ? uuid[0] : name;
}

/** Newest transcript a runtime wrote for `cwd`, across every config root. */
function newestTranscriptForCwd(runtime: Runtime, cwd: string): string | null {
  let candidates: string[] = [];
  if (runtime === "claude") {
    for (const root of configRoots("CLAUDE_CONFIG_DIR", ".claude")) {
      candidates.push(...walkFiles(join(root, "projects", claudeSlug(cwd)), (name) => name.endsWith(".jsonl")));
    }
  } else if (runtime === "codex") {
    for (const root of configRoots("CODEX_HOME", ".codex")) {
      for (const path of walkFiles(join(root, "sessions"), (name) => name.endsWith(".jsonl"))) {
        if (codexRolloutCwd(path) === cwd) candidates.push(path);
      }
    }
  } else if (runtime === "pi") {
    const root = process.env.PI_SESSION_DIR || join(HOME, ".pi", "agent", "sessions");
    candidates = walkFiles(root, (name) => name.endsWith(".jsonl")).filter((path) => piSessionCwd(path) === cwd);
  }
  const newest = candidates
    .map((path) => ({ path, mtime: statSync(path).mtimeMs }))
    .sort((a, b) => a.mtime - b.mtime)
    .pop();
  return newest?.path ?? null;
}

function headerField(path: string, field: string): string | null {
  try {
    const first = readFileSync(path, "utf8").split("\n", 1)[0];
    const header = JSON.parse(first);
    return (header.payload?.[field] ?? header[field] ?? null) as string | null;
  } catch {
    return null;
  }
}

const codexRolloutCwd = (path: string) => headerField(path, "cwd");
const piSessionCwd = (path: string) => headerField(path, "cwd");

/** `cx` is a local alias for claude; anything else unsupported returns null. */
function normalizeRuntime(name: unknown): Runtime | null {
  if (typeof name !== "string") return null;
  const value = name.trim().toLowerCase();
  const canonical = value === "cx" ? "claude" : value;
  return (RUNTIMES as readonly string[]).includes(canonical) ? (canonical as Runtime) : null;
}

// ── transcript discovery ───────────────────────────────────────────────────

/** Every CLAUDE_CONFIG_DIR / CODEX_HOME root on this machine, not just the default one. */
function configRoots(envVar: string, fallback: string): string[] {
  const roots: string[] = [];
  const fromEnv = process.env[envVar];
  if (fromEnv) roots.push(fromEnv);
  roots.push(join(HOME, fallback));
  const harnesses = join(HOME, "harnesses");
  if (existsSync(harnesses)) {
    for (const entry of safeReaddir(harnesses)) {
      const root = join(harnesses, entry, "config");
      if (existsSync(root)) roots.push(root);
    }
  }
  return [...new Set(roots)];
}

function safeReaddir(dir: string): string[] {
  try {
    return readdirSync(dir);
  } catch {
    return [];
  }
}

function walkFiles(dir: string, match: (name: string) => boolean, out: string[] = [], depth = 6): string[] {
  if (depth < 0) return out;
  for (const entry of safeReaddir(dir)) {
    const path = join(dir, entry);
    let stats;
    try {
      stats = statSync(path);
    } catch {
      continue;
    }
    if (stats.isDirectory()) walkFiles(path, match, out, depth - 1);
    else if (match(entry)) out.push(path);
  }
  return out;
}

function claudeSlug(cwd: string): string {
  return cwd.replace(/[^a-zA-Z0-9]/g, "-");
}

function findTranscript(source: Source): string | null {
  if (source.sessionKind === "path") {
    return existsSync(source.sessionValue) ? source.sessionValue : null;
  }
  const id = source.sessionValue;
  switch (source.runtime) {
    case "claude": {
      for (const root of configRoots("CLAUDE_CONFIG_DIR", ".claude")) {
        const direct = join(root, "projects", claudeSlug(source.cwd), `${id}.jsonl`);
        if (existsSync(direct)) return direct;
      }
      // The cwd slug can drift (worktrees, symlinks); fall back to a scan.
      for (const root of configRoots("CLAUDE_CONFIG_DIR", ".claude")) {
        const hit = walkFiles(join(root, "projects"), (name) => name === `${id}.jsonl`);
        if (hit.length) return hit[0];
      }
      return null;
    }
    case "codex": {
      for (const root of configRoots("CODEX_HOME", ".codex")) {
        const hit = walkFiles(join(root, "sessions"), (name) => name.endsWith(`-${id}.jsonl`));
        if (hit.length) return hit[0];
      }
      return null;
    }
    case "pi": {
      const root = process.env.PI_SESSION_DIR || join(HOME, ".pi", "agent", "sessions");
      const hit = walkFiles(root, (name) => name.endsWith(`_${id}.jsonl`) || name === `${id}.jsonl`);
      return hit.length ? hit[0] : null;
    }
    case "jcode":
      return null;
  }
}

// ── transcript parsing → flat messages (jcode's convert + normalize) ───────

type Message = { role: string; text: string };

function readLines(path: string): any[] {
  const records: any[] = [];
  for (const line of readFileSync(path, "utf8").split("\n")) {
    // jcode's byte-level prefilter: transcripts are dominated by non-message
    // records (mode, permission-mode, file-history-snapshot, reasoning, telemetry).
    if (!line.includes('"role"')) continue;
    try {
      records.push(JSON.parse(line));
    } catch {
      /* partial trailing write */
    }
  }
  return records;
}

function renderBlocks(content: unknown): string {
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  const parts: string[] = [];
  for (const block of content) {
    if (typeof block === "string") {
      parts.push(block);
      continue;
    }
    if (!block || typeof block !== "object") continue;
    switch (block.type) {
      case "text":
      case "output_text":
      case "input_text":
        parts.push(String(block.text ?? ""));
        break;
      // Provider-native reasoning cannot safely be replayed under a different
      // provider — jcode drops it, so do we.
      case "thinking":
      case "redacted_thinking":
      case "reasoning":
        break;
      case "tool_use":
        parts.push(`[tool call: ${block.name ?? "unknown"}]\n${trim(JSON.stringify(block.input ?? {}))}`);
        break;
      case "tool_result":
        parts.push(`[tool result: ${block.tool_use_id ?? ""}]\n${trim(renderBlocks(block.content))}`);
        break;
      case "image":
        parts.push(`[image: ${block.source?.media_type ?? "image"}]`);
        break;
      default:
        break;
    }
  }
  return parts.filter(Boolean).join("\n");
}

function trim(text: string): string {
  const bytes = Buffer.from(text, "utf8");
  if (bytes.length <= MAX_BLOCK_BYTES) return text;
  return `${bytes.subarray(0, MAX_BLOCK_BYTES).toString("utf8")}\n[... ${bytes.length - MAX_BLOCK_BYTES} bytes omitted from imported history]`;
}

function parseTranscript(runtime: Runtime, path: string): Message[] {
  const messages: Message[] = [];
  for (const record of readLines(path)) {
    let role: unknown;
    let content: unknown;
    if (runtime === "claude") {
      if (record.type !== "user" && record.type !== "assistant") continue;
      role = record.message?.role ?? record.type;
      content = record.message?.content;
    } else if (runtime === "codex") {
      const payload = record.type === "response_item" ? record.payload : record;
      if (payload?.type && payload.type !== "message") continue;
      role = payload?.role;
      content = payload?.content;
    } else {
      // pi (and anything else that nests the payload under `message`)
      if (record.type && record.type !== "message") continue;
      role = record.message?.role ?? record.role;
      content = record.message?.content ?? record.content;
    }
    if (role !== "user" && role !== "assistant") continue;
    const text = renderBlocks(content).trim();
    if (text) messages.push({ role: String(role), text });
  }
  return messages;
}

/** jcode's normalize_imported_history: keep the tail, mark what was dropped. */
function normalize(messages: Message[]): { kept: Message[]; dropped: number } {
  let kept = messages.slice(-MAX_MESSAGES);
  let total = 0;
  const budgeted: Message[] = [];
  for (let i = kept.length - 1; i >= 0; i--) {
    const size = Buffer.byteLength(kept[i].text, "utf8") + kept[i].role.length + 2;
    if (total + size > MAX_TEXT_BYTES && budgeted.length) break;
    total += size;
    budgeted.unshift(kept[i]);
  }
  kept = budgeted;
  return { kept, dropped: messages.length - kept.length };
}

// ── handoff prompt ─────────────────────────────────────────────────────────

function stateDir(): string {
  const dir = join(process.env.XDG_RUNTIME_DIR || tmpdir(), "herdr-runtime-swap");
  mkdirSync(dir, { recursive: true });
  return dir;
}

function buildHandoff(source: Source, target: Runtime, messages: Message[], transcript: string) {
  const { kept, dropped } = normalize(messages);
  const body = kept.map((message) => `### ${message.role}\n${message.text}`).join("\n\n");
  const historyPath = join(stateDir(), `${source.runtime}-to-${target}-${basename(transcript)}.md`);
  writeFileSync(historyPath, body, "utf8");

  const header = [
    `Continuing a conversation started under ${source.runtime} (session ${sessionId(source)}).`,
    `This history is a flattened, provider-safe import: reasoning was dropped and tool traffic is summarized, exactly as jcode imports foreign sessions.`,
    dropped > 0
      ? `[Imported session: ${dropped} older messages were omitted for a fast, provider-safe resume.]`
      : "",
    `Full imported history: ${historyPath}`,
    `Original transcript: ${transcript}`,
    `Pick up where it left off. Do not restart the task, and do not re-run work that already succeeded.`,
    "",
    "--- history ---",
  ]
    .filter(Boolean)
    .join("\n");

  let inline = body;
  let truncatedInline = false;
  const budget = MAX_INLINE_BYTES - Buffer.byteLength(header, "utf8");
  if (Buffer.byteLength(inline, "utf8") > budget) {
    inline = Buffer.from(inline, "utf8").subarray(-budget).toString("utf8");
    truncatedInline = true;
  }

  const prompt = [
    header,
    truncatedInline ? "[earlier turns trimmed for argv safety — read the full imported history file above]" : "",
    inline,
    "--- end history ---",
  ]
    .filter(Boolean)
    .join("\n");

  return { prompt, historyPath, kept: kept.length, dropped };
}

// ── native fast paths ──────────────────────────────────────────────────────

/**
 * Absolute path of a runtime binary, or null. PATH alone is not enough: plugin
 * panes and spawned agent panes inherit the herdr server's environment, where
 * per-user bin dirs added by an interactive shell rc (notably `~/.bun/bin`) are
 * missing — `pi` reads as "not installed" while `jcode` resolves. Resolving to
 * an absolute path also means the new pane does not depend on PATH at all.
 */
function which(bin: string): string | null {
  const out = spawnSync("sh", ["-lc", `command -v ${bin}`], { encoding: "utf8" });
  const path = (out.stdout || "").trim().split("\n")[0];
  if (path) return path;
  for (const dir of [".local/bin", ".bun/bin", ".npm-global/bin"]) {
    const candidate = join(HOME, dir, bin);
    if (existsSync(candidate)) return candidate;
  }
  for (const dir of ["/usr/local/bin", "/opt/homebrew/bin", "/usr/bin"]) {
    const candidate = join(dir, bin);
    if (existsSync(candidate)) return candidate;
  }
  return null;
}

/**
 * codex's own Claude Code importer, driven over the app-server stdio protocol.
 * Verified on codex-cli 0.147.0: it materializes a real rollout under
 * $CODEX_HOME/sessions and returns its thread id, so the result is a true
 * native `codex resume`, not a text handoff. Import is deduped by content hash,
 * so re-running is idempotent.
 */
function codexHome(): string {
  return process.env.CODEX_HOME || join(HOME, ".codex");
}

/** codex's import ledger, keyed by the source transcript path. */
function previouslyImportedThread(transcript: string, codexRoot: string | null): string | null {
  const ledger = join(codexRoot ?? codexHome(), "external_agent_session_imports.json");
  if (!existsSync(ledger)) return null;
  try {
    const records = JSON.parse(readFileSync(ledger, "utf8")).records ?? [];
    const matches = records
      .filter((record: any) => record.source_path === transcript && record.imported_thread_id)
      .sort((a: any, b: any) => (a.imported_at ?? 0) - (b.imported_at ?? 0));
    return matches.length ? (matches[matches.length - 1].imported_thread_id as string) : null;
  } catch {
    return null;
  }
}

async function codexImportClaudeSession(
  transcript: string,
  cwd: string,
  codexRoot: string | null,
): Promise<string | null> {
  // The import MUST run under the same CODEX_HOME the reopened pane will use,
  // or the rollout lands in one root while the resume looks in another.
  const proc = Bun.spawn(["codex", "app-server"], {
    stdin: "pipe",
    stdout: "pipe",
    stderr: "ignore",
    env: codexRoot ? { ...process.env, CODEX_HOME: codexRoot } : process.env,
  });
  const send = (message: unknown) => proc.stdin.write(`${JSON.stringify(message)}\n`);
  send({ id: 1, method: "initialize", params: { clientInfo: { name: "herdr-runtime-swap", title: "herdr runtime swap", version: "0.1.0" } } });

  const decoder = new TextDecoder();
  const deadline = Date.now() + 60_000;
  let buffer = "";
  let requested = false;
  try {
    for await (const chunk of proc.stdout) {
      buffer += decoder.decode(chunk);
      let newline: number;
      while ((newline = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, newline);
        buffer = buffer.slice(newline + 1);
        if (!line.trim()) continue;
        let message: any;
        try {
          message = JSON.parse(line);
        } catch {
          continue;
        }
        if (message.id === 1 && message.result && !requested) {
          requested = true;
          send({
            id: 2,
            method: "externalAgentConfig/import",
            params: {
              migrationSource: "claude_code",
              providerId: "claude_code",
              source: "herdr-runtime-swap",
              migrationItems: [
                {
                  itemType: "SESSIONS",
                  description: "herdr runtime swap: single session handoff",
                  cwd,
                  details: { sessions: [{ path: transcript, cwd, title: "Reopened from Claude Code" }] },
                },
              ],
            },
          });
        }
        if (message.method === "externalAgentConfig/import/completed") {
          const results = message.params?.itemTypeResults ?? [];
          for (const result of results) {
            for (const success of result.successes ?? []) {
              if (success.itemType === "SESSIONS" && success.target) return success.target as string;
            }
          }
          // Import is deduped by content hash: a transcript codex already holds
          // reports no successes at all. The earlier thread is still the right
          // target, so recover it from codex's own import ledger.
          return previouslyImportedThread(transcript, codexRoot);
        }
      }
      if (Date.now() > deadline) return null;
    }
  } finally {
    proc.kill();
  }
  return null;
}

// ── launch plan ────────────────────────────────────────────────────────────

const SKIP_FLAGS: Record<Runtime, string[]> = {
  claude: ["--dangerously-skip-permissions"],
  codex: ["--dangerously-bypass-approvals-and-sandbox"],
  pi: ["--approve"],
  jcode: [],
};

/**
 * claude has no flag for its workspace-trust dialog, only the `~/.claude.json`
 * record the dialog writes. The swap always reopens the cwd the source pane was
 * ALREADY running an agent in, so this can never trust a directory the user has
 * not already worked in — and without it the handoff prompt dies behind the
 * modal. Existing entries are never rewritten, and the file is backed up once.
 */
function seedClaudeTrust(cwd: string, configRoot: string | null) {
  const path = join(configRoot ?? HOME, ".claude.json");
  if (!existsSync(path)) return;
  try {
    const raw = readFileSync(path, "utf8");
    const config = JSON.parse(raw);
    if (config.projects?.[cwd]?.hasTrustDialogAccepted) return;
    writeFileSync(`${path}.herdr-runtime-swap.bak`, raw, "utf8");
    config.projects = config.projects ?? {};
    config.projects[cwd] = { ...(config.projects[cwd] ?? {}), hasTrustDialogAccepted: true };
    writeFileSync(path, JSON.stringify(config, null, 2), "utf8");
  } catch {
    // A malformed or unreadable config is the user's to fix; the swap still
    // works, the user just answers the trust dialog once.
  }
}

/**
 * A reopened session must land at a live prompt, not behind a modal. codex asks
 * "do you trust this directory?" for an unseen cwd even in bypass mode, and the
 * dialog swallows the handoff prompt. Declaring the trust as a per-invocation
 * `-c` override keeps the user's own config.toml untouched (unlike the agent
 * launcher, which writes trust into it). `trust_level` is a real config field:
 * `codex --strict-config` accepts it and rejects a made-up sibling.
 */
function trustFlags(target: Runtime, cwd: string): string[] {
  if (target !== "codex") return [];
  void cwd;
  // codex also stops on a hook-review modal when the machine's hooks changed.
  // The reopened pane already runs in the bypass mode the swap is defined to
  // use, so stalling behind that modal only loses the handoff.
  return ["--dangerously-bypass-hook-trust"];
}

/**
 * codex's directory-trust dialog reads persisted project trust, not `-c`
 * overrides: `-c projects."<cwd>".trust_level="trusted"` parses (and passes
 * `--strict-config`) but the dialog still appears. Persisting the same key is
 * what actually skips it. Same rule as claude's: only the cwd an agent is
 * already running in, never rewritten if present, one backup.
 */
function seedCodexTrust(cwd: string, configRoot: string | null) {
  const path = join(configRoot ?? codexHome(), "config.toml");
  if (!existsSync(path)) return;
  try {
    const raw = readFileSync(path, "utf8");
    const header = `[projects."${cwd}"]`;
    if (raw.includes(header)) return;
    writeFileSync(`${path}.herdr-runtime-swap.bak`, raw, "utf8");
    writeFileSync(path, `${raw.replace(/\n*$/, "\n")}\n${header}\ntrust_level = "trusted"\n`, "utf8");
  } catch {
    // Same as claude: worst case the user answers the dialog once.
  }
}

type Plan = {
  argv: string[];
  strategy: string;
  note: string;
  /** Handoff text to deliver after launch, for runtimes with no argv prompt. */
  prompt?: string;
};

/**
 * Which runtimes accept a first prompt as a positional argument and still open
 * their interactive UI. claude, codex and pi do; jcode's CLI has `--resume` but
 * no positional prompt, so its handoff is delivered through
 * `herdr agent prompt` once the pane reports the agent ready.
 */
const TAKES_ARGV_PROMPT: Record<Runtime, boolean> = {
  claude: true,
  codex: true,
  pi: true,
  jcode: false,
};

/**
 * Both native importers discover sessions under the DEFAULT config root only:
 * codex's detector scans `~/.claude/projects` (a harness-root transcript is
 * rejected with `session_not_detected`, and pointing `CLAUDE_CONFIG_DIR` at the
 * harness does not help), and jcode resolves foreign ids through its own
 * `user_home_path()`. Handing either a harness-root session is not a soft miss:
 * jcode prints "No session found" and exits, which kills the pane we just
 * spawned — and a single-pane tab with it. So gate the native paths on
 * visibility and let the flattened handoff take those pairs.
 *
 * pi is exempt: jcode keys pi sessions by path, not by id.
 */
function nativelyVisible(runtime: Runtime, transcript: string | null): boolean {
  if (!transcript) return false;
  if (runtime === "pi") return true;
  const defaultRoot = runtime === "codex" ? join(HOME, ".codex") : join(HOME, ".claude");
  return transcript.startsWith(`${defaultRoot}/`);
}

async function buildPlan(
  source: Source,
  target: Runtime,
  transcript: string | null,
  configRoot: string | null,
): Promise<Plan> {
  // jcode imports every supported foreign session itself; `--resume` takes a
  // jcode id, a raw foreign session id, or a pi session path, and probes
  // codex -> claude -> opencode -> cursor -> pi path.
  if (target === "jcode" && nativelyVisible(source.runtime, transcript)) {
    return {
      argv: [
        "jcode",
        "--resume",
        source.runtime === "pi" && source.sessionKind === "path" ? source.sessionValue : sessionId(source),
      ],
      strategy: "native:jcode-import",
      note: `jcode imports the ${source.runtime} session natively`,
    };
  }

  if (
    target === "codex" &&
    source.runtime === "claude" &&
    transcript &&
    nativelyVisible(source.runtime, transcript)
  ) {
    const threadId = await codexImportClaudeSession(transcript, source.cwd, configRoot);
    if (threadId) {
      return {
        argv: ["codex", ...trustFlags("codex", source.cwd), "resume", ...SKIP_FLAGS.codex, threadId],
        strategy: "native:codex-import",
        note: `codex imported the claude session as thread ${threadId}`,
      };
    }
  }

  if (!transcript) {
    throw new Error(
      `could not locate the ${source.runtime} transcript for session ${sessionId(source)}; no native path to ${target} either`,
    );
  }
  const messages = parseTranscript(source.runtime, transcript);
  if (!messages.length) throw new Error(`transcript ${transcript} has no replayable user/assistant turns yet`);
  const handoff = buildHandoff(source, target, messages, transcript);
  const launch = [target, ...trustFlags(target, source.cwd), ...SKIP_FLAGS[target]];
  const viaArgv = TAKES_ARGV_PROMPT[target];
  return {
    argv: viaArgv ? [...launch, handoff.prompt] : launch,
    prompt: viaArgv ? undefined : handoff.prompt,
    strategy: "import:flattened-handoff",
    note: `${handoff.kept} messages carried over (${handoff.dropped} older omitted); full history at ${handoff.historyPath}`,
  };
}

// ── placement ──────────────────────────────────────────────────────────────

function uniqueLabel(target: Runtime): string {
  const taken = new Set<string>();
  try {
    for (const agent of herdr("agent", "list").agents as any[]) taken.add(String(agent.name));
  } catch {
    /* an empty agent list is fine */
  }
  if (!taken.has(target)) return target;
  for (let n = 2; n < 1000; n++) {
    const candidate = `${target}-${n}`;
    if (!taken.has(candidate)) return candidate;
  }
  return `${target}-${process.pid}`;
}

function replacePane(
  source: Source,
  target: Runtime,
  argv: string[],
  configRoot: string | null,
  prompt: string | undefined,
): string {
  // Pin the reopened runtime to the same config root the replaced pane used —
  // otherwise a harness-scoped pane reopens against the default root, and the
  // trust we just seeded lands in the wrong file.
  if (target === "claude") seedClaudeTrust(source.cwd, configRoot);
  if (target === "codex") seedCodexTrust(source.cwd, configRoot);
  const envArgs = configRoot ? ["--env", `${CONFIG_ROOT_VAR[target]}=${configRoot}`] : [];
  const label = uniqueLabel(target);
  // Spawn BEFORE closing: if the tab holds only the source pane, closing first
  // would destroy the tab.
  const spawned = herdr(
    "agent", "start", label,
    "--tab", source.tabId,
    "--split", "right",
    "--cwd", source.cwd,
    ...envArgs,
    "--focus",
    "--", ...argv,
  );
  const newPane = spawned.agent?.pane_id;
  if (!newPane) throw new Error("agent spawn returned no pane id; leaving the source pane untouched");
  // With the handoff in argv the pane is already carrying the history, so the
  // source can go immediately. When it has to be typed, wait for the agent to
  // become interactive first and only then drop the source — if the target
  // never comes up, the user still has the conversation they started with.
  if (prompt) waitForAgentThenPrompt(newPane, prompt);
  herdr("pane", "close", source.paneId);
  return newPane;
}

function waitForAgentThenPrompt(paneId: string, prompt: string) {
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    try {
      const pane = herdr("pane", "get", paneId).pane;
      if (pane.agent && pane.agent_status && pane.agent_status !== "unknown") {
        herdr("agent", "prompt", paneId, prompt);
        return;
      }
    } catch {
      /* the pane may not be registered yet */
    }
    spawnSync("sleep", ["1"]);
  }
  throw new Error(`${paneId} never became an interactive agent; the handoff was not delivered`);
}

// ── main ───────────────────────────────────────────────────────────────────

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const target = normalizeRuntime(args.to);
  if (!target) throw new Error(`--to must be one of ${RUNTIMES.join(", ")}`);

  const source = readSource(args.pane || process.env.HERDR_PANE_ID || focusedPaneId());
  if (source.runtime === target) throw new Error(`pane ${source.paneId} already runs ${target}`);
  const binary = which(target);
  if (!binary) throw new Error(`${target} is not installed on this machine`);

  const transcript = findTranscript(source);
  const configRoot = targetConfigRoot(target, source);
  const plan = await buildPlan(source, target, transcript, configRoot);
  // The spawned pane inherits the server environment, so launch by absolute path.
  plan.argv[0] = binary;

  if (args.dryRun) {
    report(args.json, {
      dry_run: true,
      source_pane: source.paneId,
      tab: source.tabId,
      from: source.runtime,
      to: target,
      session: sessionId(source),
      transcript,
      strategy: plan.strategy,
      note: plan.note,
      argv: plan.argv.map((arg) => (arg.length > 200 ? `${arg.slice(0, 200)}… (${arg.length} chars)` : arg)),
    });
    return;
  }

  const newPane = replacePane(source, target, plan.argv, configRoot, plan.prompt);
  report(args.json, {
    source_pane: source.paneId,
    new_pane: newPane,
    tab: source.tabId,
    from: source.runtime,
    to: target,
    session: sessionId(source),
    transcript,
    strategy: plan.strategy,
    note: plan.note,
  });
}

function report(asJson: boolean, payload: Record<string, unknown>) {
  if (asJson) {
    console.log(JSON.stringify(payload, null, 2));
    return;
  }
  console.log(`${payload.from} → ${payload.to} (${payload.strategy})`);
  console.log(payload.note);
}

main().catch((err) => {
  const message = err instanceof Error ? err.message : String(err);
  if (process.argv.includes("--json")) console.log(JSON.stringify({ error: message }));
  else console.error(`runtime-swap: ${message}`);
  process.exit(1);
});
