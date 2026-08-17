#!/usr/bin/env bun
/**
 * The isolated stage the gate records: its own Xvfb, its own herdr session, its
 * own fleet root, and one real pane per fixture instance.
 *
 * Isolated on purpose. The live `hr` session is the operator's actual working
 * session and other agents are editing this plugin right now; driving clicks
 * there would move a human's focus and read as noise in the recording. Nothing
 * here touches ~/harnesses or the running session.
 *
 * Three isolation seams:
 *   DISPLAY        a private Xvfb, so x11grab records only this scenario
 *   XDG_*_HOME     the herdr-test config tree, so the live session is untouched
 *   HARNESS_ROOT   a private fleet root, honoured by the tidus-bar collector
 *                  (collector/src/main.rs: env_nonempty("HARNESS_ROOT")), so the
 *                  bar sees ONLY the fixture tree and the live fleet never
 *                  appears on camera
 *
 * The fleet is fixtures; the SESSION, the PANES and the CLICKS are real. The
 * collector is the real one and nothing here writes the snapshot it renders —
 * parentage is read off harness.json `team` edges exactly as in production.
 * Fixtures rather than `harness spawn` because a spawn costs real model calls
 * per instance and buys the gate nothing: the bar renders the spawn GRAPH, and
 * a graph made of files is the same graph.
 */
import { spawnSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync, existsSync } from "node:fs";

export const HOME = process.env.HOME!;
export const DISPLAY = process.env.VGATE_DISPLAY ?? ":21";
export const SESSION = process.env.VGATE_SESSION ?? "tbvgate";
export const FLEET = process.env.VGATE_FLEET ?? "/tmp/vgate-fleet";
export const HERDR_BIN = process.env.VGATE_HERDR ?? `${HOME}/.local/bin/herdrtest`;
export const XDG = `${HOME}/.herdr-test`;
export const SOCKET = `${XDG}/config/herdr/sessions/${SESSION}/herdr.sock`;
export const W = 1920, H = 1080;

/**
 * The fixture tree. Depth 3 with brothers at two levels, which is the minimum
 * shape that can falsify every rule: a selection two levels down still has a
 * root and an intermediate ancestor above it (C6/C7), a brother beside it (C5),
 * and children of its own to hang underneath (C3).
 *
 *   vg-root
 *   ├── vg-alpha        brothers: vg-beta
 *   │   ├── vg-a1       brothers: vg-a2
 *   │   └── vg-a2
 *   └── vg-beta
 *       └── vg-b1
 */
export const FIXTURES: { name: string; team?: string }[] = [
  { name: "vg-root" },
  { name: "vg-alpha", team: "vg-root" },
  { name: "vg-beta", team: "vg-root" },
  { name: "vg-a1", team: "vg-alpha" },
  { name: "vg-a2", team: "vg-alpha" },
  { name: "vg-b1", team: "vg-beta" },
];

/** herdr, pointed at the isolated session. */
export function herdr(...args: string[]) {
  const r = spawnSync(HERDR_BIN, args, {
    encoding: "utf8",
    env: {
      ...process.env,
      // BOTH: HERDR_CLIENT_SOCKET_PATH is inherited from whatever session this
      // script runs inside and WINS over HERDR_SOCKET_PATH, so setting only the
      // latter silently drives the wrong server (harness-panel e2e learned this).
      HERDR_SOCKET_PATH: SOCKET,
      HERDR_CLIENT_SOCKET_PATH: SOCKET,
      HERDR_ENV: "",
      DISPLAY,
    },
  });
  return (r.stdout ?? "").trim();
}

export function api(...args: string[]): any {
  const out = herdr(...args);
  try { return JSON.parse(out); } catch { return null; }
}

export function snapshot(): any {
  return api("api", "snapshot")?.result?.snapshot ?? null;
}

/** The collector's own emitted snapshot — the bar's claim about itself. */
export function barInput(): any {
  const p = `${XDG}/state/herdr/plugins/builtin.tidus-bar/tidus_bar_input.json`;
  try { return JSON.parse(require("node:fs").readFileSync(p, "utf8")); } catch { return null; }
}

export const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Build the fixture fleet root. */
export function seedFleet() {
  rmSync(FLEET, { recursive: true, force: true });
  for (const f of FIXTURES) {
    const dir = `${FLEET}/${f.name}`;
    mkdirSync(`${dir}/state`, { recursive: true });
    mkdirSync(`${dir}/workspace`, { recursive: true });
    writeFileSync(
      `${dir}/harness.json`,
      JSON.stringify(
        {
          name: f.name,
          runtime: "claude",
          template: "default",
          ...(f.team ? { team: f.team } : {}),
          created: new Date().toISOString(),
        },
        null,
        1,
      ),
    );
    // An idle marker keeps the dot deterministic; without it the status flips
    // between `unknown` and `stopped` between ticks and the video shows a chip
    // whose glyph changes for no scenario reason.
    writeFileSync(`${dir}/state/idle`, "");
  }
  console.log(`fleet seeded: ${FLEET} (${FIXTURES.length} instances)`);
}

/**
 * Bring up Xvfb + a terminal running herdr in the isolated session.
 *
 * env -u HERDR_ENV is load-bearing: herdr refuses to nest ("recursive descent
 * denied") when HERDR_ENV=1 is inherited, which it always is when this runs
 * from inside a herdr pane. Clearing the session vars is what makes an isolated
 * stage possible from within a session at all.
 */
export function bringUpStage() {
  // Stop any surviving server FIRST. `herdr --session X` attaches to a running
  // server rather than starting one, and the collector inherits the SERVER's
  // environment — so a server left over from a previous run keeps its old
  // HARNESS_ROOT and the bar renders the live fleet instead of the fixtures.
  // Observed exactly that on the first bring-up: six fixture panes, and a bar
  // still showing ~/harnesses.
  herdr("server", "stop");
  spawnSync("pkill", ["-f", `[x]fce4-terminal.*${SESSION}`]);
  spawnSync("pkill", ["-f", `[X]vfb ${DISPLAY} `]);
  spawnSync("sleep", ["2"]);
  // Delete the session state, not just the processes. herdr persists tabs, so a
  // re-run otherwise ADDS six more vg-* panes beside the previous six — two
  // panes labelled `vg-root`, and then a click that lands on the older one
  // looks exactly like the "focused a different pane of the same-named harness"
  // failure C8 exists to catch. The gate must not manufacture its own findings.
  rmSync(`${XDG}/config/herdr/sessions/${SESSION}`, { recursive: true, force: true });
  rmSync(`${XDG}/state/herdr/sessions/${SESSION}`, { recursive: true, force: true });
  spawnSync("bash", ["-c",
    `Xvfb ${DISPLAY} -screen 0 ${W}x${H}x24 -nolisten tcp >/tmp/vgate-xvfb.log 2>&1 & sleep 2`]);
  if (spawnSync("xdpyinfo", [], { env: { ...process.env, DISPLAY } }).status !== 0)
    throw new Error(`Xvfb ${DISPLAY} did not come up`);

  const term = process.env.VGATE_TERM ?? "xfce4-terminal";
  spawnSync("bash", ["-c",
    `env -u HERDR_ENV -u HERDR_SESSION -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH ` +
    `-u HERDR_PANE_ID -u HERDR_TAB_ID -u HERDR_WORKSPACE_ID ` +
    `XDG_CONFIG_HOME=${XDG}/config XDG_STATE_HOME=${XDG}/state ` +
    `XDG_DATA_HOME=${XDG}/data XDG_CACHE_HOME=${XDG}/cache ` +
    `HARNESS_ROOT=${FLEET} DISPLAY=${DISPLAY} TERM=xterm-256color ` +
    `setsid ${term} --disable-server --title=${SESSION} --geometry=236x64+0+0 ` +
    `--font="DejaVu Sans Mono 10" --hide-menubar --hide-scrollbar --hide-borders ` +
    `--command="bash -c 'HARNESS_ROOT=${FLEET} ${HERDR_BIN} --session ${SESSION}; sleep 7200'" ` +
    `>/tmp/vgate-term.log 2>&1 & sleep 4`]);

  const wid = spawnSync("xdotool", ["search", "--name", SESSION],
    { encoding: "utf8", env: { ...process.env, DISPLAY } }).stdout.trim().split("\n")[0];
  if (!wid) throw new Error("no terminal window on the stage");
  spawnSync("xdotool", ["windowsize", wid, String(W), String(H), "windowmove", wid, "0", "0"],
    { env: { ...process.env, DISPLAY } });
  console.log(`stage up: ${DISPLAY} session=${SESSION} wid=${wid}`);
  return wid;
}

/**
 * One real pane per fixture instance, each carrying HARNESS_NAME.
 *
 * HARNESS_NAME in the pane process's environment is the collector's STRONGEST
 * attribution rule, so this is what makes a pane genuinely belong to an
 * instance rather than being labelled as one. It is also what gives every chip
 * a tab_id — a team edge with no pane renders nothing at all
 * (chip_is_worth_a_slot), so without these panes the bar would be empty and
 * every click criterion would be vacuous.
 */
export function openInstancePanes(): Record<string, { tab: string; pane: string }> {
  const map: Record<string, { tab: string; pane: string }> = {};
  for (const f of FIXTURES) {
    herdr("tab", "create", "--label", f.name, "--no-focus",
      "--cwd", `${FLEET}/${f.name}/workspace`,
      "--env", `HARNESS_NAME=${f.name}`,
      "--env", `HARNESS_ROOT=${FLEET}`);
  }
  const snap = snapshot();
  for (const t of snap?.tabs ?? []) {
    const name = t.label ?? t.title ?? "";
    if (!FIXTURES.some((f) => f.name === name)) continue;
    const pane = (snap.panes ?? []).find((p: any) => p.tab_id === t.tab_id);
    map[name] = { tab: t.tab_id, pane: pane?.pane_id ?? "" };
  }
  console.log("instance panes:", JSON.stringify(map));
  return map;
}

export function tearDown() {
  spawnSync("pkill", ["-f", `[x]fce4-terminal.*${SESSION}`]);
  spawnSync("pkill", ["-f", `[X]vfb ${DISPLAY} `]);
}

if (import.meta.main) {
  const cmd = process.argv[2] ?? "up";
  if (cmd === "down") { tearDown(); console.log("stage down"); }
  else {
    seedFleet();
    bringUpStage();
    await sleep(9000);
    if (!existsSync(SOCKET)) throw new Error(`no herdr socket at ${SOCKET}`);
    openInstancePanes();
    await sleep(3000);
    console.log("bar snapshot:", JSON.stringify(barInput())?.slice(0, 400));
  }
}
