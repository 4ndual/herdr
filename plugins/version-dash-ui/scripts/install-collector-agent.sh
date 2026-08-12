#!/bin/bash
# Install (or refresh) the version-dash collector as a per-user scheduled job
# that runs every 10 minutes, writing version_input.json into the hr instance's
# plugin state dir. The herdr host reads that file each frame via the generic
# `ui_input_file` transport — there is NO host code and NO server restart.
#
# Scheduler per platform (selected from `uname -s`):
#   Darwin  launchd LaunchAgent  (~/Library/LaunchAgents/<label>.plist)
#   Linux   systemd user timer   (~/.config/systemd/user/<label>.{service,timer})
# Any other platform, or Linux without a usable `systemctl --user`, exits with a
# clear error instead of installing half a job.
#
# Idempotent: re-run after rebuilding the collector to refresh the binary + job.
#
# The collector tracks TWO instances (hr + herdrtest) and reads their versions
# directly from each instance's own binary, plus a git local-changes count from
# the fork checkout. Those paths are resolved from HOME-relative defaults inside
# the collector (~/.herdr-hr/bin/herdr, ~/.local/bin/herdrtest,
# ~/.herdr-test/.../herdr.sock, and HERDR_VERSION_GIT_REPO for the checkout), so
# the job does not need a HERDR_VERSION_BIN. Override the collector's
# HERDR_VERSION_* vars only for a sandbox run.
#
# Env overrides (all optional):
#   HERDR_REPO         herdr checkout to build from (default: this script's git
#                      toplevel, so a clone anywhere works)
#   HR_ROOT            hr XDG root (default ~/.herdr-hr) — where version_input.json lands
#   VD_CHANNEL         stable|preview (default stable)
#   VD_INTERVAL_SECS   cadence (default 600)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Repo root: explicit override wins, else the git toplevel containing this
# script. Never a hardcoded path — the checkout lives wherever the user cloned it.
if [ -n "${HERDR_REPO:-}" ]; then
  REPO="$HERDR_REPO"
else
  REPO="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null || true)"
fi
if [ -z "$REPO" ] || [ ! -f "$REPO/Cargo.toml" ]; then
  echo "error: cannot locate the herdr checkout." >&2
  echo "       Run this from inside the repo, or set HERDR_REPO=/path/to/herdr." >&2
  exit 1
fi

HR_ROOT="${HR_ROOT:-$HOME/.herdr-hr}"
VD_CHANNEL="${VD_CHANNEL:-stable}"
VD_INTERVAL_SECS="${VD_INTERVAL_SECS:-600}"

LABEL="com.herdr.version-dash-collector"
BIN_DEST="$HOME/.local/bin/version-dash-collector"
LOG_DIR="$HR_ROOT/state/herdr"
LOG_FILE="$LOG_DIR/version-dash-collector.log"

OS="$(uname -s)"
case "$OS" in
  Darwin|Linux) ;;
  *)
    echo "error: no scheduler support for $OS." >&2
    echo "       Run $BIN_DEST every ${VD_INTERVAL_SECS}s from your own scheduler," >&2
    echo "       with XDG_STATE_HOME=$HR_ROOT/state and HERDR_VERSION_CHANNEL=$VD_CHANNEL." >&2
    exit 1
    ;;
esac

echo "==> building version-dash-collector (isolated target dir; never touches live hr binary)"
CARGO_TARGET_DIR="$REPO/target-versiondash" \
  cargo build --manifest-path "$REPO/Cargo.toml" -p version-dash-collector --release

echo "==> installing collector binary -> $BIN_DEST"
mkdir -p "$(dirname "$BIN_DEST")"
# mv over the destination (cp over a running binary SIGKILLs on macOS; the job
# is short-lived so a plain install is fine, but stay consistent with the repo rule).
mv -f "$REPO/target-versiondash/release/version-dash-collector" "$BIN_DEST"

mkdir -p "$LOG_DIR"

if [ "$OS" = "Darwin" ]; then
  PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
  echo "==> writing LaunchAgent -> $PLIST"
  mkdir -p "$(dirname "$PLIST")"
  cat > "$PLIST" <<PLISTEOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array><string>$BIN_DEST</string></array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>XDG_STATE_HOME</key><string>$HR_ROOT/state</string>
    <key>HERDR_VERSION_CHANNEL</key><string>$VD_CHANNEL</string>
    <!-- launchd agents get a bare PATH, so bake in the installing shell's PATH
         (which already has homebrew, cargo, etc. wherever they actually live)
         plus the standard dirs as a floor. -->
    <key>PATH</key><string>$PATH:/usr/bin:/bin:$HOME/.local/bin</string>
  </dict>
  <key>StartInterval</key><integer>$VD_INTERVAL_SECS</integer>
  <key>RunAtLoad</key><true/>
  <key>StandardErrorPath</key><string>$LOG_FILE</string>
</dict>
</plist>
PLISTEOF

  echo "==> (re)loading agent"
  launchctl unload "$PLIST" 2>/dev/null || true
  launchctl load "$PLIST"

  echo "==> kicking one immediate run"
  launchctl start "$LABEL" || true
else
  if ! command -v systemctl >/dev/null 2>&1 || ! systemctl --user show-environment >/dev/null 2>&1; then
    echo "error: no usable systemd user instance (systemctl --user)." >&2
    echo "       Run $BIN_DEST every ${VD_INTERVAL_SECS}s from cron or your own scheduler," >&2
    echo "       with XDG_STATE_HOME=$HR_ROOT/state and HERDR_VERSION_CHANNEL=$VD_CHANNEL." >&2
    exit 1
  fi

  # systemd applies `%` specifier expansion inside Environment=, so escape any
  # literal percent in the inherited PATH before baking it into the unit.
  SAFE_PATH="$(printf '%s' "$PATH" | sed 's/%/%%/g')"
  UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
  SERVICE="$UNIT_DIR/$LABEL.service"
  TIMER="$UNIT_DIR/$LABEL.timer"
  echo "==> writing systemd user units -> $SERVICE + $TIMER"
  mkdir -p "$UNIT_DIR"
  cat > "$SERVICE" <<SERVICEEOF
[Unit]
Description=herdr version-dash collector

[Service]
Type=oneshot
ExecStart=$BIN_DEST
Environment="XDG_STATE_HOME=$HR_ROOT/state"
Environment="HERDR_VERSION_CHANNEL=$VD_CHANNEL"
# User units get systemd's own PATH, so bake in the installing shell's PATH
# (which already has cargo, git, curl wherever they actually live).
Environment="PATH=$SAFE_PATH:/usr/bin:/bin:$HOME/.local/bin"
# append: needs systemd 240+; on anything older drop this line and read the
# collector's stderr with: journalctl --user -u $LABEL.service
StandardError=append:$LOG_FILE
SERVICEEOF
  cat > "$TIMER" <<TIMEREOF
[Unit]
Description=herdr version-dash collector every ${VD_INTERVAL_SECS}s

[Timer]
# RunAtLoad equivalent (fires on timer activation) + the same cadence as the
# macOS StartInterval.
OnActiveSec=0
OnUnitActiveSec=${VD_INTERVAL_SECS}s
AccuracySec=30s
Unit=$LABEL.service

[Install]
WantedBy=timers.target
TIMEREOF

  echo "==> (re)loading timer"
  systemctl --user daemon-reload
  systemctl --user enable --now "$LABEL.timer"
  systemctl --user restart "$LABEL.timer"

  echo "==> kicking one immediate run"
  systemctl --user start "$LABEL.service" || true
fi

echo "done. cadence: every ${VD_INTERVAL_SECS}s. state file:"
echo "  $HR_ROOT/state/herdr/plugins/builtin.version-dash/version_input.json"
