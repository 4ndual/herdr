#!/usr/bin/env python3
"""Diagnostic: does herdr actually spawn a subprocess when `f` (Sort) is pressed
in the focused goal panel? Captures the daemon's stderr, every NEW process whose
parent chain roots at herdr, and the goal-file mtime around the keypress."""
import os, sys, time, glob, subprocess
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dash_lib
from dash_lib import Herdr, BIN

GOAL_A = "01658394-a4eb-4847-8042-8a1bd06ff8d3"


def proc_table():
    tab = {}
    out = subprocess.run(["ps", "-axo", "pid=,ppid=,command="], capture_output=True, text=True).stdout
    for ln in out.splitlines():
        p = ln.split(None, 2)
        if len(p) < 3:
            continue
        try:
            tab[int(p[0])] = (int(p[1]), p[2])
        except ValueError:
            pass
    return tab


def roots_at(pid, tab, herdr, hops=6):
    cur, n = pid, 0
    while cur and cur > 1 and n < hops:
        pp = tab.get(cur, (None, ""))[0]
        if pp in herdr:
            return True
        cur, n = pp, n + 1
    return False


def main():
    # capture daemon stderr to a file instead of DEVNULL
    h = Herdr(usage_strip=True, goal_panel=False, session="diag1", rows=50, cols=170)
    errf = open("/tmp/diag_herdr_stderr.log", "w")
    h.start()
    p1 = h.current_pane()
    print("focused pane:", p1)
    h.report_agent(p1, "herdr:claude", "claude", GOAL_A)
    h.pump(2.0)
    h.prefix(b"d")
    h.pump(1.0)

    base = proc_table()
    print("--- all herdr-named procs (server argv?) ---")
    for pid, (pp, cmd) in base.items():
        if "herdr" in cmd and "diag_spawn" not in cmd:
            print(f"   pid={pid} ppid={pp} :: {cmd[:160]}")
    # match the herdr binary by path, not by --session (the daemon re-execs)
    herdr = {pid for pid, (_pp, cmd) in base.items() if BIN in cmd}
    print("herdr pids (by binary path):", herdr)
    before = set(base.keys())

    # goal file mtime before
    cand = glob.glob(os.path.expanduser(f"~/.claude/goal/{GOAL_A}.goal.json"))
    g0 = os.path.getmtime(cand[0]) if cand else None
    print("goal file:", cand, "mtime0:", g0)

    print(">>> pressing r (Infer — rewrites goal doc on success)")
    h.send(b"r", settle=0.05)

    seen = set()
    for i in range(150):  # 15s @ 0.1s
        tab = proc_table()
        herdr = {pid for pid, (_pp, cmd) in tab.items() if BIN in cmd}
        for pid, (pp, cmd) in tab.items():
            if pid not in before and pid not in seen and roots_at(pid, tab, herdr):
                seen.add(pid)
                print(f"  [{i*0.1:.1f}s] NEW child pid={pid} ppid={pp} :: {cmd[:140]}")
        time.sleep(0.1)

    cand = glob.glob(os.path.expanduser(f"~/.claude/goal/{GOAL_A}.goal.json"))
    g1 = os.path.getmtime(cand[0]) if cand else None
    print("mtime1:", g1, "changed:", g0 != g1)
    print("panel status line:")
    for row in h.grid():
        if "sort" in row.lower() or "infer" in row.lower():
            print("   ", row)
    h.stop()
    errf.close()
    print("\n--- daemon stderr (tail) ---")
    try:
        with open("/tmp/diag_herdr_stderr.log") as f:
            print(f.read()[-2000:])
    except OSError:
        print("(none)")
    print("total new herdr-rooted children seen:", len(seen))


if __name__ == "__main__":
    main()
