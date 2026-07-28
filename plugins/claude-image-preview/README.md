# Claude Image Preview

Renders Claude Code `[Image #N]` placeholders as previews inside the pane.

This plugin is the reason +127 exists. The same feature used to be host code in
`src/host_image.rs`: herdr itself knew that Claude keeps images in
`~/.claude/image-cache/<session>/<N>.png`, parsed `[Image #N]` out of the pane
grid, and painted the result. Removing it meant rebuilding herdr. Now:

```bash
herdr plugin disable builtin.claude-image-preview   # previews gone, pane clean
herdr plugin enable  builtin.claude-image-preview   # previews back
```

No rebuild, no restart. Everything herdr keeps is generic — an owner-tagged
graphics layer keyed by `(owner, layer_id)`, a `pane_graphics` capability, a
`graphics_roots` allowlist, and a compositor that has never heard of Claude.

## How it is put together

A wasm32 guest in herdr has **no WASI and no host imports**: it cannot open a
file, list a directory, or read a PNG header. So the work splits three ways.

| Part | Answers | Why there |
| --- | --- | --- |
| `collector/` (native process) | which `N` exist on disk, where, and how big | only it can touch the filesystem |
| `src/lib.rs` (wasm guest) | which `N` are on screen, at which cell, and how big the preview should be | only it sees the pane text and the per-frame event |
| herdr | reads the image bytes | only it can, and only from a declared `graphics_roots` directory |

Image **bytes never pass through the plugin**. The guest sends a `source_path`
and the host reads it, after canonicalising the path and checking it against
`graphics_roots` in this plugin's own manifest.

Each placeholder gets a stable `layer_id` of `img-<N>`. That is what lets image
2 scroll without disturbing image 1, and what lets a vanished placeholder clear
exactly its own layer rather than everything on the pane — including layers a
different plugin owns.

## Known limitation: horizontal placement after wide characters

**Columns are character indices, not display columns.**

The guest locates a placeholder by counting characters into the row text
returned by `pane.read`. For a row made of ordinary ASCII that is exactly the
terminal column. For a row containing a **wide character (CJK, emoji) or a
combining mark to the left of the `[Image #N]`**, it is not: the preview lands a
few cells to the left of where the placeholder actually sits.

What it never does: put the image in the wrong pane, on the wrong row, or on top
of an unrelated placeholder. The error is purely horizontal and bounded by the
number of wide characters preceding the placeholder on that one line.

**Why it is not fixed here.** Fixing it properly means asking herdr for cell
geometry rather than text — a new `pane.read { format: "cells" }` host surface.
+127 exists to *stop* growing host API for this feature, so the assumption ships
with a flag instead of a new endpoint.

**How you can tell.** The plugin's status line reads `images: 2 (~col)` whenever
any visible placeholder sits on a row where the assumption does not hold. No
`(~col)` means the placement is exact.

In practice Claude renders `[Image #N]` on its own line in ASCII, so the exact
case is the common one.

## Configuration

Everything is in `herdr-plugin.toml`:

- `graphics_roots = ["~/.claude/image-cache"]` — the only directory this plugin
  may name in a `source_path`. Visible in `herdr plugin list` before you enable
  it.
- `max_layers_per_pane = 8` — ceiling on retained layers. The guest enforces the
  same number, so it never trips the host limit and generates error traffic.
- `capabilities` — note there is **no `api_write`**. This plugin overlays only;
  it cannot edit pane text, which is why search, copy, and agent-resume keep
  reading the real output. Text mutation is a separate capability, deferred.

To point it at a different cache root, set `CLAUDE_IMAGE_CACHE_ROOT` for the
collector — and add the new root to `graphics_roots`, or the host will refuse
every paint with `source_not_allowed`. That refusal is the allowlist doing its
job, not a bug.

## Measuring what it costs

```bash
herdr plugin ui status --plugin builtin.claude-image-preview
```

reports `loaded`, `layer_count`, `layer_bytes`, `last_error`, and
`last_skip_reason` per module. For process cost use `footprint` against the
herdr pid, never `ps` RSS.

## Building

```bash
# guest
cargo build --target wasm32-unknown-unknown --release
# collector (root workspace member -> <repo>/target/release/)
cargo build --release -p claude-image-preview-collector
```

## Pinning a session

`CLAUDE_IMAGE_SESSION` forces the collector to read one session's cache
directory. herdr cannot always attribute a pane to an agent session — a resumed
or wrapper-launched Claude, or a pane driven by something herdr does not detect
— and without this the plugin is simply blind there. The focused session wins
when herdr does know it, so pinning never overrides a real attribution.

```bash
CLAUDE_IMAGE_SESSION=<session-uuid> herdr ...
```
