#!/bin/bash
# Install (or refresh) the version-dash collector as a macOS LaunchAgent that
# runs every 10 minutes, writing version_input.json into the hr instance's
# plugin state dir. The herdr host reads that file each frame via the generic
# `ui_input_file` transport — there is NO host code and NO server restart.
#
# Idempotent: re-run after rebuilding the collector to refresh the binary + agent.
#
# Env overrides (all optional):
#   HR_ROOT            hr XDG root (default ~/.herdr-hr)
#   HERDR_VERSION_BIN  herdr binary whose --version is the "current" (default the fork release binary)
#   VD_CHANNEL         stable|preview (default stable)
#   VD_INTERVAL_SECS   cadence (default 600)
set -euo pipefail

REPO="$HOME/Projects/herdr"
HR_ROOT="${HR_ROOT:-$HOME/.herdr-hr}"
HERDR_VERSION_BIN="${HERDR_VERSION_BIN:-$REPO/target/release/herdr}"
VD_CHANNEL="${VD_CHANNEL:-stable}"
VD_INTERVAL_SECS="${VD_INTERVAL_SECS:-600}"

LABEL="com.herdr.version-dash-collector"
BIN_DEST="$HOME/.local/bin/version-dash-collector"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"

echo "==> building version-dash-collector (isolated target dir; never touches live hr binary)"
CARGO_TARGET_DIR="$REPO/target-versiondash" \
  cargo build --manifest-path "$REPO/Cargo.toml" -p version-dash-collector --release

echo "==> installing collector binary -> $BIN_DEST"
mkdir -p "$(dirname "$BIN_DEST")"
# mv over the destination (cp over a running binary SIGKILLs on macOS; the agent
# is short-lived so a plain install is fine, but stay consistent with the repo rule).
mv -f "$REPO/target-versiondash/release/version-dash-collector" "$BIN_DEST"

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
    <key>HERDR_VERSION_BIN</key><string>$HERDR_VERSION_BIN</string>
    <key>HERDR_VERSION_CHANNEL</key><string>$VD_CHANNEL</string>
    <key>PATH</key><string>/usr/bin:/bin:/usr/local/bin:$HOME/.local/bin</string>
  </dict>
  <key>StartInterval</key><integer>$VD_INTERVAL_SECS</integer>
  <key>RunAtLoad</key><true/>
  <key>StandardErrorPath</key><string>$HR_ROOT/state/herdr/version-dash-collector.log</string>
</dict>
</plist>
PLISTEOF

echo "==> (re)loading agent"
launchctl unload "$PLIST" 2>/dev/null || true
launchctl load "$PLIST"

echo "==> kicking one immediate run"
launchctl start "$LABEL" || true

echo "done. cadence: every ${VD_INTERVAL_SECS}s. state file:"
echo "  $HR_ROOT/state/herdr/plugins/builtin.version-dash/version_input.json"
