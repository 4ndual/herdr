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


def main():
    nogoal = [s for s in newest_claude_sids(12)
              if not os.path.exists(os.path.expanduser(f"~/.claude/goal/{s}.goal.json"))]
    SID_NOGOAL = nogoal[0] if nogoal else newest_claude_sids(1)[0]

    # ---- Scenario 1: startup config (TC14) — strip on, panel off ----
    h = Herdr(usage_strip=True, goal_panel=False, session="dse1")
    try:
        h.start()
        record(h, "01_startup", "TC14",
               "usage_strip=true goal_panel=false honored at startup",
               "no focused agent session", absent=("GOALS",))
        p1 = h.current_pane()

        # ---- TC1 + TC15: claude usage strip, full real surface ----
        h.report_agent(p1, "herdr:claude", "claude", GOAL_A)
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
        h.report_agent(p1, "herdr:claude", "claude", GOAL_A)
        h.pump(2.5)
        record(h, "07_focusA", "TC2/TC7",
               "pane A focused: goal tree = session A (herdr native dashes goal)",
               "GOALS", "MAIN")
        p2 = h.split()
        h.report_agent(p2, "herdr:claude", "claude", GOAL_B)
        h.pump(2.5)
        record(h, "08_focusB", "TC2/TC7",
               "focus switched to pane B: strip + goal tree adopt session B",
               "GOALS", "MAIN", "Claude")
        # focus back to A
        h.focus_pane(p1)
        h.pump(2.0)
        record(h, "09_focusA_again", "TC2/TC7",
               "focus back to pane A: dashes follow herdr's focus",
               "GOALS", "Claude")
    finally:
        h.stop()

    # ---- TC8: empty/placeholder goal tree for a session with no goal ----
    h = Herdr(usage_strip=True, goal_panel=True, session="dse3")
    try:
        h.start()
        p1 = h.current_pane()
        h.report_agent(p1, "herdr:claude", "claude", SID_NOGOAL)
        h.pump(2.5)
        record(h, "10_goal_empty", "TC8",
               "session with no goal file -> placeholder goal tree (no crash)",
               "GOALS")
    finally:
        h.stop()

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

    print("\n==== SUMMARY ====")
    npass = sum(1 for r in results if r[3])
    for fid, tc, label, ok, _, _ in results:
        print(f"  {'PASS' if ok else 'FAIL'}  {fid:22} {tc:10} {label}")
    print(f"\n{npass}/{len(results)} scenarios passed")
    json.dump(frames, open(os.path.join(OUTDIR, "manifest.json"), "w"), indent=2)
    return 0 if npass == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
