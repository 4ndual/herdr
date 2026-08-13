#!/usr/bin/env bash
# Opens the runtime-swap picker as a floating overlay pane.
#
# The overlay resolves its own source pane from HERDR_PLUGIN_CONTEXT_JSON, which
# the host captures BEFORE the overlay takes focus — so it sees the pane the user
# was looking at, not the overlay itself.
set -euo pipefail

HERDR="${HERDR_BIN_PATH:-herdr}"
PLUGIN_ID="${HERDR_PLUGIN_ID:-builtin.runtime-swap}"

exec "$HERDR" plugin pane open \
  --plugin "$PLUGIN_ID" \
  --entrypoint picker \
  --placement overlay \
  --focus
