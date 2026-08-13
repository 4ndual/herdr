# Harness Indicator

One bottom row saying which **fleet harness** the focused pane is running
inside, which agent herdr detects there, and where that agent is working:

```
⛺ e2e-claude · claude · ~/harnesses/e2e-claude/workspace
⛺ e2e-codex ⚠ codex≠claude · claude · ~/harnesses/e2e-codex/workspace
⛺ no harness · claude · ~/Projects/herdr
⛺ e2e-claude · claude · ~/harnesses/e2e-claude/workspace → ~/Projects/herdr
⛺ shell · ~/Projects/herdr
```

Left-click the row to toast the full attribution, including the config dir and
the evidence it came from.

> "Harness" here means a **fleet harness** — a directory under `$HARNESS_ROOT`
> (`~/harnesses/<name>`) holding an isolated agent config. herdr uses the same
> word elsewhere for "which agent CLI" (`src/harness_host.rs`,
> `crates/harness-manager-model`). The two are unrelated.

## Why

Once a runtime has painted the screen, a harness pane and an ordinary pane look
identical. Nothing in herdr says which isolated config an agent is reading, so
"am I in the harness I think I am" gets answered by reading a tab label someone
typed. This strip answers it from the running process's own environment.

The `⚠` case is the one that earns the row: a pane whose harness declares
`codex` while the pane actually runs `claude` is reading one harness's config
with another's runtime. It is silent today, and it is exactly the shape of
mistake a fleet makes at 2am.

## Architecture

| piece | what it does |
| --- | --- |
| `collector/` | native binary, run by the host each dash tick, writes `harness_input.json` into `HERDR_PLUGIN_STATE_DIR` |
| `model/` | wasm-safe serde model + the classification both sides share |
| `src/` | wasm guest: reads the injected snapshot, renders one line, owns the click |

The collector is declared in `herdr-plugin.toml` and launched by
`App::tick_plugin_collectors`, the same generic path usage-dash and version-dash
use. It is **not** throttled: the snapshot describes the pane you are looking at
right now, so staleness after a focus change would be worse than useless. It
stays cheap enough to earn that — measured **6 ms** per run: two local socket
calls (`pane.list`, `pane.process_info`), a few `/proc` reads, and one scan of
`~/harnesses/*/harness.json`. No network, no unbounded walks.

Both artefacts build from this directory:

```bash
cargo build --target wasm32-unknown-unknown --release -p harness-indicator-ui
cargo build --release -p harness-indicator-collector
```

`hot_reload = "on"`, so a rebuilt `.wasm` is picked up on the next tick with no
restart. A rebuilt collector is picked up on the next tick too — it is re-exec'd
every time.

## How each fact is established

**Which harness** — the runtime process's own environment
(`/proc/<pid>/environ` → `CODEX_HOME`, `CLAUDE_CONFIG_DIR`, `JCODE_HOME`),
canonicalised and matched against every `~/harnesses/*/harness.json` config dir.
This is proof rather than inference: it is the config the process is actually
reading. If that fails, the focused session's transcript is looked for under the
harness roots (claude's `projects/<encoded-cwd>/<uuid>.jsonl` by direct stat,
codex's `sessions/YYYY/MM/DD/rollout-*` in the two most recent day directories).
A transcript-derived name renders with a `≈` prefix, because a weaker claim must
not look identical to a stronger one.

A harness whose config dir **is** a runtime's default root (`~/.claude`) is
never reported — the `main` profile points there, and labelling every ordinary
pane with it would erase the exact distinction this strip exists to draw.

**Which agent** — herdr's own detection (`PaneInfo.agent`, falling back to
`HERDR_FOCUS_JSON`). The plugin never sniffs the screen itself.

**Which folder** — and this is the part where the obvious answer is wrong.

herdr keeps **no record of a pane's launch cwd**. Both `PaneInfo.cwd` and
`PaneInfo.foreground_cwd` are LIVE process cwds (`src/pane.rs:2859` and the
`cwd_for_pane` path), so a `cd` inside the pane moves both. Verified directly:
after `cd` in a shell pane, `pane list` reported the new directory in both
fields. So the launch folder is not available from herdr at all.

What *is* available is better, and truer to the question:

- **`agent_cwd`** — the runtime process's own cwd, from `/proc/<pid>/cwd`,
  picking the foreground process whose name is a runtime. A runtime does not
  chdir after launch, so this is where the agent was started AND where its work
  happens, and it does not move when a shell does.
- **`shell_cwd`** — the pane shell's cwd (`PaneInfo.cwd`), i.e. where your
  prompt is.

The strip shows the agent's folder, and appends `→ <shell folder>` only when
they differ — `(cd x && claude)`, or a shell that moved after launch. Two facts
that are usually equal and occasionally not are worth one arrow between them;
collapsing them into a single "cwd" is how you end up confidently pointing at
the wrong directory.

## What it does not do

No writes, no actions, no API calls from the guest. Right-click is deliberately
inert: a reporting surface with a destructive second gesture is how a glance
becomes an accident.

## Placement note

`min_peer = 5`, not the schema default of 24. For a top/bottom band `min_peer`
counts ROWS (`src/ui.rs`: `max_slot = height - min_peer`), so the default
silently drops a one-row strip on any pane area 24 rows or shorter — the bug
that kept version-dash invisible.

The `rail_icon` field is not wired up in the current host, so this plugin has no
rail presence; it does not need one, being a bottom band rather than a right
panel.
