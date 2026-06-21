#!/usr/bin/env python3
"""Probe: is the usage-strip recolor deterministic? Render the claude strip,
then sample fg colors several times with extra pumps to see if a recolored
frame and a raw-ANSI frame alternate (a render-path / ordering race)."""
import os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from dash_lib import Herdr

GOAL_A = "01658394-a4eb-4847-8042-8a1bd06ff8d3"
CAT = {"a6e3a1", "cba6f7", "f9e2af", "89b4fa", "94e2d5", "f38ba8", "cdd6f4", "a6adc8", "6c7086"}
ANSI = {"magenta", "green", "yellow", "red", "blue", "cyan", "brown"}


def sample(h, tag):
    strip = h.fg_colors(y0=h.rows - 8)
    cat = sorted(c for c in strip if c in CAT)
    ansi = sorted(c for c in strip if c in ANSI)
    print(f"  [{tag}] cat={cat} ansi_leak={ansi}")
    return cat, ansi


def main():
    h = Herdr(usage_strip=True, goal_panel=False, session="dth1", rows=50, cols=170)
    h.start()
    p1 = h.current_pane()
    h.report_agent(p1, "herdr:claude", "claude", GOAL_A)
    print("after report_agent:")
    for i in range(8):
        h.pump(0.6)
        sample(h, f"pump{i}")
    # force a few extra repaints via resize to see if a fresh render recolors
    h.resize(50, 168)
    h.pump(1.0); sample(h, "resize-a")
    h.resize(50, 170)
    h.pump(1.0); sample(h, "resize-b")
    # dump where the strip rows actually are + their raw text
    print("--- bottom 8 rows ---")
    for y, row in enumerate(h.grid()[-8:]):
        print(f"   r{h.rows-8+y}: {row[:120]}")
    h.stop()


if __name__ == "__main__":
    main()
