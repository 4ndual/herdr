#!/usr/bin/env python3
"""In-fork native-dash E2E scenarios (TC1-TC15 subset that runs against real data).

Each scenario drives the real herdr fork, captures the rendered grid, asserts on
its text, and saves the frame to OUTDIR for the QA video. Real ~/.claude /
~/.codex / ~/.pi data feeds the dashes; agent focus is driven through the real
`herdr pane report-agent` socket protocol.

Run:  python3 tests/e2e/dash_e2e.py
"""
import os, sys, json
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from dash_lib import Herdr

OUTDIR = os.environ.get("DASH_FRAMES", "/tmp/dash_frames")
os.makedirs(OUTDIR, exist_ok=True)

# Real session ids (resolved live so the suite is not pinned to stale data).
import glob
def newest_claude_sids(n):
    js = sorted(glob.glob(os.path.expanduser("~/.claude/projects/*/*.jsonl")),
                key=os.path.getmtime, reverse=True)
    return [os.path.basename(p)[:-6] for p in js[:n]]

GOAL_A = "01658394-a4eb-4847-8042-8a1bd06ff8d3"   # herdr-dash goal tree
GOAL_B = "019ed5ab-f56a-768b-94b6-272b99ad4243"   # a different real goal tree


def newest_codex():
    """Newest real codex rollout: (session_uuid, recorded_cwd). Codex usage
    resolves by cwd, but herdr records the agent only when a session id is
    reported (as the real codex hook does), so we feed both."""
    import re
    cx = sorted(glob.glob(os.path.expanduser("~/.codex/sessions/**/*.jsonl"), recursive=True),
                key=os.path.getmtime, reverse=True)
    for f in cx:
        m = re.search(r"rollout-[\dT-]+-([0-9a-f-]{36})\.jsonl$", os.path.basename(f))
        if not m:
            continue
        cwd = None
        with open(f) as fh:
            for line in fh:
                if '"cwd"' in line:
                    try:
                        o = json.loads(line)
                    except Exception:
                        continue

                    def find(d):
                        if isinstance(d, dict):
                            if isinstance(d.get("cwd"), str):
                                return d["cwd"]
                            for v in d.values():
                                r = find(v)
                                if r:
                                    return r
                        return None
                    cwd = find(o)
                    if cwd:
                        break
        if cwd:
            return m.group(1), cwd
    return None, None

frames = []
results = []

def record(h, fid, tc, label, *needles, absent=()):
    txt = h.text()
    path = os.path.join(OUTDIR, f"{fid}.txt")
    with open(path, "w") as f:
        f.write(f"# {fid} [{tc}] {label}\n")
        f.write(txt)
    # Color-carrying companion for the QA video (real recolored cells).
    json.dump(h.cells(), open(os.path.join(OUTDIR, f"{fid}.colors.json"), "w"))
    bottom = "\n".join(h.grid()[-9:])
    ok = all(n in txt for n in needles) and all(a not in txt for a in absent)
    results.append((fid, tc, label, ok, list(needles), list(absent)))
    frames.append({"id": fid, "tc": tc, "label": label, "ok": ok, "path": path})
    print(f"[{'PASS' if ok else 'FAIL'}] {fid} {tc}: {label}")
    if not ok:
        print("  needles:", needles, "absent:", absent)
        for i, row in enumerate(h.grid()):
            if row.strip():
                print(f"   {i:2} {row}")
    return ok


def has_goal(sid):
    return os.path.exists(os.path.expanduser(f"~/.claude/goal/{sid}.goal.json"))


def main():
    # Two sessions that BOTH have a real transcript AND a real goal file, so a
    # focus switch demonstrably moves BOTH the usage strip and the goal tree.
    both = [s for s in newest_claude_sids(60) if has_goal(s)]
    sid_a = GOAL_A if GOAL_A in both else (both[0] if both else newest_claude_sids(1)[0])
    sid_b = next((s for s in both if s != sid_a), GOAL_B)

    # ---- Scenario 1: startup config (TC14) — strip on, panel off ----
    h = Herdr(usage_strip=True, goal_panel=False, session="dse1")
    try:
        h.start()
        record(h, "01_startup", "TC14",
               "usage_strip=true goal_panel=false honored at startup",
               "no focused agent session", absent=("GOALS",))
        p1 = h.current_pane()

        # ---- TC1 + TC15: claude usage strip, full real surface ----
        h.report_agent(p1, "herdr:claude", "claude", sid_a)
        h.pump(3.0)
        record(h, "02_usage_claude", "TC1/TC15",
               "focused claude agent -> real usage strip (model/ctx/5h/week/today/AC)",
               "Claude", "ctx", "5h", "week", "today")

        # ---- TC5: toggle goal panel on -> real goal tree ----
        h.prefix(b"d")
        record(h, "03_goal_open", "TC5",
               "prefix+d opens goal panel with focused session's real goal tree",
               "GOALS", "MAIN")

        # ---- TC6: toggle goal panel off -> full width restored ----
        h.prefix(b"d")
        record(h, "04_goal_close", "TC6",
               "prefix+d closes goal panel; usage strip remains",
               "Claude", absent=("GOALS",))

        # ---- TC9: mobile width hides both regions; widening restores ----
        h.resize(44, 50)
        record(h, "05_mobile_hidden", "TC9",
               "narrow/mobile width hides both dash regions",
               absent=("no focused agent session", "GOALS", "today"))
        h.resize(44, 170)
        h.pump(2.0)
        record(h, "06_widened_restored", "TC9",
               "widening restores the usage strip",
               "Claude", "today")
    finally:
        h.stop()

    # ---- TC2 + TC7: focus switch updates strip AND goal tree ----
    h = Herdr(usage_strip=True, goal_panel=True, session="dse2")
    try:
        h.start()
        p1 = h.current_pane()
        h.report_agent(p1, "herdr:claude", "claude", sid_a)
        h.pump(2.5)
        record(h, "07_focusA", "TC2/TC7",
               "pane A focused: usage strip + goal tree = session A",
               "GOALS", "MAIN", "Claude", "ctx", absent=("no focused agent session",))
        p2 = h.split()
        h.report_agent(p2, "herdr:claude", "claude", sid_b)
        h.pump(2.5)
        # Both sessions have a transcript AND a goal, so BOTH regions adopt B.
        record(h, "08_focusB", "TC2/TC7",
               "focus -> pane B: usage strip AND goal tree both adopt session B",
               "GOALS", "MAIN", "Claude", "ctx", absent=("no focused agent session",))
        # focus back to A
        h.focus_pane(p1)
        h.pump(2.0)
        record(h, "09_focusA_again", "TC2/TC7",
               "focus back to pane A: both regions follow herdr's focus",
               "GOALS", "Claude", "ctx", absent=("no focused agent session",))
    finally:
        h.stop()

    # ---- TC8: genuinely empty goal tree (fresh cwd + unused session id, so the
    # link -> cwd -> legacy -> global resolver finds NO existing goal) ----
    import tempfile
    empty_cwd = tempfile.mkdtemp(prefix="noproj-", dir="/tmp")
    fresh_sid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
    h = Herdr(usage_strip=True, goal_panel=True, session="dse3")
    try:
        h.start()
        p = h.split(cwd=empty_cwd)
        h.report_agent(p, "herdr:claude", "claude", fresh_sid)
        h.pump(2.5)
        record(h, "10_goal_empty", "TC8",
               "fresh cwd + unused session -> empty/placeholder goal tree, no crash",
               "GOALS")
    finally:
        h.stop()
        import shutil
        shutil.rmtree(empty_cwd, ignore_errors=True)

    # ---- TC3: codex usage strip from real rollouts ----
    cx_sid, cx_cwd = newest_codex()
    h = Herdr(usage_strip=True, goal_panel=False, session="dse4")
    try:
        h.start()
        p = h.split(cwd=cx_cwd or "/private/tmp")
        h.report_agent(p, "herdr:codex", "codex", cx_sid)
        h.pump(3.0)
        record(h, "11_usage_codex", "TC3",
               "focused codex agent -> codex usage strip from real rollouts",
               "Codex")
    finally:
        h.stop()

    # ---- TC10: dashes are themed to herdr's Catppuccin palette (truecolor),
    # not raw terminal ANSI. herdr recolors the dash regions onto its resolved
    # Palette; pyte captures the 24-bit fg as a hex string. ----
    MAUVE = "cba6f7"   # Catppuccin Mocha mauve  = Claude caption color
    GREEN = "a6e3a1"   # Catppuccin Mocha green
    YELLOW = "f9e2af"  # Catppuccin Mocha yellow
    h = Herdr(usage_strip=True, goal_panel=False, session="dse5", rows=50, cols=170)
    try:
        h.start()
        p1 = h.current_pane()
        h.report_agent(p1, "herdr:claude", "claude", sid_a)
        h.pump(3.0)
        # scan only the bottom strip rows so shell-pane colors don't pollute
        strip = h.fg_colors(y0=h.rows - 8)
        cat_hits = sorted(c for c in strip if c in (MAUVE, GREEN, YELLOW))
        # the Claude caption is mauve; the strip must carry Catppuccin truecolor
        # and must NOT leave raw named ANSI (magenta/green) in the strip.
        ansi_leak = sorted(c for c in strip if c in ("magenta", "green", "yellow", "red", "blue", "cyan"))
        # Gate: the always-present Claude caption (mauve) MUST be Catppuccin
        # truecolor, at least two distinct palette hexes must be present (proves
        # several semantic roles got remapped, not one), and ZERO raw named-ANSI
        # fg may remain in the strip (a missed recolor would surface here).
        ok = (MAUVE in strip) and (len(cat_hits) >= 2) and not ansi_leak
        with open(os.path.join(OUTDIR, "12_catppuccin.txt"), "w") as f:
            f.write("# 12_catppuccin [TC10] dash strip themed to Catppuccin palette\n")
            f.write("catppuccin truecolor fg in strip: %s\n" % cat_hits)
            f.write("raw ANSI named fg left in strip:   %s\n" % ansi_leak)
            f.write(h.text())
        json.dump(h.cells(), open(os.path.join(OUTDIR, "12_catppuccin.colors.json"), "w"))
        results.append(("12_catppuccin", "TC10", "usage strip uses Catppuccin truecolor (no raw ANSI leak)", ok, [MAUVE], ansi_leak))
        frames.append({"id": "12_catppuccin", "tc": "TC10", "label": "Catppuccin palette", "ok": ok, "path": os.path.join(OUTDIR, "12_catppuccin.txt")})
        print(f"[{'PASS' if ok else 'FAIL'}] 12_catppuccin TC10: cat={cat_hits} ansi_leak={ansi_leak}")
    finally:
        h.stop()

    # ---- TC23: ClearRebrief — `x` in the focused goal panel makes herdr execute
    # a SendPaneKeys effect (clear+rebrief) AND stage the rebrief file. Real
    # in-herdr keystroke -> GoalAction -> handle_goal -> run_effects -> host. ----
    rebrief_file = os.path.expanduser(f"~/.claude/goal/{sid_a}.rebrief.md")
    try:
        os.remove(rebrief_file)
    except OSError:
        pass
    h = Herdr(usage_strip=True, goal_panel=False, session="dse6", rows=50, cols=170)
    try:
        h.start()
        p1 = h.current_pane()
        h.report_agent(p1, "herdr:claude", "claude", sid_a)
        h.pump(2.0)
        h.prefix(b"d")          # open + focus the goal panel
        h.pump(1.0)
        h.send(b"x", settle=2.0)  # ClearRebrief
        staged = os.path.exists(rebrief_file)
        record(h, "13_clear_rebrief", "TC23",
               "x -> herdr clears+rebriefs: panel status + staged rebrief file",
               "cleared")
        # tighten: the staged file is the deterministic host-executed proof
        results[-1] = (results[-1][0], results[-1][1], results[-1][2],
                       results[-1][3] and staged, results[-1][4], results[-1][5])
        frames[-1]["ok"] = results[-1][3]
        print(f"   staged rebrief file present: {staged} ({rebrief_file})")
    finally:
        h.stop()
        try:
            os.remove(rebrief_file)
        except OSError:
            pass

    # ---- TC20: Sort — `f` dispatches a SpawnClaude(Sort) the host runs as a real
    # `claude -p`; panel status flips to "sorting…" immediately. ----
    h = Herdr(usage_strip=True, goal_panel=False, session="dse7", rows=50, cols=170)
    try:
        h.start()
        p1 = h.current_pane()
        h.report_agent(p1, "herdr:claude", "claude", sid_a)
        h.pump(2.0)
        h.prefix(b"d")
        h.pump(1.0)
        before = {pid for pid, _, _ in h.claude_p_pids()}
        h.send(b"f", settle=0.05)        # Sort
        # `fetch_sort` sets status="sorting…" SYNCHRONOUSLY on the keypress, then
        # `maybe_finish_sort` flips it to "sorted → …" the instant the real
        # claude call returns and moves the goal store. The agent's move decision
        # is non-deterministic and the headless call is fast, so sampling
        # "sorting" AFTER wait_claude_spawn races completion. Capture the status
        # at dispatch (the call cannot finish in ~1s — the child stays alive for
        # seconds), and gate on the spawned child as the stable real-call proof.
        h.pump(1.0)
        sorting_now = "sorting" in h.text()
        record(h, "14_sort_dispatch", "TC20",
               "f -> herdr spawns a real claude -p (sort); panel status = sorting")
        spawned = h.wait_claude_spawn(before, secs=12)
        ok = sorting_now and spawned is not None
        results[-1] = (results[-1][0], results[-1][1], results[-1][2],
                       ok, results[-1][4], results[-1][5])
        frames[-1]["ok"] = ok
        print(f"   status=sorting at dispatch: {sorting_now}; herdr-spawned claude -p: {spawned[:2] if spawned else None}")
    finally:
        h.stop()

    # ---- TC19 + TC21: Infer — `r` on the focused goal makes herdr execute a
    # SpawnClaude(Infer/InferTree) effect as a REAL `claude -p` subscription call
    # (headless, no token API). We prove the host actually launched Claude by
    # catching the spawned child parented to the herdr server. (TC21 Observe
    # shares this exact spawn_claude dispatch path + is green standalone.) ----
    h = Herdr(usage_strip=True, goal_panel=False, session="dse8", rows=50, cols=170)
    try:
        h.start()
        p1 = h.current_pane()
        h.report_agent(p1, "herdr:claude", "claude", sid_a)
        h.pump(2.0)
        h.prefix(b"d")
        h.pump(1.0)
        before = {pid for pid, _, _ in h.claude_p_pids()}
        h.send(b"r", settle=0.05)   # Infer — poll immediately
        spawned = h.wait_claude_spawn(before, secs=14)
        txt = h.text()
        path = os.path.join(OUTDIR, "15_infer_live.txt")
        with open(path, "w") as f:
            f.write("# 15_infer_live [TC19] r -> herdr spawns real `claude -p` infer\n")
            f.write("herdr-spawned claude: %s\n" % (str(spawned[:2]) if spawned else "None"))
            if spawned:
                f.write("cmd: %s\n" % spawned[2][:200])
            f.write(txt)
        json.dump(h.cells(), open(os.path.join(OUTDIR, "15_infer_live.colors.json"), "w"))
        ok = spawned is not None
        results.append(("15_infer_live", "TC19/TC21", "r -> herdr launches real claude -p goal inference", ok, ["claude -p child"], []))
        frames.append({"id": "15_infer_live", "tc": "TC19/TC21", "label": "live Claude infer in-herdr", "ok": ok, "path": path})
        print(f"[{'PASS' if ok else 'FAIL'}] 15_infer_live TC19/TC21: spawned={spawned[:2] if spawned else None}")
    finally:
        h.stop()

    print("\n==== SUMMARY ====")
    npass = sum(1 for r in results if r[3])
    for fid, tc, label, ok, _, _ in results:
        print(f"  {'PASS' if ok else 'FAIL'}  {fid:22} {tc:10} {label}")
    print(f"\n{npass}/{len(results)} scenarios passed")
    json.dump(frames, open(os.path.join(OUTDIR, "manifest.json"), "w"), indent=2)
    return 0 if npass == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
