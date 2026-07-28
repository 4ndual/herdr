#!/usr/bin/env bash
# collect-detached.sh — out-of-process collector for the detached-sessions dash.
#
# The host runs this once per dashboard tick (declared as the module `collector`
# in herdr-plugin.toml). It enumerates LIVE external tmux sessions whose name
# starts with the detached prefix (created by button-dash's detach-conversation.sh)
# and prints the sticky-snapshot protocol on stdout:
#
#   {"outcome":"replace","snapshot":{"sessions":[ {name,label,agent,session,cwd} ]}}
#
# Because the list is rebuilt from `tmux list-sessions` every tick, a session that
# dies (agent exits → `exec` shell ends → tmux session gone) disappears from the
# dash within one poll. No hardcoding: everything is read back from the tmux
# user-options the detach script stamped on each session.

set -u

PREFIX="${HERDR_DETACHED_PREFIX:-herdr-detached}"

if ! command -v tmux >/dev/null 2>&1; then
  for t in /opt/homebrew/bin/tmux /usr/local/bin/tmux /usr/bin/tmux; do
    [[ -x "$t" ]] && { export PATH="$(dirname "$t"):$PATH"; break; }
  done
fi
if ! command -v tmux >/dev/null 2>&1; then
  # No tmux at all ⇒ nothing detached.
  printf '%s\n' '{"outcome":"replace","snapshot":{"sessions":[]}}'
  exit 0
fi

# List sessions with their stamped metadata. Format is tab-separated so labels /
# cwds with spaces survive. Missing user-options come back empty.
LINES="$(tmux list-sessions -F \
  '#{session_name}	#{@herdr_detached_label}	#{@herdr_detached_agent}	#{@herdr_detached_session}	#{@herdr_detached_cwd}' \
  2>/dev/null)"

emit() {
  # Build the JSON array from the matching lines, JSON-escaping each field.
  awk -F '\t' -v prefix="$PREFIX" '
    function esc(s,   r) {
      r = s
      gsub(/\\/, "\\\\", r)
      gsub(/"/, "\\\"", r)
      gsub(/\t/, " ", r)
      gsub(/\r/, "", r)
      return r
    }
    BEGIN { printf "{\"outcome\":\"replace\",\"snapshot\":{\"sessions\":["; n = 0 }
    {
      name = $1
      if (index(name, prefix) != 1) next
      if (n++ > 0) printf ","
      printf "{\"name\":\"%s\",\"label\":\"%s\",\"agent\":\"%s\",\"session\":\"%s\",\"cwd\":\"%s\"}", \
        esc($1), esc($2), esc($3), esc($4), esc($5)
    }
    END { printf "]}}" }
  '
}

printf '%s\n' "$LINES" | emit
printf '\n'
