#!/usr/bin/env bash
# show-latest-notes.sh — open the latest release's patch notes in a new pane.
#
# Invoked by the version-dash guest when the user clicks the trailing
# `latest X.Y.Z` token. Everything it reads is LOCAL: the collector caches the
# release-notes markdown out of the channel manifest on every successful fetch,
# so the click costs no network round trip and still works offline. A click that
# has to wait on the network is a click that looks broken.
#
# The pane is split from the plugin's own session and runs a pager, so the notes
# are scrollable and closing them is the ordinary "quit the pager / close the
# pane" gesture rather than a bespoke overlay with bespoke keys.
set -uo pipefail

HERDR_BIN="${HERDR_BIN_PATH:-herdr}"
STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-}"
NOTES="$STATE_DIR/latest_notes.md"
ARCHIVE="$STATE_DIR/releases_notes.md"

log() { printf '[show-latest-notes] %s\n' "$*" >&2; }

if [[ -z "$STATE_DIR" ]]; then
  log "HERDR_PLUGIN_STATE_DIR unset; cannot locate the notes cache"
  exit 2
fi

# Fall back to the full archive, then to a written explanation. The pane always
# opens: a click that silently does nothing is indistinguishable from a broken
# plugin, so an empty cache must still produce a readable answer.
VIEW="$NOTES"
if [[ ! -s "$VIEW" ]]; then
  if [[ -s "$ARCHIVE" ]]; then
    VIEW="$ARCHIVE"
  else
    VIEW="$STATE_DIR/latest_notes.missing.md"
    cat >"$VIEW" <<'EOF'
# No release notes cached yet

The collector caches release notes on each successful fetch of the channel
manifest. It has not completed one yet, or the fetch failed.

Check:
  version-dash-collector            # run it once by hand
  ~/.herdr-hr/state/herdr/version-dash-collector.log
EOF
  fi
fi

# Pager preference: a markdown renderer if one is installed, else less, else cat
# with a hold so the pane does not vanish before it can be read.
if command -v glow >/dev/null 2>&1; then
  PAGER_CMD=(glow -p "$VIEW")
elif command -v bat >/dev/null 2>&1; then
  PAGER_CMD=(bat --style=plain --paging=always "$VIEW")
elif command -v less >/dev/null 2>&1; then
  PAGER_CMD=(less -R "$VIEW")
else
  PAGER_CMD=(sh -c "cat '$VIEW'; printf '\n[enter to close]'; read _")
fi

# Split, then run: `pane split` creates the pane, `pane run` puts the command in
# it. Reading the new pane id out of the split response is what lets the two
# steps address the same pane.
split_json="$("$HERDR_BIN" pane split --current --direction down --ratio 0.45 2>&1)" || {
  log "pane split failed: $split_json"
  exit 1
}

pane_id="$(printf '%s' "$split_json" | python3 -c '
import json, re, sys

# A pane id looks like `w2:p1M`. Matching on the KEY alone is not enough: the
# response envelope carries its own `"id": "cli:pane:split"`, which contains
# ":p" and would otherwise be picked up as a pane. Require the shape.
PANE = re.compile(r"^w[0-9A-Za-z]+:p[0-9A-Za-z]+$")
raw = sys.stdin.read()

try:
    doc = json.loads(raw)
except Exception:
    m = re.search(r"\bw[0-9A-Za-z]+:p[0-9A-Za-z]+\b", raw)
    print(m.group(0) if m else "")
    sys.exit(0)


def find(node):
    if isinstance(node, dict):
        # Prefer the explicitly named field before falling back to shape.
        v = node.get("pane_id")
        if isinstance(v, str) and PANE.match(v):
            return v
        for value in node.values():
            got = find(value)
            if got:
                return got
    elif isinstance(node, list):
        for value in node:
            got = find(value)
            if got:
                return got
    elif isinstance(node, str) and PANE.match(node):
        return node
    return None


print(find(doc) or "")
')"

if [[ -z "$pane_id" ]]; then
  log "could not determine the new pane id from: $split_json"
  exit 1
fi

"$HERDR_BIN" pane run "$pane_id" "${PAGER_CMD[@]}" || {
  log "pane run failed in $pane_id"
  exit 1
}
log "opened $VIEW in $pane_id"
