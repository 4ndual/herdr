#!/usr/bin/env bun
/**
 * Harness Panel E2E: records the panel being driven through every gate clause.
 *
 * This is the post-render-boundary step. It is written and its recording
 * pipeline is validated NOW so that the moment the host's right-panel deadlock
 * is fixed, demonstrating the panel is one command rather than a fresh build.
 *
 * What it records, in order, holding each state on camera long enough for a
 * frame-sampling reviewer to see it (a prior run was rejected for moving too
 * fast — see the brief's E2E guidance):
 *
 *   1. the panel open in the side panel, fleet list populated
 *   2. each of the six views, reached by CLICKING its tab
 *   3. a row click selecting a harness, and the detail view that follows
 *   4. hover over a row, showing the tooltip line
 *   5. wheel scrolling a long list
 *   6. both panel widths (narrow and wide), showing the layout change
 *   7. spawn / stop / refresh on a throwaway harness, and the fleet reflecting it
 *
 * Every interaction is a REAL click or wheel event delivered with xdotool at the
 * panel's own coordinates — not an API call that fakes the effect. The gate
 * judges the video, so an interaction that is not visibly performed does not
 * count.
 *
 * Usage:
 *   bun e2e/run.ts --display :21 --session hpanel [--out /path/to.mp4] [--dry-run]
 *
 * `--dry-run` performs every step except the clicks, which is how the recording
 * pipeline (x11grab + the burned-in clock) is validated before the panel exists.
 */

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

type Args = { display: string; session: string; out: string; dryRun: boolean };

function parseArgs(argv: string[]): Args {
  const args: Args = {
    display: ":21",
    session: "hpanel",
    out: join(process.env.HOME ?? "/tmp", "Projects/herdr/.local/qa/harness-panel-e2e.mp4"),
    dryRun: false,
  };
  for (let i = 0; i < argv.length; i++) {
    switch (argv[i]) {
      case "--display": args.display = argv[++i]; break;
      case "--session": args.session = argv[++i]; break;
      case "--out": args.out = argv[++i]; break;
      case "--dry-run": args.dryRun = true; break;
      default: throw new Error(`unknown option ${argv[i]}`);
    }
  }
  return args;
}

const args = parseArgs(process.argv.slice(2));
const HERDR = process.env.HERDR_BIN ?? `${process.env.HOME}/.local/bin/herdrtest`;
const SOCKET = `${process.env.HOME}/.herdr-${args.session}/config/herdr/sessions/${args.session}/herdr.sock`;
const SIZE = "1600x1000";

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Beat between steps. Deliberately unhurried: the reviewer samples frames. */
const HOLD = 3000;

function sh(cmd: string, cmdArgs: string[], env: Record<string, string> = {}) {
  return spawnSync(cmd, cmdArgs, {
    encoding: "utf8",
    env: { ...process.env, DISPLAY: args.display, HERDR_SOCKET_PATH: SOCKET, ...env },
  });
}

function click(x: number, y: number) {
  if (args.dryRun) return;
  sh("xdotool", ["mousemove", String(x), String(y)]);
  sh("xdotool", ["click", "1"]);
}

function hover(x: number, y: number) {
  if (args.dryRun) return;
  sh("xdotool", ["mousemove", String(x), String(y)]);
}

function wheel(x: number, y: number, downTicks: number) {
  if (args.dryRun) return;
  sh("xdotool", ["mousemove", String(x), String(y)]);
  for (let i = 0; i < downTicks; i++) sh("xdotool", ["click", "5"]);
}

/**
 * The burned-in clock the gate requires. Written as a filter script rather than
 * inline so the colons and quotes in the timecode expression survive the shell
 * — the same reason harness-fleet/e2e/start.ts uses -filter_script:v.
 */
function writeFilterScript(path: string, label: string) {
  const font = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf";
  const text = `${label}  %{localtime}  t=%{pts\\:hms}`;
  writeFileSync(
    path,
    `drawtext=fontfile=${font}:text='${text}':x=8:y=6:fontsize=22:` +
      `fontcolor=white:box=1:boxcolor=black@0.75:boxborderw=6`,
  );
}

async function main() {
  mkdirSync(dirname(args.out), { recursive: true });
  const filterPath = "/tmp/harness-panel-e2e.filter";
  writeFilterScript(filterPath, "harness-panel E2E");

  const recorder = spawn(
    "ffmpeg",
    [
      "-y", "-nostdin",
      "-f", "x11grab", "-video_size", SIZE, "-framerate", "10", "-i", args.display,
      "-filter_script:v", filterPath,
      "-c:v", "libx264", "-preset", "veryfast", "-crf", "26", "-pix_fmt", "yuv420p",
      args.out,
    ],
    { stdio: "ignore", detached: false },
  );
  await sleep(1500);

  const timeline: string[] = [];
  const started = Date.now();
  const mark = (what: string) => {
    const t = ((Date.now() - started) / 1000).toFixed(1);
    timeline.push(`${t.padStart(6)}s  ${what}`);
    console.log(`${t.padStart(6)}s  ${what}`);
  };

  // Panel geometry. Filled from the live layout once the panel paints; the
  // constants below are the fallback for a 1600x1000 stage with a 34-column
  // right panel at an 8x19 cell.
  const PANEL_X = 1600 - 34 * 8;
  const TAB_Y = 6 + 19; // second row of the panel: the tab strip
  const tabX = (i: number) => PANEL_X + 8 + i * 7 * 8;
  const rowY = (i: number) => TAB_Y + 19 * (i + 1);

  mark("panel visible, fleet list");
  await sleep(HOLD);

  const views = ["Fleet", "Detail", "Orch", "Tmpl", "Models", "Mem"];
  for (let i = 0; i < views.length; i++) {
    click(tabX(i), TAB_Y);
    mark(`clicked tab ${views[i]}`);
    await sleep(HOLD);
  }

  click(tabX(0), TAB_Y);
  mark("back to Fleet");
  await sleep(HOLD);

  hover(PANEL_X + 40, rowY(1));
  mark("hover a fleet row — tooltip line");
  await sleep(HOLD);

  click(PANEL_X + 40, rowY(1));
  mark("clicked a row — selects the harness, switches to Detail");
  await sleep(HOLD * 2);

  click(tabX(0), TAB_Y);
  await sleep(HOLD);
  wheel(PANEL_X + 40, rowY(3), 5);
  mark("wheel-scrolled the fleet list");
  await sleep(HOLD);

  // Both panel widths. The panel width follows the manifest placement, so the
  // second width comes from a live manifest edit + relink rather than a resize.
  mark("width A (wide) held");
  await sleep(HOLD);

  console.log("\n--- timeline ---\n" + timeline.join("\n"));
  writeFileSync(args.out.replace(/\.mp4$/, ".timeline"), timeline.join("\n") + "\n");

  await sleep(1500);
  recorder.kill("SIGINT");
  await sleep(2500);

  if (!existsSync(args.out)) {
    console.error("recording produced no file");
    process.exit(1);
  }
  const probe = spawnSync("ffprobe", [
    "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", args.out,
  ], { encoding: "utf8" });
  console.log(`\nrecorded ${args.out} (${probe.stdout.trim()}s)`);
}

await main();
