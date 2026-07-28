#!/bin/bash
# herdr-registry-guard.sh — protect an instance's plugin registry from the
# ui_modules-stripping hazard, and undo it when it happens.
#
# WHY A SECOND GUARD
# ------------------
# `herdr-bridge-check.sh` stops a bridge-less binary from being PROMOTED. It is
# not sufficient on its own. Per `.local/prd/fork-update-safety-audit.md`
# Finding 3, the damage is done by a bridge-less binary BOOTING against fork
# state, from any route — a hand-run binary, a stale launcher loop, a tmux
# session someone forgot, `herdr update` slipping past. There is no
# `deny_unknown_fields` anywhere in the registry schema, so:
#
#   * an upstream binary LOADS fork `plugins.json` without error, and
#   * `plugin_registry::save` then rewrites the whole file from ITS struct, which
#     has no `ui_modules` field, so the fork's wasm modules are silently dropped.
#
# The audit's key point is that this **survives rolling the binary back**: you
# restore the fork binary and the panes are still gone, because the state, not
# the binary, was destroyed. A binary-only check cannot cover that. Hence:
# snapshot before every risky step, verify after, restore on demand.
#
# USAGE
#   herdr-registry-guard.sh snapshot <xdg-config-home> [--label TEXT]
#   herdr-registry-guard.sh verify   <xdg-config-home> [--json]
#   herdr-registry-guard.sh restore  <xdg-config-home> [--dry-run]
#   herdr-registry-guard.sh list     <xdg-config-home>
#
#   <xdg-config-home> is the instance's CONFIG root, e.g. ~/.herdr-hr/config or
#   ~/.herdr-test/config. Registries are found at herdr/**/plugins.json beneath it.
#
# EXIT CODES
#    0  ok
#   10  usage / internal error
#   30  verify: at least one registry LOST ui_modules entries vs the baseline
#   31  verify/restore: no usable snapshot baseline exists yet
#   32  restore: nothing to restore (current state already matches baseline)
#
# SAFETY
#   * Never touches ~/.config/herdr (the untouched official instance) — refuses.
#   * Writes are atomic (temp + rename in the same dir), never in place.
#   * `restore` always snapshots the CURRENT (damaged) state first, so a restore
#     is itself reversible.
set -euo pipefail

CMD="${1:-}"
ROOT="${2:-}"
shift 2 2>/dev/null || true
LABEL=""
JSON=0
DRY=0
for arg in "$@"; do
  case "$arg" in
    --label)   LABEL="__NEXT__" ;;
    --json)    JSON=1 ;;
    --dry-run) DRY=1 ;;
    *) if [[ "$LABEL" == "__NEXT__" ]]; then LABEL="$arg"; else
         printf 'unknown argument: %s\n' "$arg" >&2; exit 10; fi ;;
  esac
done
[[ "$LABEL" == "__NEXT__" ]] && LABEL=""

case "$CMD" in
  snapshot|verify|restore|list) ;;
  *) printf 'usage: %s {snapshot|verify|restore|list} <xdg-config-home> [opts]\n' "$0" >&2; exit 10 ;;
esac
[[ -n "$ROOT" ]] || { printf 'missing <xdg-config-home>\n' >&2; exit 10; }

ROOT="${ROOT%/}"
# Refuse the official/prod instance outright. It is the crisis fallback and no
# automation of ours may read or write its state.
if [[ "$ROOT" == "$HOME/.config" || "$ROOT" == "$HOME/.config/herdr" ]]; then
  printf 'REFUSING to operate on the official herdr config root (%s)\n' "$ROOT" >&2
  exit 10
fi
[[ -d "$ROOT/herdr" ]] || { printf 'no herdr config dir under %s\n' "$ROOT" >&2; exit 10; }

BACKUP_DIR="$ROOT/herdr/.registry-backups"
KEEP=20

export HRG_ROOT="$ROOT" HRG_BACKUP_DIR="$BACKUP_DIR" HRG_LABEL="$LABEL" \
       HRG_JSON="$JSON" HRG_DRY="$DRY" HRG_KEEP="$KEEP"

python3 - "$CMD" <<'PY'
import json, os, shutil, sys, time
from pathlib import Path

cmd     = sys.argv[1]
root    = Path(os.environ["HRG_ROOT"])
backups = Path(os.environ["HRG_BACKUP_DIR"])
label   = os.environ.get("HRG_LABEL", "")
as_json = os.environ.get("HRG_JSON") == "1"
dry     = os.environ.get("HRG_DRY") == "1"
keep    = int(os.environ.get("HRG_KEEP", "20"))

def registries():
    """Every plugins.json under the instance's herdr config dir, excluding our
    own backup tree (so snapshots never snapshot snapshots)."""
    out = []
    for p in sorted((root / "herdr").rglob("plugins.json")):
        if backups in p.parents:
            continue
        out.append(p)
    return out

def ui_module_map(path):
    """{plugin_id: ui_module_count} for one registry, or None if unreadable.

    Entries are keyed by whichever id-ish field the schema uses; unknown shapes
    fall back to positional keys so a schema change degrades to a weaker check
    rather than a false 'all clear'."""
    try:
        data = json.loads(path.read_text())
    except Exception:
        return None
    items = data if isinstance(data, list) else data.get("plugins", data)
    if not isinstance(items, list):
        return None
    out = {}
    for i, entry in enumerate(items):
        if not isinstance(entry, dict):
            continue
        manifest = entry.get("manifest")
        key = entry.get("plugin_id") or entry.get("id")
        if not key and isinstance(manifest, dict):
            key = manifest.get("id")
        # Positional fallback keeps the comparison meaningful (weaker, but never
        # a false all-clear) if the registry schema ever drops the id fields.
        if not key:
            key = f"#{i}"
        mods = entry.get("ui_modules")
        if mods is None and isinstance(entry.get("manifest"), dict):
            mods = entry["manifest"].get("ui_modules")
        out[key] = len(mods) if isinstance(mods, list) else 0
    return out

def snapshot_dirs():
    if not backups.is_dir():
        return []
    return sorted([d for d in backups.iterdir() if d.is_dir()], key=lambda d: d.name)

def do_snapshot(reason):
    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    name = f"{stamp}-{reason}" if reason else stamp
    dest = backups / name
    n = 1
    while dest.exists():
        dest = backups / f"{name}.{n}"; n += 1
    total = 0
    meta = {"created_unix": int(time.time()), "label": reason, "registries": {}}
    for reg in registries():
        rel = reg.relative_to(root)
        target = dest / rel
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(reg, target)
        m = ui_module_map(reg)
        meta["registries"][str(rel)] = m if m is not None else {}
        total += 1
    if total == 0:
        return None, 0
    dest.mkdir(parents=True, exist_ok=True)
    (dest / "meta.json").write_text(json.dumps(meta, indent=2))
    # Retention: keep the newest `keep` snapshots.
    for old in snapshot_dirs()[:-keep]:
        shutil.rmtree(old, ignore_errors=True)
    return dest, total

def baseline():
    """Newest snapshot that actually recorded ui_modules somewhere. A snapshot
    taken AFTER the damage would otherwise become a baseline that blesses it."""
    for d in reversed(snapshot_dirs()):
        meta_path = d / "meta.json"
        if not meta_path.is_file():
            continue
        try:
            meta = json.loads(meta_path.read_text())
        except Exception:
            continue
        if any(any(v > 0 for v in regs.values())
               for regs in meta.get("registries", {}).values()):
            return d, meta
    return None, None

def do_verify():
    base_dir, meta = baseline()
    if base_dir is None:
        print("no snapshot baseline with ui_modules yet — run `snapshot` first", file=sys.stderr)
        return 31, {"ok": False, "reason": "no baseline"}
    losses = []
    for rel, expected in meta["registries"].items():
        cur_path = root / rel
        current = ui_module_map(cur_path) if cur_path.is_file() else None
        if current is None:
            losses.append({"registry": rel, "plugin": "*", "was": "readable", "now": "missing/unreadable"})
            continue
        for plugin, was in expected.items():
            now = current.get(plugin, 0)
            if was > 0 and now < was:
                losses.append({"registry": rel, "plugin": plugin, "was": was, "now": now})
    result = {"ok": not losses, "baseline": base_dir.name, "losses": losses}
    if losses:
        print(f"✗ ui_modules LOST vs baseline {base_dir.name}:", file=sys.stderr)
        for l in losses:
            print(f"    {l['registry']}  {l['plugin']}: {l['was']} → {l['now']}", file=sys.stderr)
        print("  a bridge-less herdr almost certainly booted against this instance;", file=sys.stderr)
        print("  rolling the binary back will NOT undo this — run `restore`.", file=sys.stderr)
        return 30, result
    print(f"✓ registries intact vs baseline {base_dir.name}", file=sys.stderr)
    return 0, result

def do_restore():
    base_dir, meta = baseline()
    if base_dir is None:
        print("no snapshot baseline with ui_modules — nothing to restore from", file=sys.stderr)
        return 31
    code, res = do_verify()
    if code == 0:
        print("registries already match the baseline; nothing to restore", file=sys.stderr)
        return 32
    if dry:
        print(f"[dry-run] would restore {len(meta['registries'])} registry file(s) from {base_dir.name}", file=sys.stderr)
        return 0
    # A restore is itself a mutation, so preserve the damaged state first.
    do_snapshot("pre-restore-damaged")
    for rel in meta["registries"]:
        src = base_dir / rel
        dst = root / rel
        if not src.is_file():
            continue
        dst.parent.mkdir(parents=True, exist_ok=True)
        tmp = dst.with_suffix(dst.suffix + ".restore-tmp")
        shutil.copy2(src, tmp)
        os.replace(tmp, dst)          # atomic within the same directory
        print(f"restored {rel}", file=sys.stderr)
    print(f"✓ restored from {base_dir.name}", file=sys.stderr)
    return 0

rc = 0
payload = {}
if cmd == "snapshot":
    dest, n = do_snapshot(label)
    if dest is None:
        print("no plugins.json found to snapshot", file=sys.stderr); rc = 10
    else:
        print(f"✓ snapshot {dest.name}: {n} registry file(s)", file=sys.stderr)
        payload = {"snapshot": dest.name, "files": n}
elif cmd == "verify":
    rc, payload = do_verify()
elif cmd == "restore":
    rc = do_restore()
elif cmd == "list":
    for d in snapshot_dirs():
        meta_path = d / "meta.json"
        tot = 0
        if meta_path.is_file():
            try:
                meta = json.loads(meta_path.read_text())
                tot = sum(sum(r.values()) for r in meta.get("registries", {}).values())
            except Exception:
                pass
        print(f"{d.name}\tui_modules_total={tot}")

if as_json:
    print(json.dumps(payload))
sys.exit(rc)
PY
