#!/bin/bash
# verify-instance.sh — prove a herdr instance is ACTUALLY GOOD, then write a
# sha256-bound receipt that authorizes promoting it onward.
#
# WHAT "VERIFIED" HAS TO MEAN
# ---------------------------
# The failure this exists to prevent: a bridge-less upstream 0.7.5 was installed,
# answered `--version` perfectly, and looked healthy. Every weaker definition of
# "verified" would have blessed it:
#
#   "the file changed"          → blesses it
#   "it answers --version"      → blesses it   (this was the old gate)
#   "the binary has the markers"→ catches THAT one, but not a fork build that
#                                 boots and then fails to load its plugins
#
# So verification here is a claim about the RUNNING PROCESS, not about a file:
# the instance is relaunched onto the new binary, its server socket comes up, the
# process actually executing is proven to be the binary we installed (sha256, not
# path), that running executable passes the bridge gate, and its plugin registry
# reports ui_modules still loaded. Only then is a receipt written.
#
# The receipt is bound to the binary's sha256. It expires by construction: swap
# the binary by ANY route — this pipeline, a hand `mv`, a stray `cargo` relink —
# and the receipt stops matching, so the instance silently drops out of
# `Verified` rather than carrying a stale blessing forward.
#
# NEVER RUNS AGAINST hr
# ---------------------
# Verification relaunches the instance. `hr` is the multiplexer hosting the
# user's session and every agent in it, so relaunching it kills the process tree
# that would report the result. This script REFUSES `--label hr` with a relaunch,
# unconditionally and with no override flag. hr's half of the pipeline is a
# static check plus a consent point (see update-hr.sh).
#
# USAGE
#   verify-instance.sh --label test [--bin PATH] [--root DIR] [--session NAME]
#                      [--tmux NAME] [--no-relaunch] [--json]
#
# EXIT CODES
#    0  verified — receipt written with ok=true
#   10  usage / internal error
#   40  a verification check FAILED — receipt written with ok=false
#   41  refused: would have had to restart a protected instance
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BRIDGE_CHECK="$SCRIPT_DIR/herdr-bridge-check.sh"
REGISTRY_GUARD="$SCRIPT_DIR/herdr-registry-guard.sh"

LABEL="test"
BIN=""
ROOT=""
SESSION=""
TMUX_HOST=""
RELAUNCH=1
JSON=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --instance)    INSTANCE="$2"; shift 2 ;;
    --label)       LABEL="$2"; shift 2 ;;
    --bin)         BIN="$2"; shift 2 ;;
    --root)        ROOT="$2"; shift 2 ;;
    --session)     SESSION="$2"; shift 2 ;;
    --tmux)        TMUX_HOST="$2"; shift 2 ;;
    --no-relaunch) RELAUNCH=0; shift ;;
    --json)        JSON=1; shift ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; exit 10 ;;
  esac
done

# ── the instance must be NAMED, never guessed ──────────────────────────────
# `--instance test` is the only shorthand, and it expands to the sandbox. There
# is no shorthand that expands to hr, and there is no bare default: running this
# script with no instance is an ERROR, not an implicit choice. The incident this
# rule comes from is in .local/prd/hr-plugin-registry-incident.md — a script that
# silently resolved its own default wrote into the live instance twice in one
# day. A default that quietly resolves to hr is a loaded gun; the safe failure is
# to refuse, never to guess.
case "${INSTANCE:-}" in
  test)
    BIN="${BIN:-$HOME/.local/bin/herdrtest}"
    ROOT="${ROOT:-$HOME/.herdr-test}"
    SESSION="${SESSION:-test}"
    TMUX_HOST="${TMUX_HOST:-test-host}"
    ;;
  "") ;;
  *)
    printf '✗ unknown --instance %q. The only shorthand is `test` (the sandbox).\n' "$INSTANCE" >&2
    printf '  Every other instance must be named explicitly with --bin/--root/--session.\n' >&2
    exit 10 ;;
esac
MISSING_ARGS=()
[[ -n "$BIN"     ]] || MISSING_ARGS+=(--bin)
[[ -n "$ROOT"    ]] || MISSING_ARGS+=(--root)
[[ -n "$SESSION" ]] || MISSING_ARGS+=(--session)
if [[ ${#MISSING_ARGS[@]} -gt 0 ]]; then
  printf '✗ refusing to guess which instance to act on. Missing: %s\n' "${MISSING_ARGS[*]}" >&2
  printf '  Pass them explicitly, or use --instance test for the sandbox.\n' >&2
  exit 10
fi
TMUX_HOST="${TMUX_HOST:-$SESSION-host}"

# Where the strip's state lives: the plugin state dir of the instance HOSTING the
# strip, not the instance being verified — receipts must land where the collector
# that renders the strip will read them. Resolution order matches vd-common.sh so
# that a run inside a sandbox writes into that sandbox: an explicit VD_STATE_DIR
# (exported by the pipeline scripts), then HERDR_PLUGIN_STATE_DIR (set by the
# host when it launches a plugin action), then hr as the hand-run default.
# Same rule as vd-common.sh, same reason: NO hr fallback. This exact default is
# what put a sandbox receipt into the live hr state dir on 2026-07-27.
VD_STATE_DIR="${VD_STATE_DIR:-${HERDR_PLUGIN_STATE_DIR:-}}"
if [[ -z "$VD_STATE_DIR" ]]; then
  printf '✗ refusing to guess where to write the receipt. Set VD_STATE_DIR (or run as a\n' >&2
  printf '  plugin action so the host supplies HERDR_PLUGIN_STATE_DIR).\n' >&2
  exit 10
fi
if [[ -f "$VD_STATE_DIR/pipeline.env" ]]; then
  # shellcheck disable=SC1091
  source "$VD_STATE_DIR/pipeline.env"
fi
RECEIPT="$VD_STATE_DIR/verify-$LABEL.json"

log() { printf '» %s\n' "$*" >&2; }

# ── the protected-instance refusal ─────────────────────────────────────────
# Deliberately not a flag, an env var, or a --force. There is no argument that
# makes this script restart hr.
#
# The ROOT test is a PREFIX test, not an equality test. Equality was not enough:
# `--root ~/.herdr-hr/ --session hr2` slipped past an exact-match guard during
# testing and started a second herdr server against hr's own XDG root, writing a
# session directory into the live instance's config. It did not kill hr — the
# socket differed — but it proves the rule: the protected thing is the ROOT
# TREE, not one exact string. Anything under hr's root, or under the official
# instance's config, is off limits for a relaunch.
#
# ── THE FIX FROM THE 2026-07-27 INCIDENT ───────────────────────────────────
# This guard used to live inside `if [[ $RELAUNCH == 1 ]]`. The re-link step did
# not. So `--no-relaunch` — the flag whose entire purpose is "inspect hr
# safely" — walked straight past the guard and re-linked hr's plugin registry,
# twice, wiping four of the user's live plugins with no restart and no consent
# point. The guard was bound to the wrong thing.
#
# It is now bound to the INSTANCE, evaluated once, unconditionally. `PROTECTED`
# gates every mutation in this script regardless of which flags are set. A
# protected instance can still be READ — that is what `--no-relaunch` is for —
# but nothing in this file may write to it, relaunch it, or call a server
# command that mutates it.
canon() { python3 -c 'import os,sys;print(os.path.realpath(sys.argv[1]))' "$1" 2>/dev/null; }
ROOT_REAL="$(canon "$ROOT")"
HR_ROOT_REAL="$(canon "${VD_PROTECTED_ROOT:-$HOME/.herdr-hr}")"
CFG_ROOT_REAL="$(canon "$HOME/.config")"
PROTECTED=0
PROTECTED_WHY=""
if [[ "$LABEL" == "hr" || "$SESSION" == "hr" \
      || "$ROOT_REAL" == "$HR_ROOT_REAL" || "$ROOT_REAL" == "$HR_ROOT_REAL"/* ]]; then
  PROTECTED=1; PROTECTED_WHY="the live hr instance"
elif [[ "$ROOT_REAL" == "$CFG_ROOT_REAL" || "$ROOT_REAL" == "$CFG_ROOT_REAL"/herdr* ]]; then
  PROTECTED=1; PROTECTED_WHY="the official herdr instance"
fi

if [[ $PROTECTED == 1 && $RELAUNCH == 1 ]]; then
  printf '✗ REFUSING to relaunch %s. It hosts this session and every agent in it;\n' "$PROTECTED_WHY" >&2
  printf '  restarting it kills the process tree that would report the result. Restarting hr is the\n' >&2
  printf '  user'"'"'s decision (⌃⌥X), never automation'"'"'s. Use --no-relaunch for a READ-ONLY check.\n' >&2
  exit 41
fi

CHECKS=()          # "name=pass|fail:detail"
FAILED=0
record() {
  local name="$1" status="$2" detail="${3:-}"
  CHECKS+=("$name=$status:$detail")
  if [[ "$status" == "fail" ]]; then
    FAILED=1; log "✗ $name — $detail"
  else
    log "✓ $name${detail:+ — $detail}"
  fi
}

sha_of() { shasum -a 256 "$1" 2>/dev/null | awk '{print $1}'; }

# ── check 1: the installed binary passes the bridge gate ───────────────────
INSTALLED_SHA=""
if [[ ! -x "$BIN" ]]; then
  record binary_present fail "not executable: $BIN"
else
  INSTALLED_SHA="$(sha_of "$BIN")"
  record binary_present pass "$BIN"
  if "$BRIDGE_CHECK" "$BIN" --quiet >/dev/null 2>&1; then
    record binary_bridge pass "fork build, bridge intact"
  else
    rc=$?
    record binary_bridge fail "bridge gate rejected the installed binary (exit $rc)"
  fi
fi

# A binary that fails the static gate must never be launched: booting a
# bridge-less herdr against fork state STRIPS ui_modules from plugins.json, and
# that damage survives rolling the binary back. Stop before the relaunch.
if [[ $FAILED == 1 ]]; then
  log "aborting before relaunch — refusing to boot an unverified binary against fork state"
  RELAUNCH=0
fi

# ── snapshot the registry before we boot anything ──────────────────────────
if [[ -d "$ROOT/config/herdr" ]]; then
  # Snapshotting only READS the registry and copies it aside, so it is allowed on
  # a protected instance — and it is the reason a byte-exact pre-damage baseline
  # existed when this script damaged hr. Keep it unconditional.
  "$REGISTRY_GUARD" snapshot "$ROOT/config" --label "pre-verify-$LABEL" >/dev/null 2>&1 \
    && record registry_snapshot pass "baseline captured" \
    || record registry_snapshot pass "no baseline captured (non-fatal)"
fi

SOCK="$ROOT/config/herdr/sessions/$SESSION/herdr.sock"
OLD_PID=""

# ── check 2: relaunch the sandbox onto the new binary ──────────────────────
if [[ $RELAUNCH == 1 ]]; then
  log "relaunching $LABEL (tmux $TMUX_HOST) onto $BIN"
  # Kill the SERVER by the pid holding the socket, not by binary path. The
  # running server may exec a different path than the one we just installed
  # (that is the whole reason check 4 exists), and a launcher can keep the
  # server alive after its tmux host dies — so pattern-killing the new path
  # silently leaves the old process owning the socket and the "relaunch"
  # becomes a no-op that check 4 then reports as a mismatch.
  # Last line of defence, immediately in front of the only `kill` in this whole
  # pipeline: resolve the socket we are about to kill the owner of, and refuse if
  # it is hr's. The label/root/session guard above already covers this; this
  # catches the paths it cannot reason about (a trailing slash, a symlinked root,
  # a future caller that constructs $ROOT some other way). The guard sits where
  # the damage would happen, not only at the entry point.
  HR_SOCK_REAL="$(python3 -c 'import os,sys;print(os.path.realpath(sys.argv[1]))' \
      "$HOME/.herdr-hr/config/herdr/sessions/hr/herdr.sock" 2>/dev/null)"
  SOCK_REAL="$(python3 -c 'import os,sys;print(os.path.realpath(sys.argv[1]))' "$SOCK" 2>/dev/null)"
  if [[ -n "$SOCK_REAL" && "$SOCK_REAL" == "$HR_SOCK_REAL" ]]; then
    printf '✗ REFUSING: the relaunch target resolves to the live hr socket (%s).\n' "$SOCK_REAL" >&2
    exit 41
  fi
  OLD_PID="$(lsof -t "$SOCK" 2>/dev/null | head -1)"
  tmux kill-session -t "$TMUX_HOST" 2>/dev/null
  if [[ -n "$OLD_PID" ]]; then
    kill "$OLD_PID" 2>/dev/null
    for _ in $(seq 1 20); do kill -0 "$OLD_PID" 2>/dev/null || break; sleep 0.2; done
    kill -0 "$OLD_PID" 2>/dev/null && { kill -9 "$OLD_PID" 2>/dev/null; sleep 0.3; }
  fi
  # Sweep any straggler still holding the socket open.
  for _ in $(seq 1 10); do
    STRAG="$(lsof -t "$SOCK" 2>/dev/null)"
    [[ -z "$STRAG" ]] && break
    kill -9 $STRAG 2>/dev/null; sleep 0.2
  done
  sleep 0.5
  # ~/.local/bin/herdrtest is a RAW binary, not an env wrapper: launching it bare
  # inherits the caller's env (we run as a child of an hr plugin action) and it
  # would come up against the WRONG XDG root. Scrub every inherited HERDR_* and
  # pin the sandbox roots explicitly.
  tmux new-session -d -s "$TMUX_HOST" -x 220 -y 55 \
    "env -u HERDR_ENV -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_SESSION \
         -u HERDR_WORKSPACE_ID -u HERDR_TAB_ID -u HERDR_PANE_ID \
     XDG_CONFIG_HOME=$ROOT/config XDG_STATE_HOME=$ROOT/state XDG_DATA_HOME=$ROOT/data \
     HERDR_SESSION=$SESSION $BIN --session $SESSION" 2>/dev/null
  record relaunch pass "tmux $TMUX_HOST"
fi

# ── check 3: the server socket actually comes up ───────────────────────────
UP=0
if [[ $RELAUNCH == 1 || -S "$SOCK" ]]; then
  for _ in $(seq 1 60); do   # up to ~30 s for a cold boot
    if [[ -S "$SOCK" ]] && lsof -t "$SOCK" >/dev/null 2>&1; then UP=1; break; fi
    sleep 0.5
  done
fi
if [[ $UP == 1 ]]; then
  record socket_up pass "$SOCK"
else
  record socket_up fail "server did not come up on $SOCK within 30s"
fi

# ── check 4: the RUNNING process is the binary we installed ────────────────
# Path equality is not enough — a launcher loop can exec a different path, and a
# path can be swapped under a running process. Compare content hashes.
RUNNING_EXE=""
if [[ $UP == 1 ]]; then
  PID="$(lsof -t "$SOCK" 2>/dev/null | head -1)"
  RUNNING_EXE="$(ps -p "$PID" -o comm= 2>/dev/null | sed 's/^ *//;s/ server$//')"
  if [[ -n "$RUNNING_EXE" && -f "$RUNNING_EXE" ]]; then
    RUN_SHA="$(sha_of "$RUNNING_EXE")"
    if [[ -n "$INSTALLED_SHA" && "$RUN_SHA" == "$INSTALLED_SHA" ]]; then
      record running_exe pass "pid $PID running the installed build"
    else
      record running_exe fail "pid $PID is running a DIFFERENT build ($RUNNING_EXE)"
    fi
    # ── check 5: the bridge is present in the executable actually running ──
    if "$BRIDGE_CHECK" "$RUNNING_EXE" --quiet >/dev/null 2>&1; then
      record running_bridge pass "live process carries the wasm bridge"
    else
      record running_bridge fail "the RUNNING executable has no wasm bridge"
    fi
  else
    record running_exe fail "could not resolve the running executable"
  fi
fi

# ── re-link: heal a registry that moved out from under this instance ───────
#
# Upstream 0.7.5 relocates the plugin registry, so an upgraded instance can come
# up correct-but-empty. `herdr plugin link <path>` re-registers a local plugin
# against whatever registry the RUNNING binary considers authoritative, which
# makes this deterministic and version-agnostic: we do not need to know where the
# registry moved to, only to ask the new binary to register the plugins again.
#
# It runs BEFORE the populated-by-name assertion, inside the pipeline, because a
# re-link that only happens after a human notices the failure is not automation.
# It is idempotent — linking an already-linked plugin is a no-op — so it is safe
# on every run, not just upgrades. It never runs against hr (this whole script
# refuses hr with a relaunch), and the assertion below still has to pass on its
# own merits: re-link is a repair attempt, not a substitute for proof.
#
# ARTIFACT CHECK BEFORE LINKING — learned the hard way. An early version linked
# every manifest in this worktree's plugins/ dir. Most of those plugins have no
# built `.wasm` in THIS worktree (only version-dash does), so re-linking pointed
# the instance's registry at plugin roots with missing artifacts and took the
# live herdrtest from 10 rendered layers down to 6, with four `ui module … .wasm
# file not found` warnings. A repair step that silently downgrades the instance
# it is repairing is worse than no repair step. So: only link a plugin whose
# declared module artifact actually exists on disk at that root. A plugin dir
# that cannot render is not a candidate for the registry.
RELINKED=0
SKIPPED_NO_ARTIFACT=""
# `$PROTECTED == 0` is the load-bearing term. It is FIRST on purpose: the re-link
# is a WRITE to the instance's registry, so it must sit behind the instance
# guard, not behind whichever adjacent flag happened to be nearby. Binding it to
# `$RELAUNCH` is precisely the bug that cost the user four plugins.
if [[ $PROTECTED == 1 ]]; then
  log "skipping re-link — $PROTECTED_WHY is read-only to this script"
fi
if [[ $PROTECTED == 0 && $UP == 1 && -x "$BIN" && "${VD_SKIP_RELINK:-0}" != "1" ]]; then
  PLUGIN_SRC_DIR="${VD_PLUGIN_SRC_DIR:-$(cd "$SCRIPT_DIR/../.." && pwd)}"
  for manifest in "$PLUGIN_SRC_DIR"/*/herdr-plugin.toml; do
    [[ -f "$manifest" ]] || continue
    grep -q '\[\[ui_modules\]\]' "$manifest" 2>/dev/null || continue
    plugin_dir="$(dirname "$manifest")"
    # Every wasm module the manifest declares must be present at this root.
    MISSING_ART="$(python3 - "$manifest" "$plugin_dir" <<'PY' 2>/dev/null
import re, sys
from pathlib import Path
text = Path(sys.argv[1]).read_text(errors="ignore")
root = Path(sys.argv[2])
missing = [m for m in re.findall(r'^\s*module\s*=\s*"([^"]+)"', text, re.M)
           if not (root / m).is_file()]
print(" ".join(missing))
PY
)"
    if [[ -n "${MISSING_ART// }" ]]; then
      SKIPPED_NO_ARTIFACT="$SKIPPED_NO_ARTIFACT $(basename "$plugin_dir")"
      continue
    fi
    if env -u HERDR_ENV -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_SESSION \
         XDG_CONFIG_HOME="$ROOT/config" XDG_STATE_HOME="$ROOT/state" XDG_DATA_HOME="$ROOT/data" \
         HERDR_SESSION="$SESSION" "$BIN" plugin link "$plugin_dir" >/dev/null 2>&1; then
      RELINKED=$((RELINKED+1))
    fi
  done
  [[ $RELINKED -gt 0 ]] && log "re-linked $RELINKED plugin dir(s) into the running registry"
  [[ -n "${SKIPPED_NO_ARTIFACT// }" ]] && \
    log "skipped (no built artifact at this root, left as-is):$SKIPPED_NO_ARTIFACT"
  sleep 1   # let the server settle the registry before we read it back
fi

# ── check 6: the plugin layer is POPULATED BY NAME in the running process ──
#
# This check exists because of a failure mode that every other check waves
# through. Upstream 0.7.5 moves the plugin registry from the fork's PER-SESSION
# `plugins.json` to a GLOBAL registry. A 0.7.5-based fork build therefore ignores
# the old file entirely: the binary launches, the socket comes up, `--version` is
# right, and every wasm bridge symbol is present in the executable — and NOT ONE
# PLUGIN LOADS. It is a correct binary reading the registry from the wrong place.
#
# That is a third, distinct way to lose the plugin layer:
#   audit Finding 2/3  bridge-less binary            → binary_bridge / running_bridge
#   audit Finding 3    plugins.json ui_modules strip → registry_intact
#   0.7.5 registry move (this)                       → THIS CHECK, and only this one
#
# So the assertion has to be by NAME against an expected set, never a count: an
# empty registry and a half-migrated one both produce plausible-looking numbers.
# We ask the LIVE server what it actually loaded and diff it against what this
# instance had before the upgrade.
EXPECTED_IDS=""
ACTUAL_IDS=""
MISSING_IDS=""
UI_MODULE_PLUGINS=0

# Expected set, in precedence order:
#   1. explicit override (sandbox tests, or a deliberate shrink);
#   2. the pre-upgrade registry baseline — "you had these, do you still?";
#   3. the fork checkout's plugin dirs that declare [[ui_modules]].
if [[ -n "${VD_EXPECTED_UI_PLUGINS:-}" ]]; then
  EXPECTED_IDS="$VD_EXPECTED_UI_PLUGINS"
else
  EXPECTED_IDS="$(python3 - "$ROOT/config/herdr/.registry-backups" <<'PY' 2>/dev/null
import json, sys
from pathlib import Path
backups = Path(sys.argv[1])
ids = set()
if backups.is_dir():
    for d in sorted(backups.iterdir(), reverse=True):
        meta = d / "meta.json"
        if not meta.is_file():
            continue
        try:
            m = json.loads(meta.read_text())
        except Exception:
            continue
        found = {p for regs in m.get("registries", {}).values()
                 for p, n in regs.items() if n > 0 and not p.startswith("#")}
        if found:
            ids = found
            break
print(" ".join(sorted(ids)))
PY
)"
fi
if [[ -z "${EXPECTED_IDS// }" && -d "$SCRIPT_DIR/../.." ]]; then
  EXPECTED_IDS="$(python3 - "$SCRIPT_DIR/../.." <<'PY' 2>/dev/null
import re, sys
from pathlib import Path
ids = []
for manifest in sorted(Path(sys.argv[1]).glob("*/herdr-plugin.toml")):
    text = manifest.read_text(errors="ignore")
    if "[[ui_modules]]" not in text:
        continue
    m = re.search(r'^\s*id\s*=\s*"([^"]+)"', text, re.M)
    if m:
        ids.append(m.group(1))
print(" ".join(sorted(set(ids))))
PY
)"
fi

if [[ $UP == 1 && -x "$BIN" ]]; then
  PLUGIN_JSON="$(env -u HERDR_ENV -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_SESSION \
      XDG_CONFIG_HOME="$ROOT/config" XDG_STATE_HOME="$ROOT/state" XDG_DATA_HOME="$ROOT/data" \
      HERDR_SESSION="$SESSION" "$BIN" plugin list --json 2>/dev/null)"
  read -r UI_MODULE_PLUGINS ACTUAL_IDS <<<"$(printf '%s' "$PLUGIN_JSON" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    print("0"); sys.exit(0)
plugins = d.get("result", {}).get("plugins", []) if isinstance(d, dict) else []
loaded = sorted(p.get("plugin_id", "") for p in plugins if p.get("ui_modules"))
print(len(loaded), " ".join(i for i in loaded if i))
' 2>/dev/null || echo "0")"
  UI_MODULE_PLUGINS="${UI_MODULE_PLUGINS:-0}"

  for want in $EXPECTED_IDS; do
    case " $ACTUAL_IDS " in
      *" $want "*) ;;
      *) MISSING_IDS="$MISSING_IDS $want" ;;
    esac
  done

  if [[ "$UI_MODULE_PLUGINS" -eq 0 ]]; then
    record plugins_loaded fail "the running server loaded ZERO plugins with ui_modules — the plugin layer is EMPTY (0.7.5 moved the registry; this instance needs a re-link)"
  elif [[ -n "${MISSING_IDS// }" ]]; then
    record plugins_loaded fail "plugin layer incomplete — missing:${MISSING_IDS}"
  else
    record plugins_loaded pass "$UI_MODULE_PLUGINS plugin(s) loaded by name: $ACTUAL_IDS"
  fi
fi

# ── check 7: the registry survived the boot ────────────────────────────────
# This is the Finding-3 hazard checked directly: if a bridge-less binary had
# booted, plugins.json would come back with ui_modules stripped, and rolling the
# binary back would NOT undo it.
if [[ -d "$ROOT/config/herdr" ]]; then
  if "$REGISTRY_GUARD" verify "$ROOT/config" >/dev/null 2>&1; then
    record registry_intact pass "ui_modules survived the boot"
  else
    rc=$?
    if [[ $rc == 31 ]]; then
      record registry_intact pass "no baseline yet (first run)"
    else
      record registry_intact fail "ui_modules were STRIPPED from plugins.json (exit $rc) — run: herdr-registry-guard.sh restore $ROOT/config"
    fi
  fi
fi

# ── receipt ────────────────────────────────────────────────────────────────
mkdir -p "$VD_STATE_DIR"
OK=true; [[ $FAILED == 1 ]] && OK=false

CHECKS_JSON="$(python3 - "$@" <<'PY' "${CHECKS[@]}"
import json, sys
out = []
for raw in sys.argv[1:]:
    if "=" not in raw:
        continue
    name, rest = raw.split("=", 1)
    status, _, detail = rest.partition(":")
    out.append({"name": name, "ok": status == "pass", "detail": detail})
print(json.dumps(out))
PY
)"

TMP="$RECEIPT.tmp.$$"
# Values cross the shell→python boundary through the ENVIRONMENT, never through
# heredoc interpolation: a bash `false` is not a Python `False`, and a binary
# path containing a quote would otherwise break the literal.
VD_R_LABEL="$LABEL" \
VD_R_OK="$([[ $FAILED == 1 ]] && echo 0 || echo 1)" \
VD_R_BIN="$BIN" \
VD_R_SHA="$INSTALLED_SHA" \
VD_R_RUNNING="$RUNNING_EXE" \
VD_R_NPLUGINS="${UI_MODULE_PLUGINS:-0}" \
VD_R_LOADED="$ACTUAL_IDS" \
VD_R_EXPECTED="$EXPECTED_IDS" \
VD_R_CHECKS="$CHECKS_JSON" \
python3 - >"$TMP" <<'PY'
import json, os, time
print(json.dumps({
    "label": os.environ.get("VD_R_LABEL", ""),
    "ok": os.environ.get("VD_R_OK") == "1",
    "binary": os.environ.get("VD_R_BIN", ""),
    "sha256": os.environ.get("VD_R_SHA", ""),
    "running_exe": os.environ.get("VD_R_RUNNING", ""),
    "verified_unix": int(time.time()),
    "ui_module_plugins": int(os.environ.get("VD_R_NPLUGINS") or 0),
    "plugins_loaded": os.environ.get("VD_R_LOADED", "").split(),
    "plugins_expected": os.environ.get("VD_R_EXPECTED", "").split(),
    "checks": json.loads(os.environ.get("VD_R_CHECKS") or "[]"),
}, indent=2))
PY
mv -f "$TMP" "$RECEIPT"
log "receipt → $RECEIPT (ok=$OK)"

[[ $JSON == 1 ]] && cat "$RECEIPT"

[[ $FAILED == 1 ]] && exit 40
exit 0
