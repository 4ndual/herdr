#!/usr/bin/env bash
# swap-here.sh — dash action: open the "resume this session anywhere" picker as
# a floating OVERLAY pane.
#
# Successor to codex-here.sh, which ran the swap directly because there was
# nothing to choose: one runtime, one harness. With both ends open the choice
# has to be shown, and a herdr button cannot take typed input, so the button
# opens a surface instead of performing an action.
#
# The overlay resolves its own source pane from HERDR_PLUGIN_CONTEXT_JSON, which
# the host captures BEFORE the overlay takes focus — so it acts on the pane the
# user was looking at, not on itself. All swap logic lives in `harness
# swap-here` (harness-fleet/go/swaphere.go); this script only opens the window.
set -u
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/dash-common.sh" 2>/dev/null || true

HERDR="${HERDR_BIN:-${HERDR_BIN_PATH:-herdr}}"
PLUGIN_ID="${HERDR_PLUGIN_ID:-builtin.button-dash}"
TUI="$SCRIPT_DIR/../tui-swap/target/release/swap-here-tui"

# Say WHY nothing opened rather than flashing an empty overlay: the pane command
# would exit instantly and the user would be left with a blank floating box.
if [[ ! -x "$TUI" ]]; then
  if declare -F toast >/dev/null 2>&1; then
    toast "Swap here unavailable" "picker not built — run: cd $(dirname "$TUI")/../.. && cargo build --release"
  else
    echo "swap-here: picker binary missing at $TUI" >&2
  fi
  exit 3
fi

exec "$HERDR" plugin pane open \
  --plugin "$PLUGIN_ID" \
  --entrypoint swap \
  --placement overlay \
  --focus
