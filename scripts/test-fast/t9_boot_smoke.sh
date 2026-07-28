#!/usr/bin/env bash
# T9 — sandbox boot smoke (host scope).
#
# Real bug: the herdrtest relaunch lost its XDG root and booted a server that
# never bound a connectable socket. This exercises the runtime end of that
# contract: bring up a real isolated sandbox from the fork release binary and
# assert the server binds a live socket the CLI can actually talk to — then
# tear it down (teardown NEVER touches the vault, prod, hr, or herdrtest).
#
# Uses hsand for isolation (own XDG root + socket + tmux session). ~28K/sandbox,
# mandatory teardown even on failure.
set -uo pipefail

HSAND="$HOME/bin/hsand"
[ -x "$HSAND" ] || { echo "hsand not found at $HSAND"; exit 1; }

# Unique-ish slug without Date.now — pid-based, cleaned up regardless.
SLUG="tf-boot-$$"
ROOT="$HOME/.herdr-sb-$SLUG"
SOCK="$ROOT/config/herdr/sessions/$SLUG/herdr.sock"

cleanup() { "$HSAND" down "$SLUG" >/dev/null 2>&1 || true; }
trap cleanup EXIT

# Bring the sandbox up (hsand already waits up to ~10s for the socket).
if ! "$HSAND" up "$SLUG" >/tmp/tf9.$$.out 2>&1; then
  cat /tmp/tf9.$$.out; rm -f /tmp/tf9.$$.out
  echo "sandbox did not boot to a connectable socket"; exit 1
fi
rm -f /tmp/tf9.$$.out

[ -S "$SOCK" ] || { echo "no socket at $SOCK after boot"; exit 1; }

# Prove the socket is actually serviced: a real client command must round-trip.
# Clear inherited hr/test env, point only at the sandbox socket.
env -u HERDR_ENV -u HERDR_CLIENT_SOCKET_PATH -u HERDR_SESSION \
    -u HERDR_WORKSPACE_ID -u HERDR_TAB_ID -u HERDR_PANE_ID \
    HERDR_SOCKET_PATH="$SOCK" \
    "$HOME/Projects/herdr/target/release/herdr" agent list >/tmp/tf9c.$$.out 2>&1
rc=$?
if [ $rc -ne 0 ]; then
  echo "client could not talk to the freshly-booted server (rc=$rc):"
  sed 's/^/    /' /tmp/tf9c.$$.out; rm -f /tmp/tf9c.$$.out
  exit 1
fi
rm -f /tmp/tf9c.$$.out
echo "ok: sandbox booted to a live, serviceable socket"
