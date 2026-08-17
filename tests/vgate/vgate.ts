#!/usr/bin/env bun
/**
 * tidus-bar video quality gate — one command, end to end.
 *
 *   bun tests/vgate/vgate.ts run          stage → record → drive → judge
 *   bun tests/vgate/vgate.ts stage        bring the isolated stage up only
 *   bun tests/vgate/vgate.ts judge <mp4> <claims.jsonl>
 *   bun tests/vgate/vgate.ts down         tear the stage down
 *
 * Exit code 0 = ACCEPT, 1 = REJECT. A REJECT is meant to BLOCK "done": wire it
 * as the last step of the plugin's task and let the non-zero exit stop the
 * work from being called finished.
 *
 * The gate never edits the plugin. It drives, records, and judges — the fixing
 * loop belongs to whoever implements the bar (skills/vl-gate: the reviewer is a
 * reviewer only).
 */
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { DISPLAY, SESSION, bringUpStage, openInstancePanes, seedFleet, sleep, tearDown, SOCKET } from "./stage.ts";
import { OUT, runScenario } from "./scenario.ts";
import { judge } from "./judge.ts";

const VIDEO = process.env.VGATE_VIDEO ?? `${OUT}/tidus-bar-vgate.mp4`;
const CLAIMS = `${OUT}/claims.jsonl`;
const WORK = `${OUT}/judge`;

/**
 * Burned-in watermarks: phase top-left (reloaded from phase.txt every frame) and
 * wall clock top-right. Written as a filter SCRIPT rather than inline because
 * the colons in the timecode expression do not survive shell quoting — the same
 * reason harness-fleet/e2e/start.ts uses -filter_script:v.
 *
 * The clock is the gate's sync ground truth: the reviewer trusts the burned
 * time over any assumed offset between claim timestamps and video seconds.
 */
function writeFilter(path: string, suffix: string) {
  const font = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf";
  writeFileSync(path,
    `drawtext=fontfile=${font}:textfile=${OUT}/phase.txt:reload=1:x=16:y=12:fontsize=26:` +
    `fontcolor=white:box=1:boxcolor=blue@0.55:boxborderw=8,` +
    `drawtext=fontfile=${font}:text='%{localtime\\:%X}':x=w-300:y=12:fontsize=28:` +
    `fontcolor=yellow:box=1:boxcolor=black@0.65:boxborderw=8` + suffix);
}

/**
 * VAAPI when this box has a usable render node, x264 otherwise — the same
 * policy as harness-fleet/e2e/encode-pick.sh, including its `vfx` half.
 *
 * That half is load-bearing and was missed on the first run: selecting
 * h264_vaapi WITHOUT appending `,format=nv12,hwupload` to the filter chain
 * makes ffmpeg exit immediately ("impossible to convert between the formats"),
 * and the gate then judged a 0-byte file and rejected the build for its own
 * recorder. The encoder and the filter suffix are one decision, so they are
 * returned together and can never be applied apart.
 */
function encoderArgs(): { pre: string[]; post: string[]; vfx: string; name: string } {
  const probe = spawnSync("bash", ["-c",
    `[ -e /dev/dri/renderD128 ] && ffmpeg -hide_banner -encoders 2>/dev/null | grep -q h264_vaapi && ` +
    `ffmpeg -v error -vaapi_device /dev/dri/renderD128 -f lavfi -i color=c=black:s=320x240:d=0.1:r=10 ` +
    `-vf format=nv12,hwupload -c:v h264_vaapi -frames:v 1 -f null - >/dev/null 2>&1`]);
  if (probe.status === 0 && !process.env.VGATE_FORCE_X264)
    return { pre: ["-vaapi_device", "/dev/dri/renderD128"],
      post: ["-c:v", "h264_vaapi", "-rc_mode", "CQP", "-qp", "26"],
      vfx: ",format=nv12,hwupload", name: "h264_vaapi" };
  return { pre: [], post: ["-c:v", "libx264", "-preset", "veryfast", "-crf", "24", "-pix_fmt", "yuv420p"],
    vfx: "", name: "libx264" };
}

async function run() {
  mkdirSync(OUT, { recursive: true });
  writeFileSync(`${OUT}/phase.txt`, "vgate: booting");

  seedFleet();
  bringUpStage();
  await sleep(9000);
  if (!existsSync(SOCKET)) throw new Error(`no herdr socket at ${SOCKET} — stage did not come up`);
  const tabs = openInstancePanes();
  const missing = Object.entries(tabs).filter(([, v]) => !v.tab).map(([k]) => k);
  if (Object.keys(tabs).length < 6) throw new Error(`only ${Object.keys(tabs).length}/6 instance panes opened (${missing})`);
  await sleep(3000);

  const filter = `${OUT}/vgate.filter`;
  const enc = encoderArgs();
  writeFilter(filter, enc.vfx);
  console.log(`recorder encoder=${enc.name} → ${VIDEO}`);
  const rec = spawn("ffmpeg", ["-y", "-nostdin", ...enc.pre,
    "-f", "x11grab", "-draw_mouse", "1", "-framerate", "15", "-video_size", "1920x1080", "-i", DISPLAY,
    "-filter_script:v", filter, ...enc.post, "-r", "15", "-fps_mode", "cfr", VIDEO],
    { stdio: ["ignore", "ignore", "pipe"] });
  let recErr = "";
  rec.stderr!.on("data", (d) => { recErr += String(d); });
  await sleep(3500);

  try {
    await runScenario(tabs);
  } finally {
    rec.kill("SIGINT");
    await sleep(3500);
  }
  // A gate that judges an unreadable file rejects the product for its own bug,
  // so the recording is verified as a RECORDING before it is judged as evidence.
  const dur = Number(spawnSync("ffprobe", ["-v", "error", "-show_entries", "format=duration",
    "-of", "csv=p=0", VIDEO], { encoding: "utf8" }).stdout.trim());
  if (!existsSync(VIDEO) || !Number.isFinite(dur) || dur < 10) {
    console.error(recErr.slice(-1500));
    throw new Error(`recorder produced no usable video (duration=${dur || "unreadable"}, encoder=${enc.name})`);
  }
  console.log(`recorded ${VIDEO} (${dur.toFixed(1)}s, ${enc.name})`);

  const { verdict } = await judge(VIDEO, CLAIMS, WORK);
  return verdict;
}

const cmd = process.argv[2] ?? "run";
if (cmd === "down") { tearDown(); console.log("stage down"); }
else if (cmd === "stage") { seedFleet(); bringUpStage(); await sleep(9000); openInstancePanes(); }
else if (cmd === "judge") {
  const v = await judge(process.argv[3]!, process.argv[4] ?? CLAIMS, WORK);
  process.exit(v.verdict === "ACCEPT" ? 0 : 1);
} else {
  const verdict = await run();
  if (!process.env.VGATE_KEEP) tearDown();
  process.exit(verdict === "ACCEPT" ? 0 : 1);
}
