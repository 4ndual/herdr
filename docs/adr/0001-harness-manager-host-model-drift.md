# ADR 0001 — Resolve the harness-manager host/guest model drift by adopting the plugin's reconciled collector

Status: accepted
Date: 2026-08-05
Branch: `task/linux-compat` (from `752d715d`, the accept-080 / upstream v0.8.0 merge)
Tracks: task #52, campaign task #144 (hr plugins Linux E2E, wave 1)

## Context

`harness-manager` renders three tabs: `1 session`, `2 profile`, `3 profiles`. Tabs 2 and 3
show a profile's *global* harness configuration — what a profile configures profile-wide,
independent of any live session — and the picker that selects between the daily `~/.claude`
profile, each `~/.claude-code-router/profiles/<name>` (ccr), and each
`~/claude-hub/profiles/<name>` (ccp).

The guest can render all three. The host cannot feed two of them.

Two crates carry the contract, and they had drifted apart:

| | `crates/harness-manager-model` (host) | `vendor/harness-manager-model` (guest) |
|---|---|---|
| lines at `752d715d` | 205 | 277 |
| `ProfileRef`, `ProfileView`, `ResourceCatalogEntry` | absent | present |
| `HarnessRenderModel.profiles` / `.profile_view` / `.selected_profile` / `.selected_view` / `.catalog` | absent | present |

The host's collector (`src/harness_host.rs`) therefore never *emits* those fields, and the
guest — which does route `Tab::Profile` and `Tab::Profiles` — receives them as the serde
defaults: empty vec, `None`. The result is the failure mode this ADR exists to kill:

**tabs 2 and 3 render their empty-state strings on a plugin that is otherwise working.**
Not a crash, not a WARN, not a missing layer. The user clicks `2 profile` and reads
"no profile config found for the active profile" on a machine that has three profiles.

Every new field is `#[serde(default)]`, which is why the drift is silent in both
directions: the old host's JSON still deserialises against the new guest, and vice versa.
Backward compatibility bought us a bug that cannot announce itself.

### Why the host crate is the one that lags

This is not an accident of one commit; it is structural, and the ADR must say so or the
same drift returns next quarter.

Plugins vendor their host contract rather than path-depending on it, so that a plugin
repository clones and builds with no herdr checkout present
(`herdr-plugins/harness-manager/VENDOR.md`). Vendoring is a *copy*, and a copy has a
direction. The profile work was done on the plugin side — commit `21c154b3`
("feat(harness-manager): global configurations view and host discovery") — where the guest
that needed it lives. It landed in `vendor/`, and in the plugin repo's `host/harness_host.rs`,
which that repo carries as *reference* source: `Cargo.toml` there builds only the guest
cdylib, so nothing in the plugin repository type-checks `host/`.

So the feature was complete, reviewed, and unbuildable, in a directory whose own README
says: *"`harness_host.rs` is **not** verified to compile, and cannot be from this
repository. Whoever does wave-2 must type-check it in the host tree first. Do not read
'reconciled' as 'compiles'."*

That is exactly the work this ADR records. The drift persisted because the copy has a
direction and no gate ran in the direction that mattered.

## Decision

Sync the model crate, and port the profile machinery into the host's own collector
**additively**. Net effect: +576 lines, −2 lines, no host behaviour removed.

1. `crates/harness-manager-model/src/lib.rs` ← `vendor/harness-manager-model/src/lib.rs`.
   Purely additive: +72 lines, three new structs, five new `#[serde(default)]` fields on
   `HarnessRenderModel`, and `..Default::default()` in the existing roundtrip test.
2. `src/harness_host.rs` keeps the host's own production code and gains only the profile
   machinery lifted from the plugin repo's reference copy: `resource_catalog_fixtures`,
   `ProfileInfo`, `ProfileScan`, `discover_profiles`, `profile_signature`, `scan_profile`,
   `ensure_profile_scan`, `active_profile_name`, `profile_view_sections`,
   `build_profile_model`, four `HarnessCache` fields, and the call site in `render_json`
   (which becomes `&mut HarnessCache` so scans memoise across ticks).

### The alternative I tried first, and why it was wrong

The obvious move — and the one the plugin repo's `host/README.md` invites — is to adopt its
`host/harness_host.rs` wholesale, since that README establishes the two variants as a strict
semantic superset: *"621 lines added, 5 replaced, 0 lost."*

**That claim is stale, and adopting on the strength of it silently deletes working code.**

It was measured against `2b012356` (the v0.7.5-138 lineage). The host has since moved to
`752d715d` through the v0.8.0 merge and gained work the plugin's snapshot never saw. I
adopted the file wholesale, and the compiler found the drift in the *other* direction:

| host has at `752d715d` | plugin reference copy |
|---|---|
| `HarnessRenderModel.generated_at` | absent |
| `create_tab_argv_command(.., host_terminal_appearance)` | 7-arg call |
| `split_pane_argv_command(.., host_terminal_appearance, ..)` | 10-arg call |
| `resolve_row_path` | **absent** |
| `remove_mcp_from_file` | **absent** |
| `restore_from_stash` | **absent** |
| `stashed_rows` | **absent** |
| `write_atomic` | **absent** |

The first three are mechanical and I fixed them. The last five are not: they are the
disable/undo/stash machinery — the only durable record of what a disable removed — and the
host's own tests exercise them. A wholesale adopt compiles the *binary* fine (those helpers
are only reachable from code the plugin copy also dropped) and fails only in the **test**
target. Had I trusted `cargo check --bins`, this ADR would be recording a "clean superset
adoption" that had quietly deleted a feature.

So the superset relationship held once and does not hold now. Both lineages have advanced.
Additive porting is the only resolution that loses nothing, and the `--tests` target is what
proved it — `--bins` alone would have shipped the regression.

**Only sync the model crate, leave the collector.** Also rejected: it makes the host *able*
to express profiles without ever *emitting* them. The panel would look identical, and the
issue would be marked fixed. A type that can represent data nothing populates is not a fix.

**Delete tabs 2/3 and deprecate the profile view.** Rejected — see below.

**Delete tabs 2/3 and deprecate the profile view.** The doctrine says deprecate a custom
feature when *native* functionality covers its goal. Nothing in v0.8.0 enumerates Claude
Code harness profiles or costs their context. There is no native replacement to adopt.

## Consequences

* Tabs 2 and 3 populate from a real host scan: `~/.claude`, each ccr profile (whose config
  nests under `<name>/claude`), and each ccp profile. The active profile is derived from
  the focused session's transcript path.
* Profile scans are memoised behind an mtime signature over `settings.json`,
  `settings.local.json`, `CLAUDE.md`, `skills/` and `agents/`, so the ~1 Hz dash tick does
  not re-walk every profile every second.
* The host and the guest's `vendor/` copy are byte-identical again. `VENDOR.md`'s note that
  `vendor/` "intentionally no longer matches the `2b012356` pin" is now resolved in the
  other direction: the host moved to meet the plugin, so there is one contract again.

### Preventing recurrence

The rearchitecture that matters is not this file swap — it is removing the copy that has a
direction. Two changes, in order of cost:

1. **Now, cheap: a drift gate.** The host tree is the only place that can type-check a
   collector, and it is the only place both copies exist. A test that compares
   `crates/<x>-model/src/lib.rs` against every plugin's `vendor/<x>-model/src/lib.rs` and
   fails on difference converts this silent, months-long divergence into a red CI run. It
   is the cheapest thing that would have caught this, and it generalises to every plugin
   that vendors a model crate — which, per `VENDOR.md`, is all of them.

   Whatever gate is built must run `cargo check --tests`, not `--bins`. This change is the
   worked example: the regression a wholesale adopt introduced was invisible to `--bins`
   and caught only by the test target.
2. **Wave 2, structural: retire host collectors.** The real defect is that a *plugin's*
   render model is built by *host* code in a different repository. `usage_host.rs` already
   carries the canonical exit plan: a plugin-side collector process subscribes to
   `events.subscribe` and writes its own `ui_input_file`, and the guest owns its view state
   through v3 `Action`. When `harness_host.rs` moves into the plugin repository as a real
   binary target, `crates/harness-manager-model` stops existing host-side, the vendored
   copy stops being a copy, and this class of drift is not fixed but *absent*.

Until (2) lands, (1) is what holds the line.

## Verification

* `cargo check` in the host tree — the type-check the plugin repository structurally cannot
  perform, and the one the reference copy's README explicitly defers to whoever does this
  work.
* Linux E2E on a crabbox-leased sprite running hr `0.8.0-181-g752d715d`: with a
  host-shaped (pre-fix) model, tabs 2 and 3 render empty-state text; with a guest-shaped
  model carrying `profiles` / `profile_view` / `selected_profile`, the same unmodified wasm
  guest populates both. That pair isolates the defect to host emission and confirms the
  guest needed no change — which is why this ADR is a host change and there is no
  corresponding guest fix.
