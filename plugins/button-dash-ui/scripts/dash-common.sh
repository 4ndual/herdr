# dash-common.sh — shared helpers for this plugin's action scripts.
# SOURCED, never executed (no shebang, not +x on purpose).
#
# It owns the things that were copy-pasted into every script and had
# already drifted apart:
#
#   1. PATH + binary resolution   -> $HERDR, hd(), find_exe()
#   2. user-visible messaging     -> toast()
#   3. the per-agent resume/fork matrix -> agent_launch_cmd()
#   3b. Remote Control on every claude launch -> rc_flags()
#
# The caller may set $LOG before sourcing; otherwise a per-plugin default is
# used. Sourcing defines log(), so callers no longer carry their own copy.
#
# NOTE: archive-dash-ui/scripts/dash-common.sh is a deliberate sibling copy.
# A plugin must work when it is the only one installed, so it cannot source a
# file out of another plugin's directory. Keep the two in sync — the agent
# matrix in §3 is the part that matters.

: "${LOG:=$HOME/.herdr-dash.log}"
UNAME_S="$(uname -s 2>/dev/null || echo unknown)"

log() { printf '%s %s\n' "$(date '+%H:%M:%S')" "$*" >>"$LOG" 2>/dev/null; }

# ── 1. PATH + binaries ───────────────────────────────────────────────────────
# A plugin action inherits the SERVER's env (src/app/api/plugins/runtime.rs
# never clears it), and that server may have been launched from a desktop
# session with a minimal PATH — no ~/.local/bin, no ~/.bun/bin, no
# /opt/homebrew/bin. Every agent CLI and jq lives in one of those. Append (never
# prepend: the user's own PATH order stays authoritative) the standard user and
# package-manager bin dirs that actually exist, so `command -v` answers the same
# question a login shell would. This replaces the per-script
# `for j in /opt/homebrew/bin/jq /usr/local/bin/jq ...` fallback chains.
for _d in "$HOME/.local/bin" "$HOME/.bun/bin" "$HOME/bin" /opt/homebrew/bin /usr/local/bin; do
  [ -d "$_d" ] || continue
  case ":$PATH:" in
    *":$_d:"*) ;;
    *) PATH="$PATH:$_d" ;;
  esac
done
unset _d
export PATH

# Resolve an executable for a command line we are about to run IN A PANE.
# Prints the bare name when it is on PATH — the pane's own login shell should
# resolve it, so a user's wrapper function/alias still wins — and an absolute
# path only when we had to find it ourselves. Non-zero (and prints nothing) when
# the tool is not installed, which is the signal to toast instead of spawning a
# pane that will just print "command not found".
find_exe() {
  if command -v "$1" >/dev/null 2>&1; then
    printf '%s' "$1"
    return 0
  fi
  for _c in "$HOME/.local/bin/$1" "$HOME/.bun/bin/$1" "$HOME/bin/$1" \
    "/opt/homebrew/bin/$1" "/usr/local/bin/$1"; do
    if [ -x "$_c" ]; then
      printf '%s' "$_c"
      return 0
    fi
  done
  return 1
}

# $HERDR_BIN_PATH is the RUNNING server's own executable, injected into every
# plugin command by the host (src/app/api/plugins/runtime.rs, from
# std::env::current_exe). It therefore speaks exactly the subcommands this
# server has, which a stale `herdr` shim on PATH famously did not. $HERDR_BIN
# overrides for manual runs; PATH is the last resort.
HERDR="${HERDR_BIN:-${HERDR_BIN_PATH:-$(command -v herdr 2>/dev/null)}}"
if [ -z "$HERDR" ] || [ ! -x "$HERDR" ]; then HERDR=""; fi
hd() { "$HERDR" "$@"; }

# ── 2. Messaging ─────────────────────────────────────────────────────────────
# Toast, and make sure it actually REACHES a human.
#
# The bug this fixes is NOT the argv (the old `${body:+--body "$body"}` form is
# fine — bash and dash both honour the quotes inside a `:+` expansion, verified
# with a printf argv dump). It is that a herdr toast is silent by default:
#
#   $ herdr notification show T --body "some body"
#   {"result":{"reason":"disabled","shown":false,…}}
#
# `[ui.toast] delivery` defaults to Off (ToastConfig::default,
# src/config/model.rs), and rc is still 0 — so every "…failed" message these
# scripts emit reached the log ONLY, on a stock config, while the script
# happily reported success. That is the exact silent-failure mode the buttons
# were supposed to have stopped having.
#
# So: check `shown`, and fall back to the desktop notifier the way
# handoff-herdr.sh already does. The argv is spelled out as an if/else purely
# because it reads clearer than the conditional expansion.
toast() {
  _t_title="$1"
  _t_body="${2:-}"
  _t_out=""
  log "TOAST: $_t_title — $_t_body"
  if [ -n "$HERDR" ]; then
    if [ -n "$_t_body" ]; then
      _t_out="$("$HERDR" notification show "$_t_title" --body "$_t_body" 2>&1)"
    else
      _t_out="$("$HERDR" notification show "$_t_title" 2>&1)"
    fi
    printf '%s\n' "$_t_out" >>"$LOG" 2>/dev/null
    case "$_t_out" in
      *'"shown":true'* | *'"shown": true'*) return 0 ;;
    esac
    log "herdr toast not shown (delivery off/busy) — falling back to the desktop notifier"
  fi
  desktop_notify "$_t_title" "$_t_body"
}

desktop_notify() {
  case "$UNAME_S" in
    Darwin)
      if command -v osascript >/dev/null 2>&1; then
        # AppleScript string literals: escape backslashes first, then quotes.
        _n_t="$(printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')"
        _n_b="$(printf '%s' "$2" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')"
        osascript -e "display notification \"$_n_b\" with title \"$_n_t\"" >>"$LOG" 2>&1 ||
          log "WARN: osascript fallback failed — this message reached the log ONLY"
      else
        log "WARN: no osascript — this message reached the log ONLY"
      fi
      ;;
    *)
      if command -v notify-send >/dev/null 2>&1; then
        notify-send -a herdr -- "$1" "$2" >>"$LOG" 2>&1 ||
          log "WARN: notify-send fallback failed — this message reached the log ONLY"
      else
        log "WARN: no notify-send on PATH — this message reached the log ONLY"
      fi
      ;;
  esac
}

# ── 3. The per-agent resume/fork matrix ──────────────────────────────────────
# agent_launch_cmd <resume|fork> <agent> <session-value> [skip|noskip] [rc-name]
#
# On success: prints the exact command line to `exec` in a fresh pane, exit 0.
# On refusal: prints a human-readable REASON, exit 1. Callers must therefore
# capture stdout once and use it either as the command or as the toast body:
#
#     if ! cmd="$(agent_launch_cmd fork "$agent" "$sid")"; then
#       toast "Fork not available" "$cmd"; exit 7
#     fi
#
# (The obvious alternative — stashing the reason in a global — cannot work:
# `$(…)` runs in a subshell, so the assignment dies with it. That bug shipped
# once already, and every refusal toasted with an empty body.)
#
# Every argv here was verified against the real binaries (2026-08-12; the skip
# column re-verified against claude and codex-cli 0.147.0 on 2026-08-13):
#
#   agent  | resume                  | fork                                | skip flag
#   -------+-------------------------+-------------------------------------+--------------------------------------------
#   claude | claude --resume <id>    | claude --resume <id> --fork-session | --dangerously-skip-permissions
#   codex  | codex resume <id>       | codex fork <id>  (real subcommand)   | --dangerously-bypass-approvals-and-sandbox
#   pi     | pi --session <id>       | pi --fork <id>                      | (none — `pi --help` has no yolo/bypass flag)
#   jcode  | jcode --resume <id>     | (none — refused, not guessed)       | (not installed here — nothing verified)
#
# The 4th argument selects permission mode and DEFAULTS TO `skip`. That default
# is deliberate: every one of these launches is a continuation of a conversation
# the user already trusted enough to run, re-prompting for each tool call in the
# forked/restored copy is pure friction, and it matches the agent-launcher popup,
# whose permission dropdown is also bypass-by-default. Pass `noskip` to opt out.
# `--dangerously-bypass-approvals-and-sandbox` is accepted by `codex resume` and
# `codex fork` themselves (verified with `codex resume --help`), so it goes AFTER
# the subcommand, not before it. pi and jcode have no verified equivalent and are
# emitted unchanged rather than given a guessed flag.
#
# The 5th argument names the Remote Control session prefix (see §3b) and is
# claude-only. Callers pass the most meaningful label they hold — the fork's tab
# name, the archived chip's label — because that is what the human will be
# looking at in the Claude app. With none given it falls back to the fleet
# harness that owns the session, which is the only context this function has.
#
# Two traps encoded here on purpose:
#
#   * `pi --resume <id>` is NOT a resume-by-id. `-r/--resume` opens pi's
#     INTERACTIVE picker and takes no argument, so passing an id there either
#     drops it or misparses it. `--session <path|id>` is the by-id form and it
#     accepts a partial uuid.
#   * jcode has no fork flag. Resuming the same session in a second pane is not
#     a fork — it is two views of one conversation, with two processes writing
#     to it. Refusing is the honest answer; the caller toasts it.
#
# Agents outside this table (omp, gemini, cursor, a plain terminal…) are refused
# rather than routed through a neighbour's flags. `omp` in particular used to
# share pi's branch, which emitted `pi --fork` for an omp pane.
#
# ── The harness env prefix ───────────────────────────────────────────────────
# A session is only resumable from the config root that OWNS its transcript.
# Fleet harnesses (~/harnesses/<name>/config, runtime declared in
# ~/harnesses/<name>/harness.json) are exactly such roots: they are a
# CLAUDE_CONFIG_DIR / CODEX_HOME pointing somewhere other than ~/.claude and
# ~/.codex. Emitting a bare `claude --resume <id>` for a harness-owned session
# therefore fails — claude looks in the default root, does not find the
# transcript, and starts a NEW conversation instead. That silently-wrong fork
# shipped for as long as this matrix has existed.
#
# So: locate the transcript, and when it lives under a harness root, prefix the
# command with the env var that points the runtime back at it. `env` is used
# rather than a bare `VAR=x cmd` assignment because the emitted string is also
# passed to `herdr pane run "exec $CMD"` — `exec VAR=x cmd` is a syntax error in
# every POSIX shell, while `exec env VAR=x cmd` is not.
#
# Lookup is glob-only (no `find`): claude names one file per session id, and a
# codex rollout filename ends in the uuid, so both are a single glob per root.
#
# harness_owner <agent> <session-value>
#   -> two lines, the owning harness NAME then its CONFIG DIR; nothing at all
#      when the session lives under the default root. Always exits 0.
#
# Two lines rather than one field: a harness directory name is user-chosen and a
# single space-joined line could not be split back apart safely.
harness_owner() {
  _h_agent="$1"
  _h_value="$2"
  [ -n "$_h_value" ] || return 0
  case "$_h_agent" in
    claude | codex) ;;
    *) return 0 ;;
  esac
  [ -d "$HOME/harnesses" ] || return 0

  for _h_dir in "$HOME"/harnesses/*/; do
    [ -d "$_h_dir/config" ] || continue
    # harness.json is the authoritative runtime declaration; a harness with a
    # different runtime cannot own this session, and one with none is skipped
    # rather than assumed (several exist: main, vanilla, mac-main, …).
    _h_rt=""
    if [ -r "$_h_dir/harness.json" ] && command -v jq >/dev/null 2>&1; then
      _h_rt="$(jq -r '.runtime // empty' <"$_h_dir/harness.json" 2>/dev/null)"
    fi
    [ "$_h_rt" = "$_h_agent" ] || continue

    _h_root="${_h_dir%/}"
    case "$_h_agent" in
      claude)
        for _h_f in "$_h_dir"config/projects/*/"$_h_value".jsonl; do
          [ -f "$_h_f" ] || continue
          printf '%s\n%s\n' "${_h_root##*/}" "$_h_root/config"
          return 0
        done
        ;;
      codex)
        for _h_f in "$_h_dir"config/sessions/*/*/*/rollout-*-"$_h_value".jsonl; do
          [ -f "$_h_f" ] || continue
          printf '%s\n%s\n' "${_h_root##*/}" "$_h_root/config"
          return 0
        done
        ;;
    esac
  done
  return 0
}

# harness_env_prefix <agent> <session-value> -> "env VAR=/path " or "" (always 0)
harness_env_prefix() {
  _hp_dir="$(harness_owner "$1" "$2" | tail -n +2)"
  [ -n "$_hp_dir" ] || return 0
  case "$1" in
    claude) printf 'env CLAUDE_CONFIG_DIR=%q ' "$_hp_dir" ;;
    codex) printf 'env CODEX_HOME=%q ' "$_hp_dir" ;;
  esac
  return 0
}

# harness_name <agent> <session-value> -> "<harness>" or "" (always 0)
harness_name() { harness_owner "$1" "$2" | head -1; }

# ── 3b. Remote Control on every claude launch ────────────────────────────────
# Every claude session a plugin starts is Remote-Control-enabled by default, so
# it can be picked up from the phone / claude.ai without first walking over to
# the machine and typing `/remote-control` into the live REPL (which is all
# remote-control.sh can do — that button acts on an ALREADY RUNNING session and
# is unrelated to this).
#
# Two escape hatches, because a flag that cannot be turned off breaks something
# eventually:
#
#   HARNESS_RC=off   opt out, honoured at every call site. Test rigs set it so a
#                    CI/e2e launch never registers a Remote Control session.
#   version guard    a claude too old to know the flags gets none of them. The
#                    launch must never die on an unknown option.
#
# ORDERING IS LOAD-BEARING. `--remote-control [name]` takes an OPTIONAL value,
# so the argv word after it is swallowed as the session name unless it starts
# with `-`. Emitting `--remote-control-session-name-prefix` IMMEDIATELY after it
# guarantees that, which is what keeps a trailing positional prompt (runtime-swap
# hands claude its handoff text that way) out of the session name. Never split
# the pair, and never put `--remote-control` last.

# Memoized per process: "yes" | "no" | "" (not probed yet).
RC_SUPPORT=""

_rc_cache_file() { printf '%s/herdr/remote-control-support' "${XDG_CACHE_HOME:-$HOME/.cache}"; }

# `claude --help` costs ~0.5s warm and ~3s cold — too much to pay on every
# button press — so the answer is cached on disk, keyed on the binary's own
# size+mtime so that `claude update` re-probes by itself.
rc_probe() {
  RC_SUPPORT=no
  _rc_exe="$(command -v claude 2>/dev/null)"
  [ -n "$_rc_exe" ] || _rc_exe="$(find_exe claude)" || return 0
  [ -x "$_rc_exe" ] || return 0

  # `ls -Ln`, not stat(1): its flags are incompatible between macOS and Linux.
  _rc_stamp="$(ls -Ln "$_rc_exe" 2>/dev/null | awk '{ print $5 "-" $6 "-" $7 "-" $8 }')"
  _rc_file="$(_rc_cache_file)"
  if [ -n "$_rc_stamp" ] && [ -r "$_rc_file" ]; then
    _rc_line="$(cat "$_rc_file" 2>/dev/null)"
    case "$_rc_line" in
      "$_rc_stamp yes") RC_SUPPORT=yes; return 0 ;;
      "$_rc_stamp no") RC_SUPPORT=no; return 0 ;;
    esac
  fi

  if "$_rc_exe" --help 2>/dev/null | grep -q -- '--remote-control-session-name-prefix'; then
    RC_SUPPORT=yes
  fi
  if [ -n "$_rc_stamp" ]; then
    mkdir -p "${_rc_file%/*}" 2>/dev/null
    printf '%s %s\n' "$_rc_stamp" "$RC_SUPPORT" >"$_rc_file" 2>/dev/null || true
  fi
  return 0
}

rc_supported() {
  [ -n "$RC_SUPPORT" ] || rc_probe
  [ "$RC_SUPPORT" = "yes" ]
}

# A Remote Control session name is shown in the Claude app, so keep it to the
# portable identifier charset and a glanceable length. An empty or fully-stripped
# name degrades to "herdr" rather than emitting an empty prefix.
rc_name() {
  _rn="$(printf '%s' "${1:-}" | LC_ALL=C tr -c 'A-Za-z0-9._-' '-' |
    LC_ALL=C sed -e 's/--*/-/g' -e 's/^-//' -e 's/-$//' | cut -c1-40)"
  [ -n "$_rn" ] || _rn="herdr"
  printf '%s' "$_rn"
}

# rc_flags [name] -> " --remote-control --remote-control-session-name-prefix <n>"
# (leading space, shell-quoted, ready to append) or "". Always exits 0.
rc_flags() {
  [ "${HARNESS_RC:-}" = "off" ] && return 0
  rc_supported || return 0
  printf ' --remote-control --remote-control-session-name-prefix %q' "$(rc_name "${1:-}")"
  return 0
}

# claude_roots -> every Claude Code config root on this machine, one per line.
#
# The default root FIRST, then each fleet harness root (~/harnesses/<name>/config
# whose harness.json declares runtime "claude"). Callers that look up a
# transcript must walk all of them: a harness pane's transcript has never lived
# under ~/.claude, so a single-root lookup silently reports "no session" for
# exactly the panes that most need recovering. Same defect the host carried in
# `claude_transcript_base` until #36.
#
# Emits absolute paths; roots that do not exist are skipped, so the output can be
# consumed directly without existence checks.
claude_roots() {
  _cr_default="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
  [ -d "$_cr_default" ] && printf '%s\n' "$_cr_default"

  [ -d "$HOME/harnesses" ] || return 0
  for _cr_dir in "$HOME"/harnesses/*/; do
    [ -d "$_cr_dir/config" ] || continue
    [ "$_cr_dir/config" = "$_cr_default" ] && continue
    _cr_rt=""
    if [ -r "$_cr_dir/harness.json" ] && command -v jq >/dev/null 2>&1; then
      _cr_rt="$(jq -r '.runtime // empty' <"$_cr_dir/harness.json" 2>/dev/null)"
    fi
    [ "$_cr_rt" = "claude" ] || continue
    printf '%s\n' "${_cr_dir%/}/config"
  done
  return 0
}

agent_launch_cmd() {
  _a_verb="$1"
  _a_agent="$2"
  _a_value="$3"
  _a_mode="${4:-skip}"
  _a_rc_name="${5:-}"

  # Legacy archive records and display skins can carry "cx" — a cosmetic label
  # for Claude Code driven through a gateway, never a distinct runtime.
  [ "$_a_agent" = "cx" ] && _a_agent="claude"

  case "$_a_agent" in
    claude | codex | pi | jcode) ;;
    "")
      printf 'this pane has no agent running'
      return 1
      ;;
    *)
      printf "'%s' is not one of claude, codex, pi or jcode" "$_a_agent"
      return 1
      ;;
  esac

  if [ "$_a_verb" = "fork" ] && [ "$_a_agent" = "jcode" ]; then
    # Deliberately NOT falling back to `jcode --resume`: a second process on the
    # same session is two views of one conversation, not a branch, and calling
    # that "fork" would quietly corrupt the thing the user meant to protect.
    printf 'fork is not supported for jcode — it has no fork flag'
    return 1
  fi

  # Refuse before spawning: a pane that opens only to print "command not found"
  # is worse than a toast, because the failure scrolls away with the pane.
  if ! _a_exe="$(find_exe "$_a_agent")"; then
    printf '%s is not installed on this machine' "$_a_agent"
    return 1
  fi

  # Per-runtime skip flag, empty when the mode is off or the runtime has none.
  _a_skip=""
  if [ "$_a_mode" = "skip" ]; then
    case "$_a_agent" in
      claude) _a_skip=" --dangerously-skip-permissions" ;;
      codex) _a_skip=" --dangerously-bypass-approvals-and-sandbox" ;;
    esac
  fi

  # Remote Control, claude only — codex/pi/jcode have no such feature and must
  # be emitted exactly as before.
  _a_rc=""
  if [ "$_a_agent" = "claude" ]; then
    [ -n "$_a_rc_name" ] || _a_rc_name="$(harness_name claude "$_a_value")"
    _a_rc="$(rc_flags "$_a_rc_name")"
  fi

  # Harness-owned sessions need their config root back (see harness_env_prefix).
  # Captured into a variable rather than printed here: the refusal arm below
  # prints a human-readable REASON on the same stdout, and a stray `env …`
  # prefix in front of it would be shown to the user as the toast body.
  _a_pfx="$(harness_env_prefix "$_a_agent" "$_a_value")"

  case "$_a_verb:$_a_agent" in
    resume:claude) printf '%s%s --resume %q%s%s' "$_a_pfx" "$_a_exe" "$_a_value" "$_a_skip" "$_a_rc" ;;
    resume:codex) printf '%s%s resume %q%s' "$_a_pfx" "$_a_exe" "$_a_value" "$_a_skip" ;;
    resume:pi) printf '%s --session %q' "$_a_exe" "$_a_value" ;;
    resume:jcode) printf '%s --resume %q' "$_a_exe" "$_a_value" ;;
    fork:claude) printf '%s%s --resume %q --fork-session%s%s' "$_a_pfx" "$_a_exe" "$_a_value" "$_a_skip" "$_a_rc" ;;
    fork:codex) printf '%s%s fork %q%s' "$_a_pfx" "$_a_exe" "$_a_value" "$_a_skip" ;;
    fork:pi) printf '%s --fork %q' "$_a_exe" "$_a_value" ;;
    *)
      printf "%s is not supported for '%s'" "$_a_verb" "$_a_agent"
      return 1
      ;;
  esac
}

# ── 4. Instance / process resolution ─────────────────────────────────────────
# Used by the restart family to answer "which process is THIS instance's server,
# and which binary is it running". Lifted from handoff-herdr.sh:53-103, which
# already solved both on Linux and documents why; the restart pair still carried
# the pre-fix versions and was dead on Linux because of it.
#
# (handoff keeps its own private copies rather than sourcing this file. That is
# deliberate for now: it is the reference implementation, it was just verified
# end-to-end on Linux, and re-pointing it at a shared file is a change to a
# working button with nothing to gain today.)

# ABSOLUTE path of the executable a pid is running.
#
# `ps -o comm=` is correct ONLY on macOS, where `comm` is the full executable
# path. On Linux `comm` is the bare, 15-char-truncated process NAME — measured
# here: pid 6370 reports `herdr`, which is not a path, is not executable, and so
# failed every `-f`/`-x` gate downstream and left the relaunch pointing at a name
# that resolves to nothing (or worse, to a stale shim on PATH).
#
# On Linux /proc/<pid>/exe is ground truth. When the binary has been replaced
# under the running server the kernel appends " (deleted)"; stripping that yields
# the path of the NEW on-disk file, which is the image we want to relaunch.
exe_path() {
  _e_pid="$1"
  _e_p=""
  if [ -n "$_e_pid" ] && [ -r "/proc/$_e_pid/exe" ]; then
    _e_p="$(readlink "/proc/$_e_pid/exe" 2>/dev/null)"
    _e_p="${_e_p% (deleted)}"
  fi
  if [ -z "$_e_p" ] && [ -n "$_e_pid" ]; then
    # macOS (and any Linux where /proc is unreadable): `comm` is the full path.
    _e_p="$(ps -o comm= -p "$_e_pid" 2>/dev/null | sed 's/ server$//')"
  fi
  printf '%s' "$_e_p"
}

# Every pid holding a unix socket, one per line.
#
# `lsof -t <sock>` is the wrong tool here even when it is not polluted: it lists
# every process holding the socket, and the herdr server's CLIENTS hold it too
# (measured on the live `hr` instance: the server holds one LISTEN fd and four
# CONNECTED fds, and any in-flight `herdr pane list` adds its own). Matching the
# NAME field keeps the answer to processes actually holding THIS path.
socket_pids() {
  command -v lsof >/dev/null 2>&1 || return 0
  _sp_out="$(lsof -- "$1" 2>/dev/null)" || true
  [ -n "$_sp_out" ] || return 0
  printf '%s\n' "$_sp_out" |
    awk -v s="$1" '{ for (i = 1; i <= NF; i++) if ($i == s) { print $2; break } }' |
    sort -u
}

# Pid of the process LISTENING on a unix socket — i.e. the server, not one of
# its clients. `lsof -t <sock> | head -1` returned whichever pid sorted first,
# which is a coin flip between the server and a connected client.
socket_pid() {
  command -v lsof >/dev/null 2>&1 || return 0
  _s_out="$(lsof -- "$1" 2>/dev/null)" || true
  [ -n "$_s_out" ] || return 0
  printf '%s\n' "$_s_out" |
    awk -v s="$1" '
        { for (i = 1; i <= NF; i++) if ($i == s) {
            if (index($0, "LISTEN")) { listen = $2 } else if (!any) { any = $2 }
            break
          } }
        END { if (listen) print listen; else if (any) print any }'
}

# The herdr binary to relaunch an instance with, most authoritative first:
#   1. the live server's own image (instance-correct, survives a replaced file)
#   2. $HERDR_BIN_PATH — the server that spawned this action, injected by the
#      host; correct for the in-app button path even after the server is dead
#   3. PATH, as a last resort
# The old code fell back to a hardcoded $HOME/Projects/herdr/target/release/herdr
# that exists on exactly one machine — and on Linux never even got there, because
# `ps -o comm=` returned a non-empty non-path and looked like success.
instance_binary() {
  _ib_pid="${1:-}"
  _ib_bin=""
  [ -n "$_ib_pid" ] && _ib_bin="$(exe_path "$_ib_pid")"
  if [ -z "$_ib_bin" ] || [ ! -x "$_ib_bin" ]; then
    _ib_bin="${HERDR_BIN_PATH:-}"
  fi
  if [ -z "$_ib_bin" ] || [ ! -x "$_ib_bin" ]; then
    _ib_bin="$(command -v herdr 2>/dev/null || true)"
  fi
  [ -n "$_ib_bin" ] && [ -x "$_ib_bin" ] || return 0
  printf '%s' "$_ib_bin"
}

# ── 5. Pane resolution ───────────────────────────────────────────────────────
# The pane a button acts on. HERDR_PANE_ID is injected only when the invocation
# carries a focused pane; a TAB-menu invocation explicitly nulls it
# (src/app/api/plugins/mod.rs, invoke_plugin_action_from_ui_surface), so the
# focused-pane lookup is not a nicety — it is the only path that works from the
# tab menu. Prints the pane id, or nothing and returns non-zero.
current_pane() {
  if [ -n "${HERDR_PANE_ID:-}" ] && [ "${HERDR_PANE_ID}" != "null" ]; then
    printf '%s' "$HERDR_PANE_ID"
    return 0
  fi
  _p="$(hd pane list 2>/dev/null | jq -r '.result.panes[] | select(.focused) | .pane_id' | head -1)"
  if [ -z "$_p" ] || [ "$_p" = "null" ]; then
    return 1
  fi
  printf '%s' "$_p"
}
