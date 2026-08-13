# runtime-swap

Reopen the focused pane's live agent session under a **different** runtime
(`claude`, `codex`, `pi`, `jcode`), replacing that pane **in place** — same tab,
same folder, conversation carried over.

No herdr core changes: everything here rides on two primitives the shipped
binary already has — the pre-0.7.5 `herdr agent start … -- <argv>` form (wire
`agent.spawn`) and `herdr plugin pane open --placement overlay`.

## Using it

- Action `reopen-in-runtime` (contexts: tab, pane) opens the picker overlay:

  ```bash
  herdr plugin action invoke reopen-in-runtime --plugin builtin.runtime-swap
  ```

- Or drive the engine directly, which is also how it is tested:

  ```bash
  bun scripts/swap-runtime.ts --to codex [--pane w1:p3] [--dry-run] [--json]
  ```

`--dry-run` prints the resolved session, transcript, strategy and argv without
touching any pane. It does still perform codex's native import when that
strategy applies, because that is what proves the strategy is available; the
import is deduped by content hash, so repeating it is idempotent.

## How the conversation moves

Following jcode (`crates/jcode-base/src/import.rs`), this is **one-way import**,
never format translation. Strategy is picked per source/target pair:

| pair | strategy | what happens |
| --- | --- | --- |
| claude (default root) → codex | `native:codex-import` | codex's own app-server importer (`externalAgentConfig/import`) materializes a real rollout; the pane runs `codex resume <thread>` — a true native resume |
| anything → jcode | `native:jcode-import` | `jcode --resume <foreign-id>`; jcode probes codex → claude → opencode → cursor → pi and imports it itself |
| everything else | `import:flattened-handoff` | we parse the transcript ourselves and hand the target a flattened history as its first prompt |

The flattened handoff applies jcode's own normalization rules and caps:

- reasoning / thinking blocks are **dropped** — provider-native reasoning cannot
  safely be replayed under a different provider;
- tool traffic becomes `[tool call: name]` / `[tool result: id]` summaries;
- 160 messages, 256 KiB total, 32 KiB per block, kept **from the tail**, with an
  explicit note when older turns were omitted;
- the full flattened history is always written to
  `$XDG_RUNTIME_DIR/herdr-runtime-swap/…md` and referenced in the prompt, so the
  agent can read what did not fit inline (jcode's `session_search` escape hatch,
  done with a file). Only ~96 KiB is inlined into argv.

Transcripts are searched across **every** config root — `CLAUDE_CONFIG_DIR` /
`CODEX_HOME`, the defaults, and every `~/harnesses/*/config` — not just the
default one.

## Placement

```
herdr agent start <label> --tab <TAB> --split right --cwd <CWD> [--env ROOT=…] -- <argv…>
herdr pane close <old pane>
```

Spawn happens **before** the close: closing a tab's only pane would destroy the
tab. The new pane is a real agent pane running argv directly, so herdr's
detector labels it and the pane dies with the agent.

## Things it does to your machine

Both are needed because a reopened session must land at a live prompt rather
than behind a modal that swallows the handoff, and both only ever apply to the
cwd an agent was **already** running in:

- **claude**: sets `projects[<cwd>].hasTrustDialogAccepted = true` in
  `<CLAUDE_CONFIG_DIR|~>/.claude.json`. claude has no CLI flag for the
  workspace-trust dialog. Existing entries are never rewritten; a `.bak` is
  written once.
- **codex**: appends `[projects."<cwd>"] trust_level = "trusted"` to
  `$CODEX_HOME/config.toml`, and passes `--dangerously-bypass-hook-trust`.
  Note: `-c projects."<cwd>".trust_level="trusted"` parses and even survives
  `--strict-config`, but does **not** suppress the dialog — only the persisted
  key does.

Target runtimes launch in skip/bypass mode
(`--dangerously-skip-permissions`, `--dangerously-bypass-approvals-and-sandbox`,
`pi --approve`), which is the point of the feature.

## Known limits

- **codex's native importer only detects `~/.claude/projects`.** A claude session
  living in a harness root (`CLAUDE_CONFIG_DIR=~/harnesses/<h>/config`) is
  rejected with `session_not_detected`, so those fall back to the flattened
  handoff. Setting `CLAUDE_CONFIG_DIR` for the importer does not help.
- herdr only reports an `agent_session` when the runtime's integration hook does.
  When it is absent the engine recovers the newest transcript this runtime wrote
  for the pane's cwd, matching on the transcript header, and derives the id from
  the filename.
- Config roots are read from the source pane's own process environment
  (`/proc/<pid>/environ`, Linux only) so a harness-scoped pane reopens inside the
  same harness. On other platforms the swap falls back to its own environment.

## Layout

```
herdr-plugin.toml          overlay pane `picker` + action `reopen-in-runtime`
scripts/open-picker.sh     opens the overlay
scripts/swap-runtime.ts    the engine: discovery, import, placement (bun + ts)
tui/                       the overlay itself (ratatui, mouse + keyboard)
```

Iteration: `cd tui && cargo build --release`, then reopen the overlay — each open
spawns a fresh process, so no herdr restart. Engine edits need no build at all.
Manifest edits need `herdr plugin unlink <path> && herdr plugin link <path>`.
