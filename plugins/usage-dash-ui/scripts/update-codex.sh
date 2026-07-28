#!/usr/bin/env bash
# Update the standalone codex CLI.
#
# Deliberately does NOT restart the pane. In a cx pane the running binary is
# Claude Code — codex is a separate program the gateway routes to — so updating
# it changes nothing about the process the pane is running and interrupting the
# user would be pure cost.
#
# Install shape: ~/.codex/packages/standalone/releases/<version>-<triple>, with
# `current` symlinked at the active one and ~/.local/bin/codex pointing there.

set -uo pipefail

LOG="$HOME/.herdr-cli-update.log"
log() { printf '[%s] %s\n' "$(date '+%Y-%m-%d %H:%M:%S')" "$*" >>"$LOG"; }

CODEX="${CODEX_BIN:-$HOME/.local/bin/codex}"
if [[ ! -x "$CODEX" ]]; then
  CODEX="$(command -v codex 2>/dev/null || true)"
fi
if [[ -z "$CODEX" || ! -x "$CODEX" ]]; then
  log "update-codex: FAILED — codex not found"
  exit 3
fi

BEFORE="$("$CODEX" --version 2>/dev/null | tr -d '\n')"
OUT="$("$CODEX" update 2>&1)"
RC=$?
AFTER="$("$CODEX" --version 2>/dev/null | tr -d '\n')"
log "update-codex: rc=$RC before='$BEFORE' after='$AFTER' out='${OUT//$'\n'/ | }'"

if [[ $RC -ne 0 ]]; then
  # `codex update` may not exist on every build; say so plainly in the log
  # rather than leaving a silent failure that reads as "nothing happened".
  log "update-codex: updater exited $RC — update codex manually"
  exit "$RC"
fi
log "update-codex: done — $BEFORE → $AFTER"
