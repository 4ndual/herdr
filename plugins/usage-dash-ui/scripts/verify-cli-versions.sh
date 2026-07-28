#!/usr/bin/env bash
# Force a fresh latest-version check, bypassing the collector's 30-minute cache.
#
# Bound to a click on an already-CURRENT (green) version token: the question
# being asked is "is this actually still true?", so the honest answer is a real
# refetch rather than re-reading the cache that produced the green in the first
# place.
#
# Implementation: delete the cache file. The collector refetches on its next
# tick (~1s) because a missing entry is always treated as expired. That keeps
# the fetch logic in exactly one place instead of duplicating it here.

set -uo pipefail

LOG="$HOME/.herdr-cli-update.log"
log() { printf '[%s] %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*" >>"$LOG"; }

STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-}"
if [[ -z "$STATE_DIR" ]]; then
  log "verify-cli-versions: no HERDR_PLUGIN_STATE_DIR — cannot locate the cache"
  exit 3
fi

CACHE="$STATE_DIR/cli_versions_cache.json"
if [[ -f "$CACHE" ]]; then
  rm -f "$CACHE"
  log "verify-cli-versions: cache cleared ($CACHE) — collector refetches next tick"
else
  log "verify-cli-versions: no cache present at $CACHE — collector will fetch anyway"
fi
