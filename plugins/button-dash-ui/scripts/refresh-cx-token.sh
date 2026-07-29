#!/usr/bin/env bash
# refresh-cx-token.sh — re-sync the CCR gateway's Claude-Plan OAuth token from the
# daily Keychain credential, on demand, from the button dash.
#
# WHY THIS BUTTON EXISTS (bitácora 2026-07-21/28, family F1): CCR authenticates its
# "Claude Plan" provider with a STATIC bearer header and has no Anthropic refresh
# executor. When that copied access token expires, the gateway serves a permanent
# 401 for every Claude-via-gateway request — cx dies mid-session and RETRYING IN
# THE SAME SESSION CANNOT FIX IT, because the only re-sync runs at cx launch. That
# is the exact failure of 2026-07-22 and again 2026-07-28.
#
# This button is the missing manual trigger: it fixes a LIVE cx session in place,
# with no relaunch and no daemon. The underlying script's own docs say "MANUAL
# ONLY — never schedule this"; a button is precisely that, which is why this is a
# button and not a LaunchAgent.
#
# SAFETY (all enforced by the underlying script, not by this wrapper):
#   - Daily Claude credentials are read READ-ONLY from the Keychain; daily
#     `claude` is direct-to-Anthropic and completely unaffected.
#   - Only the two exact Claude OAuth providerPlugins' authorization values are
#     replaced; nothing else in the CCR config is touched.
#   - The token value is NEVER printed, logged or toasted — only its length.
#   - Idempotent: an already-fresh token does a read-only getConfig and no write.
#
# Usage:  refresh-cx-token.sh          # inject if stale, toast the outcome
#   Takes no pane context — the gateway is machine-global, so this is correct from
#   any pane, including a pane that is not itself running cx.

set -u

REFRESH="${CMIX_REFRESH_TOKEN:-$HOME/cmix/scripts/refresh-claude-token.py}"
LOG="${HERDR_CX_TOKEN_LOG:-$HOME/.herdr-cx-token.log}"
log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG" 2>/dev/null; }

# ---------------------------------------------------------------------------
# 0. Resolve the herdr client binary (prefer the fork build; never assume PATH).
#    Same resolution order as the sibling button scripts.
# ---------------------------------------------------------------------------
HERDR=""
# PATH LAST, deliberately: `command -v herdr` resolves to ~/.local/bin/herdr,
# a symlink to a June-19 pre-prod build missing this fork's subcommands. Putting
# it first silently ran the wrong binary. Prefer the live instance binary.
for cand in "${HERDR_BIN:-}" \
  "$HOME/.herdr-hr/bin/herdr" \
  "$HOME/Projects/herdr-worktrees/imgapi-127/target/release/herdr" \
  "$HOME/Projects/herdr/target/release/herdr" \
  "$(command -v herdr 2>/dev/null)"; do
  if [[ -n "$cand" && -x "$cand" ]]; then
    HERDR="$cand"
    break
  fi
done

# Surface a short message to the user. Prefer a herdr toast; always log too.
toast() {
  local title="$1" body="${2:-}"
  log "TOAST: $title — $body"
  [[ -n "$HERDR" ]] || return 0
  "$HERDR" notification show "$title" ${body:+--body "$body"} >>"$LOG" 2>&1 || true
}

# ---------------------------------------------------------------------------
# 1. The refresh script must exist. It lives in the cmix repo, not in herdr, so a
#    missing/renamed script is a real possibility and must NOT fail silently —
#    a silent no-op here would look exactly like "the token was already fresh",
#    which is the failure shape this whole button exists to eliminate.
# ---------------------------------------------------------------------------
if [[ ! -f "$REFRESH" ]]; then
  toast "cx token: script missing" "Not found: $REFRESH"
  exit 3
fi

PY="$(command -v python3 2>/dev/null || echo /usr/bin/python3)"

# ---------------------------------------------------------------------------
# 2. Run it. Both streams are captured: the script prints its one-line result to
#    stdout and its one-line failure to stderr, never a traceback, never a token.
# ---------------------------------------------------------------------------
OUT="$("$PY" "$REFRESH" 2>&1)"
RC=$?
log "rc=$RC out=$OUT"

# Last non-empty line is the verdict line.
LAST="$(printf '%s\n' "$OUT" | awk 'NF{l=$0} END{print l}')"

if [[ $RC -ne 0 ]]; then
  # Most common real cause: the DAILY token is itself expired, in which case the
  # fix is to re-login daily claude first — say so instead of just echoing rc.
  toast "cx token refresh FAILED" "${LAST:-exit $RC} — if daily claude is also stale, re-login it first."
  exit "$RC"
fi

case "$OUT" in
  *"refreshed CCR Claude token"*)
    toast "cx token refreshed" "Gateway Claude token re-synced from Keychain. Live cx sessions work again — no relaunch needed."
    ;;
  *"already up to date"*)
    toast "cx token already fresh" "No write needed. If cx is still failing, the cause is NOT the token — check the CCR request log."
    ;;
  *)
    toast "cx token: unexpected result" "${LAST:-no output}"
    ;;
esac
