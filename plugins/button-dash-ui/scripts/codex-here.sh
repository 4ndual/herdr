#!/usr/bin/env bash
# codex-here.sh — dash action: reopen THIS pane's work in the codex runtime,
# in place (spawn into the same tab, then close the old pane — the same
# ordering relaunch-skip.sh uses, so a failed spawn never leaves you paneless).
#
# All logic lives in Go: `harness codex-here` resolves the pane, maps its cwd
# back to the owning harness, points CODEX_HOME at that harness's config dir,
# and resumes the harness's OWN codex session (state/session-id.codex) when one
# exists — a claude transcript can never be resumed by codex, so the contract is
# "same harness, same workspace, codex's own latest session". The claude session
# stays on disk; `harness runtime set <n> claude` brings it back.
set -u
export PATH="$HOME/.bun/bin:$PATH"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/dash-common.sh" 2>/dev/null || true

HARNESS_BIN="${HARNESS_BIN:-}"
if [[ -z "$HARNESS_BIN" ]]; then
  for c in "$HOME/.local/bin/harness" "$HOME/Projects/harness-fleet/bin/harness" \
           "$HOME/.claude/plugins/cache/harness-fleet/harness-tidus/1.0.1/bin/harness"; do
    [[ -x "$c" ]] && HARNESS_BIN="$c" && break
  done
fi
if [[ -z "$HARNESS_BIN" ]]; then
  if declare -F toast >/dev/null 2>&1; then
    toast "Codex here failed" "no harness binary found (install the harness-tidus plugin)"
  else
    echo "codex-here: no harness binary found" >&2
  fi
  exit 3
fi

OUT="$("$HARNESS_BIN" codex-here 2>&1)"
RC=$?
if [[ $RC -ne 0 ]]; then
  if declare -F toast >/dev/null 2>&1; then
    toast "Codex here failed" "${OUT:-exit $RC}"
  else
    echo "$OUT" >&2
  fi
  exit $RC
fi
exit 0
