#!/bin/bash
# update-herdrtest.sh — stage the CURRENT fork integration-tip build into the
# herdrtest instance (right-click "test" in the version-dash strip).
#
# "latest" for herdrtest = the fork integration-tip RELEASE build (a
# `cargo build --release` of the current branch). Upstream merges stay manual git
# ops; this only promotes what is already on the branch.
#
# Pipeline (staged promotion):
#   1. build the fork release binary in an ISOLATED CARGO_TARGET_DIR (never the
#      live target/ that hr snapshots from, never a running binary);
#   2. sanity-check the fresh binary answers `--version`;
#   3. stage it NEXT TO the destination (SAME volume — a cross-volume mv would
#      overwrite the running inode and SIGKILL the live process) then atomic-mv
#      over ~/.local/bin/herdrtest;
#   4. restart herdrtest (only when VD_DO_RESTART=1 — see the safety note).
#
# SAFETY: the binary swap is always safe (atomic mv keeps the old inode alive for
# the still-running process). The RESTART step is gated behind VD_DO_RESTART=1
# because herdrtest has no self-heal client loop; without it we swap the binary
# and leave the restart to the human. Live wiring of the restart is gated on
# team-lead ack after sandbox E2E.
#
# Env overrides (all optional — sandbox E2E points these at throwaway clones):
#   HERDR_REPO        fork checkout                (default: this script's git toplevel)
#   VD_TEST_BIN       destination binary            (default ~/.local/bin/herdrtest)
#   VD_TARGET_DIR     isolated cargo target dir     (default ~/Projects/herdr/target-versiondash-build)
#   VD_TEST_TMUX      herdrtest tmux host session   (default test-host)
#   VD_TEST_GEOM      tmux geometry WxH             (default 220x55)
#   VD_TEST_ROOT      herdrtest XDG root            (default ~/.herdr-test)
#   VD_TEST_SESSION   herdr session name            (default test)
#   VD_DO_RESTART     "1" to actually restart       (default off: swap only)
#   ZIG / DEVELOPER_DIR   toolchain (default: zig from PATH, then the pinned path)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Repo root: explicit override wins, else the git toplevel containing this
# script. Never a hardcoded path — the checkout lives wherever the user cloned
# it, and this repo has already burned the `~/Projects` assumption twice
# (dash-common.sh:386, full-restart-herdr.sh:125). Same idiom as the sibling
# install-collector-agent.sh.
if [ -n "${HERDR_REPO:-}" ]; then
  HERDR_REPO="$HERDR_REPO"
else
  HERDR_REPO="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null || true)"
fi
if [ -z "$HERDR_REPO" ] || [ ! -f "$HERDR_REPO/Cargo.toml" ]; then
  HERDR_REPO="$HOME/Projects/herdr"
fi
VD_TEST_BIN="${VD_TEST_BIN:-$HOME/.local/bin/herdrtest}"
VD_TARGET_DIR="${VD_TARGET_DIR:-$HERDR_REPO/target-versiondash-build}"
VD_TEST_TMUX="${VD_TEST_TMUX:-test-host}"
VD_TEST_GEOM="${VD_TEST_GEOM:-220x55}"
VD_TEST_ROOT="${VD_TEST_ROOT:-$HOME/.herdr-test}"
VD_TEST_SESSION="${VD_TEST_SESSION:-test}"
VD_DO_RESTART="${VD_DO_RESTART:-0}"
# Prefer whatever zig is on PATH; the pinned path is the last resort. The
# hardcoded default named one install layout and one version, so on a box where
# zig lives anywhere else (the Windows VM uses a different path entirely) the
# build failed against a dead path. 57ecd926 did this everywhere else.
export ZIG="${ZIG:-$(command -v zig 2>/dev/null || echo "$HOME/.local/zig/0.15.2/zig")}"
# macOS only. Exporting a Darwin toolchain path on Linux is at best inert and at
# worst confusing in a build log that is already hard to read.
if [[ "$(uname -s 2>/dev/null)" == "Darwin" ]]; then
  export DEVELOPER_DIR="${DEVELOPER_DIR:-/Library/Developer/CommandLineTools}"
fi

log() { printf '» %s\n' "$*"; }
die() { printf '✗ %s\n' "$*" >&2; exit 1; }

[[ -d "$HERDR_REPO" ]] || die "fork checkout not found: $HERDR_REPO"

log "building fork integration-tip release (isolated target: $VD_TARGET_DIR)"
CARGO_TARGET_DIR="$VD_TARGET_DIR" \
  cargo build --manifest-path "$HERDR_REPO/Cargo.toml" --release --bin herdr

FRESH="$VD_TARGET_DIR/release/herdr"
[[ -x "$FRESH" ]] || die "build did not produce $FRESH"

log "sanity-checking fresh binary --version"
FRESH_VER="$("$FRESH" --version 2>/dev/null)" || die "fresh binary won't report --version"
log "fresh build: $FRESH_VER"

# Downgrade guard. The default HERDR_REPO is the shared checkout, which routinely
# sits on an older branch than the tip that was last promoted — so "build the
# repo and install it" can quietly REPLACE a newer verified binary with an older
# one, and a downgrade past the fork commits also disables the WASM plugin layer.
# The same rule the hr launcher enforces at snapshot time is enforced here at
# install time, because this path is a user-facing button.
version_ge() { # version_ge A B → true when A >= B
  local IFS='.'
  local -a a=($1) b=($2)
  local i x y
  for i in 0 1 2; do
    x="${a[i]:-0}"; y="${b[i]:-0}"
    x="${x%%[^0-9]*}"; y="${y%%[^0-9]*}"
    if (( 10#${x:-0} > 10#${y:-0} )); then return 0; fi
    if (( 10#${x:-0} < 10#${y:-0} )); then return 1; fi
  done
  return 0
}

if [[ -x "$VD_TEST_BIN" && "${VD_ALLOW_DOWNGRADE:-0}" != "1" ]]; then
  NEW_V="$(printf '%s' "$FRESH_VER" | awk '{print $NF}')"
  CUR_V="$("$VD_TEST_BIN" --version 2>/dev/null | awk '{print $NF}')"
  if [[ -n "$NEW_V" && -n "$CUR_V" ]] && ! version_ge "$NEW_V" "$CUR_V"; then
    die "refusing to install $NEW_V over the installed $CUR_V from $HERDR_REPO.
   That checkout is on an older branch than the binary you already have.
   Point HERDR_REPO at the tree holding the fork tip, or set
   VD_ALLOW_DOWNGRADE=1 if the downgrade is genuinely what you want."
  fi
fi

DEST_DIR="$(dirname "$VD_TEST_BIN")"
mkdir -p "$DEST_DIR"
# Stage on the SAME volume as the destination, then atomic mv. A cp/mv from a
# different volume onto the live path would rewrite the running inode → SIGKILL.
STAGE="$DEST_DIR/.herdrtest.new.$$"
cp "$FRESH" "$STAGE"
chmod +x "$STAGE"
mv -f "$STAGE" "$VD_TEST_BIN"
log "promoted → $VD_TEST_BIN ($("$VD_TEST_BIN" --version 2>/dev/null))"

if [[ "$VD_DO_RESTART" == "1" ]]; then
  log "restarting herdrtest into a fresh detached tmux ($VD_TEST_TMUX)"
  # herdrtest has no self-heal client loop, so the action must relaunch it
  # explicitly (methodology §5) — kill the host session and start a fresh
  # detached one on the new binary. herdr restores the persisted session
  # (panes/tabs) on boot, so attached work reappears.
  GEOM_X="${VD_TEST_GEOM%x*}"
  GEOM_Y="${VD_TEST_GEOM#*x}"
  tmux kill-session -t "$VD_TEST_TMUX" 2>/dev/null || true
  # Sweep any straggler still bound to the old server before relaunch.
  pkill -f "$VD_TEST_BIN server" 2>/dev/null || true
  sleep 0.5
  # ~/.local/bin/herdrtest is a RAW herdr binary, not an env wrapper. Launching
  # it bare inherits the caller's env (e.g. an hr shell) and it would start
  # against the WRONG XDG root or fail to come up. Pin the herdrtest XDG root and
  # session explicitly (same isolation pattern as hsand), scrubbing any inherited
  # HERDR_* nesting vars first.
  tmux new-session -d -s "$VD_TEST_TMUX" -x "$GEOM_X" -y "$GEOM_Y" \
    "env -u HERDR_ENV -u HERDR_SOCKET_PATH -u HERDR_CLIENT_SOCKET_PATH -u HERDR_SESSION \
         -u HERDR_WORKSPACE_ID -u HERDR_TAB_ID -u HERDR_PANE_ID \
     XDG_CONFIG_HOME=$VD_TEST_ROOT/config XDG_STATE_HOME=$VD_TEST_ROOT/state XDG_DATA_HOME=$VD_TEST_ROOT/data \
     HERDR_SESSION=$VD_TEST_SESSION $VD_TEST_BIN --session $VD_TEST_SESSION"
  log "herdrtest relaunched in tmux $VD_TEST_TMUX (${GEOM_X}x${GEOM_Y}, root=$VD_TEST_ROOT, session=$VD_TEST_SESSION)"
else
  log "binary swapped; restart skipped (set VD_DO_RESTART=1 to restart herdrtest)"
fi
log "done"
