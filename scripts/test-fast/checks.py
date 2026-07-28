#!/usr/bin/env python3
"""Fast contract checks for herdr plugins (stdlib only: tomllib + json).

Subcommands, each exits 0 on pass and non-zero on failure with a one-line reason:

  manifests <plugins_dir>            T1 manifest-parse + enum validity
  placement <plugins_dir> <golden>   T2 placement-contract vs golden snapshot
  placement-write <plugins_dir> <golden>   (re)generate the golden snapshot
  wasm <plugins_dir>                 T3 every manifest `module` exists + valid wasm header
  version-input <json>               T4 version_input.json schema
  usage-line <json-line>             T5 usage collector stdout protocol line

Kept dependency-free on purpose: plugin scope must run with no build.
"""
import json
import os
import sys
import tomllib
from pathlib import Path

# Plugin scope narrows every check to one plugin dir (its basename) so a
# plugin-only run never parses/validates its peers. Empty => all plugins.
PLUGIN_FILTER = os.environ.get("HERDR_TESTFAST_PLUGIN", "")

# Enum surfaces/capabilities the host understands. Kept in sync with the plugin
# manifests + docs/next/api schema; a typo here or in a manifest fails T1 loudly
# rather than silently disabling a surface at runtime.
VALID_SURFACES = {"panel", "usage_strip", "right_panel"}
VALID_CAPS = {
    "layout_read", "focused_session", "toasts", "plugin_actions",
    "mouse_all", "api_read", "api_write",
}
VALID_REGIONS = {"bottom_band", "left", "right", "top", "bottom", "free"}
VALID_ALIGNS = {"start", "center", "end"}


def _manifests(plugins_dir: Path):
    for m in sorted(plugins_dir.glob("*/herdr-plugin.toml")):
        if PLUGIN_FILTER and m.parent.name != PLUGIN_FILTER:
            continue
        with m.open("rb") as fh:
            yield m, tomllib.load(fh)


def fail(msg: str):
    print(f"FAIL: {msg}")
    sys.exit(1)


def cmd_manifests(plugins_dir: Path):
    count = 0
    for path, data in _manifests(plugins_dir):
        rel = path.parent.name
        for key in ("id", "name", "version"):
            if not data.get(key):
                fail(f"{rel}: missing top-level `{key}`")
        for mod in data.get("ui_modules", []):
            mid = mod.get("id", "<?>")
            for key in ("id", "kind", "module"):
                if not mod.get(key):
                    fail(f"{rel}/{mid}: missing ui_module `{key}`")
            if mod["kind"] == "wasm" and not isinstance(mod.get("api_version"), int):
                fail(f"{rel}/{mid}: wasm module needs integer api_version")
            for s in mod.get("surfaces", []):
                if s not in VALID_SURFACES:
                    fail(f"{rel}/{mid}: unknown surface `{s}`")
            for c in mod.get("capabilities", []):
                if c not in VALID_CAPS:
                    fail(f"{rel}/{mid}: unknown capability `{c}` (typo silently disables it)")
            count += 1
    print(f"ok: {count} ui_modules across manifests parse + validate")


def _placement_snapshot(plugins_dir: Path):
    snap = {}
    for path, data in _manifests(plugins_dir):
        for mod in data.get("ui_modules", []):
            pl = mod.get("placement")
            if pl is None:
                continue
            key = f"{data['id']}::{mod['id']}"
            if pl.get("region") not in VALID_REGIONS:
                fail(f"{key}: invalid placement region `{pl.get('region')}`")
            if "align" in pl and pl["align"] not in VALID_ALIGNS:
                fail(f"{key}: invalid placement align `{pl['align']}`")
            snap[key] = {k: pl.get(k) for k in ("region", "align", "size", "z", "min_peer")}
    return snap


def cmd_placement(plugins_dir: Path, golden: Path):
    live = _placement_snapshot(plugins_dir)
    if not golden.exists():
        fail(f"golden {golden} missing; run placement-write to seed it")
    want = json.loads(golden.read_text())
    if PLUGIN_FILTER:
        # Compare only this plugin's placements against the shared golden.
        want = {k: v for k, v in want.items() if k in live}
    if live != want:
        # Diff each key so the version-dash bottom-band kind of regression is obvious.
        for key in sorted(set(live) | set(want)):
            if live.get(key) != want.get(key):
                print(f"  DRIFT {key}: golden={want.get(key)} live={live.get(key)}")
        fail("placement contract drift (see DRIFT lines above)")
    print(f"ok: {len(live)} placements match golden")


def cmd_placement_write(plugins_dir: Path, golden: Path):
    snap = _placement_snapshot(plugins_dir)
    golden.write_text(json.dumps(snap, indent=2, sort_keys=True) + "\n")
    print(f"wrote {golden} ({len(snap)} placements)")


def _is_wasm(p: Path) -> bool:
    try:
        with p.open("rb") as fh:
            head = fh.read(8)
    except OSError:
        return False
    # \0asm magic + version 1 little-endian.
    return head[:4] == b"\x00asm" and head[4:8] == b"\x01\x00\x00\x00"


def cmd_wasm(plugins_dir: Path):
    count = 0
    for path, data in _manifests(plugins_dir):
        root = path.parent
        for mod in data.get("ui_modules", []):
            if mod.get("kind") != "wasm":
                continue
            wasm = root / mod["module"]
            if not wasm.exists():
                fail(f"{root.name}/{mod['id']}: wasm not built at {mod['module']}")
            if not _is_wasm(wasm):
                fail(f"{root.name}/{mod['id']}: {mod['module']} is not a valid wasm module")
            count += 1
    print(f"ok: {count} wasm modules exist + valid header")


def cmd_version_input(js: Path):
    data = json.loads(js.read_text())
    for key in ("instances", "age_label", "stale"):
        if key not in data:
            fail(f"version_input.json missing `{key}`")
    if not isinstance(data["instances"], list) or not data["instances"]:
        fail("version_input.json `instances` must be a non-empty array")
    for inst in data["instances"]:
        for key in ("label", "state"):
            if key not in inst:
                fail(f"version_input instance missing `{key}`")
    print(f"ok: version_input.json valid ({len(data['instances'])} instances)")


def cmd_usage_line(line: str):
    data = json.loads(line)
    if data.get("outcome") not in ("replace", "keep", "clear"):
        fail(f"usage collector emitted bad outcome: {data.get('outcome')!r}")
    if data["outcome"] == "replace" and "snapshot" not in data:
        fail("usage collector `replace` outcome missing snapshot")
    print(f"ok: usage collector protocol line valid (outcome={data['outcome']})")


def main():
    args = sys.argv[1:]
    if not args:
        fail("no subcommand")
    cmd, rest = args[0], args[1:]
    if cmd == "manifests":
        cmd_manifests(Path(rest[0]))
    elif cmd == "placement":
        cmd_placement(Path(rest[0]), Path(rest[1]))
    elif cmd == "placement-write":
        cmd_placement_write(Path(rest[0]), Path(rest[1]))
    elif cmd == "wasm":
        cmd_wasm(Path(rest[0]))
    elif cmd == "version-input":
        cmd_version_input(Path(rest[0]))
    elif cmd == "usage-line":
        cmd_usage_line(rest[0])
    else:
        fail(f"unknown subcommand {cmd}")


if __name__ == "__main__":
    main()
