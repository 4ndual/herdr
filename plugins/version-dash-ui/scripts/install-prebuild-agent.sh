#!/bin/bash
# install-prebuild-agent.sh — schedule prebuild-trigger.sh so the fork tip is
# already built by the time the button is pressed.
#
# This is the piece that turns "click and wait 2+ minutes with an unbounded tail
# if the merge broke" into "click and wait ~25 seconds". The build is the slow,
# failure-prone half; running it on a cadence means the click only ever does
# install + relaunch + verify.
#
# The trigger itself is a sha comparison (see prebuild-trigger.sh) and exits 0
# immediately when nothing moved, so the common case costs milliseconds.
#
# Cadence defaults to 30 minutes. It is deliberately NOT tied to the collector's
# 10-minute poll: the collector must stay sub-second so the strip refreshes
# promptly after a phase change, and a build must never be able to delay it.
#
# NOTE: this agent is OFF BY DEFAULT — installing it is a decision to spend CPU
# on speculative builds, and per the user's standing preference around
# LaunchAgents, nothing here schedules itself. Run this script to opt in;
# `--uninstall` to opt back out.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
LABEL="com.herdr.version-dash-prebuild"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
HR_ROOT="${HR_ROOT:-$HOME/.herdr-hr}"
HERDR_REPO="${HERDR_REPO:-$HOME/Projects/herdr}"
INTERVAL="${VD_PREBUILD_INTERVAL_SECS:-1800}"

if [[ "${1:-}" == "--uninstall" ]]; then
  launchctl unload "$PLIST" 2>/dev/null || true
  rm -f "$PLIST"
  echo "removed $LABEL"
  exit 0
fi

mkdir -p "$(dirname "$PLIST")"
cat > "$PLIST" <<PLISTEOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/bash</string>
    <string>$SCRIPT_DIR/prebuild-trigger.sh</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>HERDR_REPO</key><string>$HERDR_REPO</string>
    <key>PATH</key><string>/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.local/bin</string>
  </dict>
  <key>StartInterval</key><integer>$INTERVAL</integer>
  <key>RunAtLoad</key><false/>
  <key>Nice</key><integer>10</integer>
  <key>LowPriorityIO</key><true/>
  <key>StandardErrorPath</key><string>$HR_ROOT/state/herdr/version-dash-prebuild.log</string>
</dict>
</plist>
PLISTEOF

launchctl unload "$PLIST" 2>/dev/null || true
launchctl load "$PLIST"
echo "installed $LABEL (every ${INTERVAL}s, niced, no run-at-load)"
echo "  uninstall: bash $0 --uninstall"
