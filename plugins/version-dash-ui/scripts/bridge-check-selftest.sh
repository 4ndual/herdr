#!/bin/bash
# bridge-check-selftest.sh — proves `herdr-bridge-check.sh` decides correctly.
#
# Three layers, because no single one is sufficient:
#
#   A. SYNTHETIC fixtures. Real binaries only ever exercise "all markers" or "no
#      markers". The dangerous middle — a FORK build whose bridge got stripped
#      (exit 23) — has no real specimen, so we compile Mach-O fixtures with the
#      exact marker sets we want. This is the only layer that tests the decision
#      logic rather than the corpus.
#
#   B. DRIFT ALARM. Every marker literal the gate requires must still exist in
#      the fork source tree. If someone renames `herdr_ui_api_version` or
#      reworks the fork self-update refusal message, this test fails LOUDLY
#      instead of the gate silently accepting an unbridged binary (or silently
#      rejecting every good one). This is the same "disarm alarm" pattern as
#      `update::tests::fork_marker_is_baked_into_this_build`.
#
#   C. REAL binaries present on this machine, checked against their known truth.
#      Skipped individually when a path is absent, so the suite still runs on a
#      fresh machine.
#
# Usage: bridge-check-selftest.sh [--verbose]
# Exit:  0 all passed, 1 any failure.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
GATE="$SCRIPT_DIR/herdr-bridge-check.sh"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
VERBOSE=0
[[ "${1:-}" == "--verbose" ]] && VERBOSE=1

PASS=0; FAIL=0; SKIP=0
ok()   { PASS=$((PASS+1)); printf '  ok   %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL %s\n' "$*"; }
skip() { SKIP=$((SKIP+1)); printf '  skip %s\n' "$*"; }

[[ -x "$GATE" ]] || { printf 'gate not executable: %s\n' "$GATE" >&2; exit 1; }

WORK="$(mktemp -d -t bridge-selftest)"
trap 'rm -rf "$WORK"' EXIT

# The marker sets the gate requires, mirrored here. Layer B proves they still
# exist in the source; layer A uses them to build fixtures.
FORK_MARKER="self-update is disabled for fork builds"
# Mirrors the gate exactly. NOTE `ui_plugin_api_bridge` is deliberately NOT here:
# it is absent from healthy fork binaries, so using it as a marker would fail the
# gate closed on good builds.
BRIDGE_MARKERS=(herdr_ui_api_version herdr_ui_alloc herdr_ui_on_event herdr_ui_static_update_ptr \
                render_ui_plugin_layers UiCapability)
OTHER_MARKERS=(ui_modules HERDR_PLUGIN_UI_INPUT_JSON wasmtime)

# ── layer A — synthetic Mach-O fixtures ────────────────────────────────────
# Each fixture is a real native executable that answers `--version` like herdr
# does and carries exactly the marker strings we choose. `volatile` + a runtime
# reference keeps the linker from dead-stripping the literals.
make_fixture() {
  local name="$1" version="$2"; shift 2
  local src="$WORK/$name.c" out="$WORK/$name"
  {
    printf '#include <stdio.h>\n#include <string.h>\n'
    printf 'static const char *const markers[] = {\n'
    local m
    for m in "$@"; do
      printf '  "%s",\n' "$m"
    done
    printf '  0\n};\n'
    printf 'int main(int argc, char **argv) {\n'
    printf '  volatile int keep = 0;\n'
    printf '  for (int i = 0; markers[i]; i++) keep += (int) strlen(markers[i]);\n'
    printf '  if (argc > 1 && strcmp(argv[1], "--version") == 0) { printf("herdr %%s\\n", "%s"); return 0; }\n' "$version"
    printf '  return keep ? 0 : 0;\n}\n'
  } >"$src"
  cc -O0 -o "$out" "$src" 2>/dev/null || return 1
  printf '%s\n' "$out"
}

expect_exit() {
  local label="$1" bin="$2" want="$3"
  local got
  "$GATE" "$bin" --quiet >/dev/null 2>&1
  got=$?
  if [[ "$got" == "$want" ]]; then
    ok "$label → exit $got"
  else
    bad "$label → exit $got, expected $want"
    [[ $VERBOSE == 1 ]] && "$GATE" "$bin" --json --quiet
  fi
}

printf 'A. synthetic fixtures (decision logic)\n'
if ! command -v cc >/dev/null 2>&1; then
  skip "no C compiler — synthetic fixtures unavailable"
else
  # A1 upstream shape: answers --version, zero fork/bridge markers.
  if F="$(make_fixture upstream 0.7.5)"; then
    expect_exit "upstream-shaped binary (no markers)" "$F" 22
  else bad "could not compile upstream fixture"; fi

  # A2 THE CASE NO REAL BINARY COVERS: fork marker present, bridge stripped.
  # A binary-only "is it a fork build" check would ACCEPT this one.
  if F="$(make_fixture forkonly 0.7.5 "$FORK_MARKER")"; then
    expect_exit "fork build with the bridge stripped" "$F" 23
  else bad "could not compile forkonly fixture"; fi

  # A3 partial bridge — three of four ABI symbols. Still a reject: the gate
  # requires every marker, so a half-migrated build cannot slip through.
  if F="$(make_fixture partial 0.7.5 "$FORK_MARKER" "${BRIDGE_MARKERS[0]}" "${BRIDGE_MARKERS[1]}" "${BRIDGE_MARKERS[2]}" "${OTHER_MARKERS[@]}")"; then
    expect_exit "fork build missing one ABI symbol" "$F" 23
  else bad "could not compile partial fixture"; fi

  # A4 complete marker set ⇒ accept. Proves the gate is not simply always-reject.
  if F="$(make_fixture full 0.7.5 "$FORK_MARKER" "${BRIDGE_MARKERS[@]}" "${OTHER_MARKERS[@]}")"; then
    expect_exit "fork build with a complete bridge" "$F" 0
  else bad "could not compile full fixture"; fi

  # A5 no --version ⇒ reject before markers are ever consulted.
  if F="$(make_fixture noversion "" "$FORK_MARKER" "${BRIDGE_MARKERS[@]}" "${OTHER_MARKERS[@]}")"; then
    expect_exit "binary that will not report --version" "$F" 21
  else bad "could not compile noversion fixture"; fi
fi

# A6/A7 structural rejections need no compiler.
expect_exit "missing file" "$WORK/definitely-not-here" 20
printf '#!/bin/sh\necho "herdr 9.9.9"\n' >"$WORK/wrapper.sh"; chmod +x "$WORK/wrapper.sh"
expect_exit "shell wrapper posing as the binary" "$WORK/wrapper.sh" 20

# ── layer B — drift alarm against the fork source ──────────────────────────
printf 'B. drift alarm (markers still exist in the fork source)\n'
if [[ ! -d "$REPO_ROOT/src" ]]; then
  skip "fork source tree not found at $REPO_ROOT"
else
  check_source_marker() {
    local marker="$1" where="$2"
    if LC_ALL=C grep -rqF -- "$marker" "$REPO_ROOT/$where" 2>/dev/null; then
      ok "marker present in $where: $marker"
    else
      bad "marker MISSING from $where — the gate is drifting: $marker"
    fi
  }
  check_source_marker "$FORK_MARKER" src/update.rs
  for m in herdr_ui_api_version herdr_ui_alloc herdr_ui_on_event herdr_ui_static_update_ptr; do
    check_source_marker "$m" src/ui_plugins.rs
  done
  check_source_marker "render_ui_plugin_layers" src/ui.rs
  check_source_marker "UiCapability" src/ui_plugins.rs
  check_source_marker "ui_modules" src/api/schema/plugins.rs
  check_source_marker "HERDR_PLUGIN_UI_INPUT_JSON" src/ui_plugins.rs
  # The fork self-update guard must still be armed in .cargo/config.toml, else
  # a future fork build would carry no fork marker and the gate would reject it.
  if LC_ALL=C grep -qF "HERDR_FORK_BUILD" "$REPO_ROOT/.cargo/config.toml" 2>/dev/null; then
    ok "HERDR_FORK_BUILD still baked by .cargo/config.toml"
  else
    bad "HERDR_FORK_BUILD marker removed from .cargo/config.toml — future fork builds would fail the gate"
  fi
fi

# ── layer C — real binaries on this machine ────────────────────────────────
printf 'C. real binaries\n'
real_case() {
  local path="$1" want="$2" note="$3"
  if [[ ! -f "$path" ]]; then skip "absent: $note"; return; fi
  local got; "$GATE" "$path" --quiet >/dev/null 2>&1; got=$?
  if [[ "$got" == "$want" ]]; then ok "$note → exit $got"
  else bad "$note → exit $got, expected $want"; fi
}
real_case "$HOME/.local/bin/herdrtest-v0.7.3-backup"                 0  "known-good fork binary (herdrtest v0.7.3 backup)"
real_case "$HOME/.herdr-hr/bin/herdr"                                0  "hr snapshot binary"
real_case "$HOME/.local/bin/herdrtest-upstream075-corrupt-20260727" 22  "bridge-less upstream 0.7.5 (the binary that broke hr)"
real_case "$HOME/.local/bin/herdr.pre-prod-promote-20260619-131550" 22  "official herdr 0.7.0 crisis fallback"

printf '\n%s passed, %s failed, %s skipped\n' "$PASS" "$FAIL" "$SKIP"
[[ $FAIL -eq 0 ]]
