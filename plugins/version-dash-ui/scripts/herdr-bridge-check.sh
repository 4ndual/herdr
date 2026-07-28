#!/bin/bash
# herdr-bridge-check.sh — FAIL-CLOSED gate: is this herdr binary a fork build that
# still carries the WASM UI-plugin bridge?
#
# WHY THIS EXISTS
# ---------------
# The version-dash right-click promotion path used to gate on one thing only:
# "does the candidate binary answer `--version`". An UPSTREAM stable binary
# answers `--version` perfectly. Promoting one into `hr` destroys the entire
# WASM plugin bridge — the exact failure `.local/prd/fork-update-safety-audit.md`
# was written to prevent, arriving through the update button instead of through
# `herdr update`. Worse, per that audit's Finding 3, a single BOOT of a
# bridge-less binary against fork state rewrites `plugins.json` and permanently
# strips `ui_modules`, which SURVIVES rolling the binary back.
#
# So every promotion must first prove the candidate is a fork build WITH the
# bridge compiled in. This script is that proof, and it is the only sanctioned
# way to answer the question.
#
# WHAT IT CHECKS (all required — any miss rejects)
# -----------------------------------------------
#   1. structural   — real, readable, executable regular file
#   2. behavioural  — answers `--version` with a parseable version
#   3. fork identity— the `HERDR_FORK_BUILD` guard is compiled in. We reuse the
#                     marker the audit already baked (.cargo/config.toml `[env]
#                     HERDR_FORK_BUILD force = true` → build_info::is_fork_build →
#                     update.rs FORK_BUILD_SELF_UPDATE_REJECTION). `option_env!`
#                     bakes the VALUE not the NAME, so the observable artifact of
#                     the marker is the refusal string that only exists on a build
#                     which compiled that guard. Upstream has no such string.
#   4. wasm bridge  — the host's guest-ABI call sites. `wasm_bridge.rs` looks these
#                     exports up BY NAME, so the literals are present iff the host
#                     can actually drive a guest module.
#   5. registry     — the `ui_modules` serde field, i.e. this host knows the fork
#                     plugins.json schema and will NOT strip it on rewrite.
#   6. runtime      — wasmtime is linked in.
#
# Presence-only, never counts: a count would break on any refactor. A build that
# loses ANY group is rejected, and an error anywhere in this script is also a
# rejection (`set -euo pipefail` + explicit trap) — the gate FAILS CLOSED.
#
# USAGE
#   herdr-bridge-check.sh <binary> [--json] [--quiet]
#
# EXIT CODES (only 0 means "safe to promote")
#    0  fork build with an intact bridge
#   10  usage / internal error
#   20  not a usable file (missing, unreadable, not executable, not Mach-O/ELF)
#   21  binary does not report a parseable --version
#   22  NOT a fork build (fork self-update guard absent) — e.g. upstream stable
#   23  fork marker present but one or more bridge/registry markers are missing
set -euo pipefail

# Any unexpected failure is a REJECTION, never an accidental success.
trap 'code=$?; [[ $code -eq 0 ]] || exit $((code == 1 ? 10 : code))' ERR

# ── marker groups ──────────────────────────────────────────────────────────
# Kept as presence requirements, not counts. `bridge-check-selftest.sh` asserts
# every literal below still exists in the fork source tree, so a rename breaks a
# test instead of silently disarming the gate.

# Group A — fork identity (audit guard 1). Observable artifact of HERDR_FORK_BUILD.
FORK_MARKERS=(
  "self-update is disabled for fork builds"
)

# Group B — host↔guest wasm ABI call sites (src/ui_plugins.rs wasm bridge) plus
# the host-side render entry point and capability type that gate them.
#
# Every literal here was checked against BOTH a healthy fork binary and the
# corrupt upstream 0.7.5: present in the first, absent in the second. That
# two-sided check is the whole discipline. `ui_plugin_api_bridge` reads like a
# plausible marker and was explicitly rejected — it is absent from hr's own
# binary and from the restored fork build, so including it would fail the gate
# CLOSED on healthy binaries, and a gate that blocks good builds is a gate that
# gets switched off.
BRIDGE_MARKERS=(
  "herdr_ui_api_version"
  "herdr_ui_alloc"
  "herdr_ui_on_event"
  "herdr_ui_static_update_ptr"
  "render_ui_plugin_layers"
  "UiCapability"
)

# Group C — fork plugin-registry schema. Its absence is precisely the binary that
# rewrites plugins.json without ui_modules (audit Finding 3).
REGISTRY_MARKERS=(
  "ui_modules"
)

# Group D — the generic ui_input_file transport the dashes render from.
TRANSPORT_MARKERS=(
  "HERDR_PLUGIN_UI_INPUT_JSON"
)

# Group E — the wasm runtime itself.
RUNTIME_MARKERS=(
  "wasmtime"
)

BIN=""
JSON=0
QUIET=0
for arg in "$@"; do
  case "$arg" in
    --json)  JSON=1 ;;
    --quiet) QUIET=1 ;;
    -*)      printf 'unknown flag: %s\n' "$arg" >&2; exit 10 ;;
    *)       [[ -n "$BIN" ]] && { printf 'only one binary may be checked\n' >&2; exit 10; }
             BIN="$arg" ;;
  esac
done
[[ -n "$BIN" ]] || { printf 'usage: %s <binary> [--json] [--quiet]\n' "$0" >&2; exit 10; }

say() { [[ $QUIET == 1 ]] && return 0; printf '%s\n' "$*" >&2; }

VERDICT="reject"
REASON=""
VERSION=""
SHA=""
MISSING=()

emit_and_exit() {
  local code="$1"
  if [[ $JSON == 1 ]]; then
    local miss_json="" first=1
    for m in ${MISSING+"${MISSING[@]}"}; do
      [[ $first == 1 ]] || miss_json+=","
      miss_json+="\"$m\""
      first=0
    done
    printf '{"ok":%s,"verdict":"%s","reason":"%s","binary":"%s","version":"%s","sha256":"%s","missing":[%s],"exit":%s}\n' \
      "$([[ $code == 0 ]] && echo true || echo false)" \
      "$VERDICT" "$REASON" "$BIN" "$VERSION" "$SHA" "$miss_json" "$code"
  fi
  exit "$code"
}

# ── 1. structural ──────────────────────────────────────────────────────────
if [[ ! -f "$BIN" || ! -r "$BIN" || ! -x "$BIN" ]]; then
  VERDICT="reject"; REASON="not a readable executable regular file"
  say "✗ reject: $BIN — $REASON"
  emit_and_exit 20
fi
# Reject a shell wrapper / script masquerading as the binary: we are checking the
# compiled artifact, and `strings` on a wrapper would find nothing anyway.
if ! head -c 4 "$BIN" | LC_ALL=C grep -qa $'\xcf\xfa\xed\xfe\|\xca\xfe\xba\xbe\|\x7fELF\|\xfe\xed\xfa\xcf' 2>/dev/null; then
  # Fall back to `file` — some Mach-O magics vary by arch/endianness.
  if ! file -b "$BIN" 2>/dev/null | LC_ALL=C grep -qiE 'mach-o|elf'; then
    VERDICT="reject"; REASON="not a native executable (Mach-O/ELF)"
    say "✗ reject: $BIN — $REASON"
    emit_and_exit 20
  fi
fi

SHA="$(shasum -a 256 "$BIN" 2>/dev/null | awk '{print $1}')" || SHA=""

# ── 2. behavioural ─────────────────────────────────────────────────────────
# The old (insufficient) gate. Kept as a necessary-but-not-sufficient first pass.
RAW_VER="$("$BIN" --version 2>/dev/null || true)"
VERSION="$(printf '%s' "$RAW_VER" | tr -d '\r' | awk '{print $NF}')"
if [[ -z "$VERSION" || ! "$VERSION" =~ ^v?[0-9]+\.[0-9]+ ]]; then
  VERDICT="reject"; REASON="binary does not report a parseable --version"
  say "✗ reject: $BIN — $REASON"
  emit_and_exit 21
fi

# ── 3-6. marker groups ─────────────────────────────────────────────────────
# Two-stage extraction, chosen for speed as much as correctness:
#   stage 1  reduce the ~28 MB Mach-O to its ~1.3 MB printable string table
#            (`strings`, or a `tr` fallback when binutils is unavailable);
#   stage 2  ONE `grep -oF` pass over that table listing WHICH markers occurred,
#            then set-compare.
# Two rejected alternatives, for the next reader: holding the table in a shell
# variable breaks (embedded control bytes make `[[ -n ]]`/`printf` unreliable
# under LC_ALL=C), and `grep -aoF` straight over the 28 MB binary takes minutes
# on BSD grep. Reduce first, then match.
ALL_MARKERS=(
  "${FORK_MARKERS[@]}" "${BRIDGE_MARKERS[@]}" "${REGISTRY_MARKERS[@]}"
  "${TRANSPORT_MARKERS[@]}" "${RUNTIME_MARKERS[@]}"
)

WORK_DIR="$(mktemp -d -t herdr-bridge-check)"
cleanup() { rm -rf "$WORK_DIR"; }
trap cleanup EXIT
STRTAB="$WORK_DIR/strtab"
PATFILE="$WORK_DIR/patterns"
FOUND_FILE="$WORK_DIR/found"

if command -v strings >/dev/null 2>&1; then
  strings -a "$BIN" >"$STRTAB" 2>/dev/null || : >"$STRTAB"
else
  LC_ALL=C tr -c '[:print:]' '\n' <"$BIN" >"$STRTAB" 2>/dev/null || : >"$STRTAB"
fi
# An empty string table means extraction failed → we cannot prove the bridge →
# reject. Fail closed, never "no markers found, must be fine".
if [[ ! -s "$STRTAB" ]]; then
  VERDICT="reject"; REASON="could not extract a string table from the binary"
  say "✗ reject: $BIN — $REASON"
  emit_and_exit 20
fi

printf '%s\n' "${ALL_MARKERS[@]}" >"$PATFILE"
LC_ALL=C grep -oFf "$PATFILE" "$STRTAB" 2>/dev/null | LC_ALL=C sort -u >"$FOUND_FILE" || true

has_marker() {
  LC_ALL=C grep -qxF -- "$1" "$FOUND_FILE"
}

# bash 3.2 (macOS system bash) has no namerefs, so markers are passed by value.
# Returns 0 when every marker in the group is present, 1 otherwise, and appends
# every miss to MISSING for the reason string.
check_group() {
  local ok=0 m
  for m in "$@"; do
    if has_marker "$m"; then :; else MISSING+=("$m"); ok=1; fi
  done
  return $ok
}

FORK_OK=0;   check_group "${FORK_MARKERS[@]}"      || FORK_OK=1
BRIDGE_OK=0; check_group "${BRIDGE_MARKERS[@]}"    || BRIDGE_OK=1
REG_OK=0;    check_group "${REGISTRY_MARKERS[@]}"  || REG_OK=1
TRANS_OK=0;  check_group "${TRANSPORT_MARKERS[@]}" || TRANS_OK=1
RUN_OK=0;    check_group "${RUNTIME_MARKERS[@]}"   || RUN_OK=1

if [[ $FORK_OK != 0 ]]; then
  # Distinguish "upstream binary" from "fork build that lost the bridge" so the
  # operator (or the escalation agent) gets an actionable reason.
  VERDICT="reject"
  REASON="not a fork build: the HERDR_FORK_BUILD self-update guard is absent (this looks like an upstream/official herdr $VERSION)"
  say "✗ reject: $BIN ($VERSION) — $REASON"
  say "  missing: ${MISSING[*]}"
  emit_and_exit 22
fi

if [[ $BRIDGE_OK != 0 || $REG_OK != 0 || $TRANS_OK != 0 || $RUN_OK != 0 ]]; then
  VERDICT="reject"
  REASON="fork build but the wasm UI-plugin bridge is incomplete"
  say "✗ reject: $BIN ($VERSION) — $REASON"
  say "  missing: ${MISSING[*]}"
  emit_and_exit 23
fi

VERDICT="accept"
REASON="fork build with an intact wasm UI-plugin bridge"
say "✓ accept: $BIN — herdr $VERSION, fork build, bridge intact"
emit_and_exit 0
