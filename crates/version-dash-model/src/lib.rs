//! wasm-safe data model for the version dashboard.
//!
//! Two consumers share this crate:
//!   - the out-of-process `version-dash-collector` binary FILLS a
//!     [`VersionRenderInput`] (one [`InstanceStatus`] per herdr instance it
//!     tracks), serializes it, and writes it to the plugin's state dir
//!     (`version_input.json`);
//!   - the `version-dash-ui` guest wasm module READS the same struct back from
//!     the host-injected `HERDR_PLUGIN_UI_INPUT_JSON` and renders it.
//!
//! It contains NO I/O and NO rendering so it compiles cleanly to
//! `wasm32-unknown-unknown` for the guest and to a native binary for the
//! collector. The only logic here is version parsing / comparison / state
//! classification, which both sides need and which is unit-tested below.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

/// Per-instance freshness/health, deciding the strip glyph and color.
///
/// # The distinction that matters: `UpToDate` vs `Verified`
///
/// `UpToDate` is a claim about a VERSION STRING — "this binary reports a number
/// at or above the latest stable release". That is a weak claim, and trusting it
/// is what let a bridge-less upstream 0.7.5 look healthy: it reported `0.7.5`,
/// answered `--version`, and would have classified `UpToDate` while carrying
/// none of the fork's wasm plugin bridge.
///
/// `Verified` is a claim about a specific BINARY — "the post-update gate ran
/// against this exact build (sha256-bound receipt), the fork bridge is present,
/// the instance actually launched, and its plugins loaded". Only `Verified`
/// earns the bright green check. `UpToDate` renders muted, because "the version
/// number looks fine" is not the same sentence as "it works".
///
/// This is deliberate and load-bearing: the update buttons promise the user that
/// a green check means done, so a green check must be something the pipeline
/// EARNED, never something a version comparison handed out.
///
/// `UpToDate`/`Behind` compare version strings only, so they cannot by
/// themselves distinguish which fork BUILD an instance runs — two instances
/// share a version string until a release bump. The `+N` marker is what
/// separates them, and it is now baked into each binary at build time and read
/// back out of the file, rather than being computed from whatever working tree
/// the collector happens to find.
///
/// DESERIALIZATION IS DELIBERATELY TOLERANT — see the `Deserialize` impl below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState {
    /// Version at or ahead of the latest stable release, but NOT verified this
    /// cycle (muted check). See the type docs for why this is not green.
    UpToDate,
    /// Behind the latest stable release — an update exists (yellow).
    Behind,
    /// Instance is broken: server socket dead, binary won't report a version, or
    /// its verification gate failed (red).
    Broken,
    /// Version could not be resolved and the instance is not known-broken (dim).
    #[default]
    Unknown,
    /// An update or verification is running against this instance RIGHT NOW
    /// (cyan). Without this state a 90-second silence is indistinguishable from
    /// a hang, so the strip must show motion while the pipeline works.
    Working,
    /// The post-update verification gate PASSED against this instance's current
    /// binary (bright green). The only state that authorizes promoting onward.
    Verified,
    /// Automation could not finish safely and handed the job to an agent
    /// (magenta). Visible so "did it work?" stays answerable from the strip
    /// instead of becoming a debugging session.
    Escalated,
    /// The new binary is installed and gated, but this instance is still RUNNING
    /// the old one; only a restart completes the update (cyan, with an explicit
    /// `press ⌃⌥X` hint).
    ///
    /// This state exists because restarting `hr` is a consent point, never an
    /// automation: `hr` is the multiplexer hosting the user's session and every
    /// agent in it, so an agent restarting it kills the very process tree that
    /// would report what happened. The pipeline therefore stops here and asks,
    /// and the strip carries the ask.
    AwaitingRestart,
    /// A DIFFERENT binary is staged on disk than the one this instance is
    /// running, and nothing has verified it (red, `⚑ staged UNVERIFIED`).
    ///
    /// The distinction from [`InstanceState::AwaitingRestart`] is the whole
    /// point and it is a safety distinction, not a cosmetic one:
    ///
    /// * `AwaitingRestart` — the staged binary passed the gate. Restarting is
    ///   the *completion* of an update. Calm cyan, "press ⌃⌥X".
    /// * `UnsafeRestart` — a binary appeared that this pipeline never gated.
    ///   Restarting is a *coin flip*. It must not look calm.
    ///
    /// Written on 2026-07-27, the day `~/.herdr-hr/bin/herdr` was staged to
    /// 0.7.5 by a path outside this pipeline while the strip painted a green
    /// `✓ up_to_date`, because the collector read the version off the binary on
    /// disk and never asked the running server what it was actually executing.
    /// The next restart would have booted 0.7.5 — the release that migrates the
    /// plugin registry to a global path and ignores the per-session
    /// `plugins.json` — emptying the plugin layer that had just been repaired.
    /// One keystroke away, and the strip was encouraging it.
    ///
    /// A calm wrong signal is worse than a loud uncertain one.
    UnsafeRestart,
}

/// An unrecognised state name deserializes to [`InstanceState::Unknown`]
/// instead of failing.
///
/// ── WHY THIS IS NOT THE DERIVE ───────────────────────────────────────────────
/// The derived impl rejects unknown variants, and `#[serde(other)]` does not
/// apply to a plain string-valued enum, so tolerance has to be written out.
///
/// It is written out because of a measured failure. The guest `.wasm` and the
/// collector that feeds it are independent artefacts installed at independent
/// times; the host loads whichever wasm the plugin registry points at. On
/// 2026-07-27 a guest built before a new state existed was handed that state in
/// a sandbox, and the result was not a visible error — `from_str` failed on the
/// WHOLE `VersionRenderInput`, the guest returned early, and the host redrew its
/// last good frame. The strip kept painting an old, confident answer for as long
/// as the mismatch lasted. A status widget that silently freezes is worse than
/// one that admits it does not know, because the user cannot tell the difference
/// between "fine" and "stopped reporting".
///
/// So one unreadable field now costs exactly one `?` glyph on one token, and the
/// rest of the strip — including every other instance — still renders. The
/// collector additionally downgrades states it can prove the installed guest
/// cannot read; this impl is the backstop for guests that predate that logic.
impl<'de> Deserialize<'de> for InstanceState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(InstanceState::ALL
            .into_iter()
            .find(|state| state.wire_name() == raw)
            .unwrap_or(InstanceState::Unknown))
    }
}

impl InstanceState {
    /// Every variant, oldest-vocabulary first.
    ///
    /// The order is the order the states were introduced, and the first four are
    /// the original vocabulary that every version-dash guest ever shipped can
    /// deserialize. The collector uses that fact to detect what an installed
    /// guest `.wasm` understands before handing it a state name, because an
    /// unknown variant does not degrade — it fails the whole `VersionRenderInput`
    /// parse and freezes the strip on its last frame.
    pub const ALL: [InstanceState; 9] = [
        InstanceState::UpToDate,
        InstanceState::Behind,
        InstanceState::Broken,
        InstanceState::Unknown,
        InstanceState::Working,
        InstanceState::Verified,
        InstanceState::Escalated,
        InstanceState::AwaitingRestart,
        InstanceState::UnsafeRestart,
    ];

    /// The serde wire name, matching `#[serde(rename_all = "snake_case")]`.
    ///
    /// Kept as an explicit table rather than derived, so that renaming a variant
    /// cannot silently change the wire format that installed guests parse.
    pub fn wire_name(self) -> &'static str {
        match self {
            InstanceState::UpToDate => "up_to_date",
            InstanceState::Behind => "behind",
            InstanceState::Broken => "broken",
            InstanceState::Unknown => "unknown",
            InstanceState::Working => "working",
            InstanceState::Verified => "verified",
            InstanceState::Escalated => "escalated",
            InstanceState::AwaitingRestart => "awaiting_restart",
            InstanceState::UnsafeRestart => "unsafe_restart",
        }
    }

    /// True when this state means the pipeline is mid-flight, so the UI should
    /// neither offer a new action nor claim a result yet.
    pub fn is_busy(self) -> bool {
        matches!(self, InstanceState::Working)
    }

    /// True when this instance has EARNED the right to be promoted onward:
    /// a sha-bound verification receipt matched its current binary.
    ///
    /// Deliberately not `matches!(self, Verified | UpToDate)`. `UpToDate` must
    /// never authorize a promotion — that is precisely the weak claim that let
    /// an unbridged binary through.
    pub fn is_promotable(self) -> bool {
        matches!(self, InstanceState::Verified)
    }
}

/// Outcome of the out-of-band verification gate for one instance, as recorded in
/// its sha256-bound receipt.
///
/// The sha binding is the whole point: a receipt is only honoured while the
/// binary it was written for is still the binary in place. Swap the binary by any
/// route — the pipeline, a hand `mv`, a stray `cargo` relink — and the receipt
/// stops matching, so the instance silently drops out of `Verified` instead of
/// carrying a stale blessing forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Verification {
    /// No receipt, or a receipt that does not match the current binary.
    #[default]
    None,
    /// Receipt matches the current binary and every check passed.
    Passed,
    /// Receipt matches the current binary and at least one check failed.
    Failed,
}

/// Which stage of the staged-promotion pipeline a run belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStage {
    /// Build/promote the fork tip into herdrtest (button 1).
    Test,
    /// Promote the verified herdrtest build into hr (button 2).
    Hr,
}

impl PipelineStage {
    /// The instance label this stage acts on, matching `InstanceStatus::label`.
    pub fn label(self) -> &'static str {
        match self {
            PipelineStage::Test => "test",
            PipelineStage::Hr => "hr",
        }
    }
}

/// Where a pipeline run currently stands. Terminal phases are `Verified`,
/// `Failed` and `Escalated`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PipelinePhase {
    /// Nothing running.
    #[default]
    Idle,
    /// A step is executing (build, swap, relaunch, verify).
    Running,
    /// Finished and the verification gate passed.
    Verified,
    /// Finished and failed, with no agent handoff (yet).
    Failed,
    /// Failed and handed to an agent via cx.
    Escalated,
    /// Finished the work but is waiting on the user — the only phase that
    /// blocks on a human. Used for the hr restart, which is never taken without
    /// consent.
    AwaitingConsent,
}

/// Live status of the staged-promotion pipeline, written by the update scripts
/// and read back by the collector so the strip can narrate a run in progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PipelineStatus {
    /// Which button is running.
    pub stage: Option<PipelineStage>,
    /// Where it stands.
    #[serde(default)]
    pub phase: PipelinePhase,
    /// Short human step, e.g. `"building fork release"`. Shown in the toast.
    #[serde(default)]
    pub step: String,
    /// Elapsed label for the current run, e.g. `"0:42"`. Rendered next to the
    /// working glyph so a long step never looks like a hang. Collector-formatted
    /// because the guest has no wall clock.
    #[serde(default)]
    pub elapsed_label: String,
    /// Full log path for the run, surfaced in the toast so the user (or an
    /// agent) can read exactly what happened.
    #[serde(default)]
    pub log_path: Option<String>,
    /// Exit code of the failing step, when one failed.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Short reason when the run failed or escalated.
    #[serde(default)]
    pub error: Option<String>,
    /// How to reach the agent that took over, when escalated.
    #[serde(default)]
    pub agent_ref: Option<String>,
}

/// One tracked herdr instance (hr or herdrtest).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InstanceStatus {
    /// Short display label, e.g. `"hr"` / `"test"`.
    pub label: String,
    /// The instance's running version (from its binary `--version`), e.g.
    /// `"0.7.3"`. `None` when unresolved (unknown/broken).
    #[serde(default)]
    pub version: Option<String>,
    /// Count of own-fork commits ahead of the latest upstream stable tag
    /// (git-source-derived; reflects source HEAD, not the exact per-binary
    /// build — see [`InstanceState`]). Rendered as `+N`. `None` when unknown.
    #[serde(default)]
    pub local_changes: Option<u32>,
    /// Freshness/health color state.
    #[serde(default)]
    pub state: InstanceState,
    /// Short one-line detail for this instance's toast, e.g. the failing check
    /// or the current pipeline step. `None` when there is nothing to add.
    #[serde(default)]
    pub detail: Option<String>,
}

impl InstanceStatus {
    /// Full classification, accounting for a pipeline run in flight and the
    /// sha-bound verification receipt as well as the version comparison.
    ///
    /// Precedence, strongest first — each rung answers a question the rungs
    /// below it cannot:
    ///   1. `Working`   — a run is executing against THIS instance. Live truth
    ///                    beats every cached fact, and nothing hides behind a
    ///                    stale glyph while the binary is being replaced.
    ///   2. `Escalated` — automation stopped and an agent owns it. Ranked above
    ///                    `Broken` because "someone is on it" is more actionable
    ///                    than "it is broken".
    ///   3. `Broken`    — liveness failed, or verification ran and FAILED. A
    ///                    failed gate is a stronger signal than any version
    ///                    string, so `UpToDate` must never override it.
    ///   4. `Verified`  — the gate passed against this exact binary.
    ///   5. version comparison — the weak, last-resort claim.
    pub fn classify_full(
        version: Option<&str>,
        latest: Option<&str>,
        broken: bool,
        pipeline: Option<PipelinePhase>,
        verification: Verification,
    ) -> InstanceState {
        Self::classify_running(version, latest, broken, pipeline, verification, None)
    }

    /// As [`Self::classify_full`], but told what the instance is ACTUALLY
    /// EXECUTING as well as what is on disk.
    ///
    /// `running` is the version reported by the live server (`herdr status
    /// server`); `version` is what the binary on disk reports. When they
    /// disagree, a restart will change the running code, and the strip's job is
    /// to say whether that is safe:
    ///
    /// * staged build carries a passing receipt → [`InstanceState::AwaitingRestart`]
    /// * staged build carries nothing           → [`InstanceState::UnsafeRestart`]
    ///
    /// Reading only the on-disk binary is what let the strip paint `✓
    /// up_to_date` over an ungated 0.7.5 that was one keystroke from emptying
    /// the user's plugin layer. A version read off a file is a statement about a
    /// file. Only the server can say what is running.
    pub fn classify_running(
        version: Option<&str>,
        latest: Option<&str>,
        broken: bool,
        pipeline: Option<PipelinePhase>,
        verification: Verification,
        running: Option<&str>,
    ) -> InstanceState {
        match pipeline {
            Some(PipelinePhase::Running) => return InstanceState::Working,
            Some(PipelinePhase::Escalated) => return InstanceState::Escalated,
            Some(PipelinePhase::AwaitingConsent) => return InstanceState::AwaitingRestart,
            _ => {}
        }
        if broken || verification == Verification::Failed {
            return InstanceState::Broken;
        }
        // A staged-but-not-running binary outranks every version comparison
        // below it. Whether the instance is "up to date" is a far less urgent
        // question than whether the next restart is safe.
        if let (Some(disk), Some(live)) = (version, running) {
            if disk != live {
                return if verification == Verification::Passed {
                    InstanceState::AwaitingRestart
                } else {
                    InstanceState::UnsafeRestart
                };
            }
        }
        if verification == Verification::Passed {
            return InstanceState::Verified;
        }
        Self::classify(version, latest, broken)
    }

    /// Classify state from the instance's version vs the latest stable release.
    /// `broken` (herdrtest liveness failure) always wins. A missing/unparseable
    /// version with no broken signal is `Unknown`.
    ///
    /// Retained as the weak, version-only classifier. Prefer
    /// [`classify_full`](InstanceStatus::classify_full), which also honours the
    /// verification receipt.
    pub fn classify(version: Option<&str>, latest: Option<&str>, broken: bool) -> InstanceState {
        if broken {
            return InstanceState::Broken;
        }
        match (version, latest) {
            (Some(v), Some(l)) => match compare_versions(v, l) {
                // v >= latest ⇒ up to date; v < latest ⇒ behind.
                Ordering::Less => InstanceState::Behind,
                _ => InstanceState::UpToDate,
            },
            _ => InstanceState::Unknown,
        }
    }
}

/// Everything the strip needs to render, produced by the collector each run.
///
/// The collector rewrites this file on every cadence tick (success OR failure),
/// so `age_label` is recomputed relative to the last SUCCESSFUL fetch and is
/// never more than one cadence interval stale. The guest stays clock-free
/// (wasm has no wall clock), so all time formatting happens collector-side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VersionRenderInput {
    /// Tracked instances, in display order (hr first, then herdrtest).
    #[serde(default)]
    pub instances: Vec<InstanceStatus>,
    /// The latest STABLE version from the release channel manifest
    /// (`herdr.dev/latest.json`), e.g. `"0.7.5"`. `None` on a failed fetch with
    /// no cached value.
    #[serde(default)]
    pub latest: Option<String>,
    /// URL to the latest release's notes / changelog (for the click toast).
    #[serde(default)]
    pub notes_url: Option<String>,
    /// Human date/label of the latest release, when the manifest carries one.
    #[serde(default)]
    pub latest_date: Option<String>,
    /// Human "freshness" of `latest` relative to the last successful fetch,
    /// e.g. `"just now"`, `"12m"`, `"3h"`. Empty when never fetched.
    #[serde(default)]
    pub age_label: String,
    /// True when the last successful fetch is older than the freshness budget:
    /// the strip dims + shows the age so a dead collector / offline host is
    /// visible, never a crash.
    #[serde(default)]
    pub stale: bool,
    /// Last error string when the most recent run failed (offline, bad JSON, …).
    /// The previous good values are still carried so the strip shows last-known.
    #[serde(default)]
    pub error: Option<String>,
    /// Live status of the staged-promotion pipeline, when a run is in flight or
    /// recently finished. `None` when nothing has run.
    #[serde(default)]
    pub pipeline: Option<PipelineStatus>,
    /// A prebuilt, already-verified fork binary is staged and ready to promote
    /// into herdrtest in seconds. Set by the background prebuild; it is what
    /// makes button 1 finish inside the user's patience budget instead of
    /// blocking on a ~126 s release build at click time.
    #[serde(default)]
    pub staged_build: Option<StagedBuild>,
}

/// A background-prebuilt fork binary waiting to be promoted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct StagedBuild {
    /// Absolute path to the staged binary.
    pub path: String,
    /// Its `--version`.
    #[serde(default)]
    pub version: Option<String>,
    /// Fork commits ahead of the version tag, at build time.
    #[serde(default)]
    pub local_changes: Option<u32>,
    /// sha256 of the staged binary, so the promoter can prove it promoted the
    /// exact artifact that was prebuilt and gated.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Human age of the staged build, e.g. `"12m"`.
    #[serde(default)]
    pub age_label: String,
    /// Whether the staged binary passed the bridge-presence gate at build time.
    /// A staged build that did not pass is never promotable.
    #[serde(default)]
    pub bridge_ok: bool,
}

impl VersionRenderInput {
    /// Find an instance by label.
    pub fn instance(&self, label: &str) -> Option<&InstanceStatus> {
        self.instances.iter().find(|i| i.label == label)
    }

    /// Whether promoting `test` → `hr` may be OFFERED at all, and why not.
    ///
    /// This is the inert-button rule, and it lives in the shared model on
    /// purpose: the guest uses it to decide whether to expose the action, and
    /// the promotion script re-derives the same answer independently before
    /// touching a byte. Two enforcement points, one definition — the UI can hide
    /// the action, but hiding is never what makes it safe.
    ///
    /// `hr` may only be promoted when `test` is `Verified`: a sha-bound receipt
    /// proving the gate ran against the exact binary now installed in herdrtest.
    /// `UpToDate` is explicitly NOT enough — that is the weak version-string
    /// claim that let a bridge-less upstream binary look healthy.
    pub fn hr_promotion_block(&self) -> Option<String> {
        if let Some(p) = &self.pipeline {
            if p.phase == PipelinePhase::Running {
                let stage = p.stage.map(|s| s.label()).unwrap_or("a");
                return Some(format!("an update is already running ({stage})"));
            }
        }
        // hr's binary has already been swapped and is waiting on the user's
        // restart. Promoting again would install over a binary the user has not
        // yet booted, so the second press would silently discard the build the
        // strip is telling them to restart into.
        if let Some(hr) = self.instance("hr") {
            match hr.state {
                InstanceState::AwaitingRestart => {
                    return Some("hr is already updated — press ⌃⌥X to restart into it".to_string())
                }
                // Promoting on top of an ungated staged binary would overwrite
                // the evidence of how it got there and make the anomaly look
                // like a normal update. Freeze instead.
                InstanceState::UnsafeRestart => {
                    return Some(
                        "hr has an UNVERIFIED binary staged by something outside this pipeline \
                         — do not restart, and do not promote over it"
                            .to_string(),
                    )
                }
                _ => {}
            }
        }
        match self.instance("test") {
            None => Some("no herdrtest status yet".to_string()),
            Some(test) if test.state.is_promotable() => None,
            Some(test) => Some(match test.state {
                InstanceState::Working => "herdrtest is still being verified".to_string(),
                InstanceState::Broken => "herdrtest is BROKEN — fix it before promoting".to_string(),
                InstanceState::Escalated => "an agent is still working on herdrtest".to_string(),
                InstanceState::Behind => "herdrtest is behind and unverified".to_string(),
                InstanceState::AwaitingRestart => {
                    "herdrtest has not been restarted onto the new build yet".to_string()
                }
                InstanceState::UnsafeRestart => {
                    "herdrtest has an UNVERIFIED binary staged — verify it first".to_string()
                }
                InstanceState::UpToDate => {
                    "herdrtest is unverified — run the herdrtest update first".to_string()
                }
                InstanceState::Unknown | InstanceState::Verified => {
                    "herdrtest status is unknown".to_string()
                }
            }),
        }
    }

    /// Whether promoting the staged build into `test` may be offered.
    /// Blocked only while a run is already in flight — otherwise button 1 is
    /// always available, since rebuilding herdrtest is the recovery path.
    pub fn test_promotion_block(&self) -> Option<String> {
        match &self.pipeline {
            Some(p) if p.phase == PipelinePhase::Running => {
                let stage = p.stage.map(|s| s.label()).unwrap_or("a");
                Some(format!("an update is already running ({stage})"))
            }
            _ => None,
        }
    }
}

/// Parse a dotted version like `"0.7.3"` or `"0.7.3-preview.4"` into its numeric
/// release components (`[0, 7, 3]`), ignoring any `-suffix`/`+build` metadata.
/// Returns `None` when there is no leading numeric component to compare.
pub fn parse_version(raw: &str) -> Option<Vec<u64>> {
    let core = raw.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or("").trim();
    if core.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    for segment in core.split('.') {
        match segment.parse::<u64>() {
            Ok(n) => parts.push(n),
            Err(_) => break,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

/// Compare two version strings by their numeric release components, treating a
/// shorter version as zero-padded (`0.7` == `0.7.0`). Unparseable inputs sort as
/// `Equal` so they never masquerade as an available update.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    match (parse_version(a), parse_version(b)) {
        (Some(a), Some(b)) => {
            let len = a.len().max(b.len());
            for i in 0..len {
                let ai = a.get(i).copied().unwrap_or(0);
                let bi = b.get(i).copied().unwrap_or(0);
                match ai.cmp(&bi) {
                    Ordering::Equal => continue,
                    other => return other,
                }
            }
            Ordering::Equal
        }
        _ => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_decorated_versions() {
        assert_eq!(parse_version("0.7.3"), Some(vec![0, 7, 3]));
        assert_eq!(parse_version("v0.7.3"), Some(vec![0, 7, 3]));
        assert_eq!(parse_version(" 0.7.3 "), Some(vec![0, 7, 3]));
        assert_eq!(parse_version("0.7.3-preview.4"), Some(vec![0, 7, 3]));
        assert_eq!(parse_version("0.7.3+build.9"), Some(vec![0, 7, 3]));
        assert_eq!(parse_version("1.0"), Some(vec![1, 0]));
    }

    #[test]
    fn rejects_non_numeric_versions() {
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("herdr"), None);
        assert_eq!(parse_version("-"), None);
    }

    #[test]
    fn numeric_ordering_not_lexicographic() {
        assert_eq!(compare_versions("0.7.10", "0.7.9"), Ordering::Greater);
        assert_eq!(compare_versions("0.7.2", "0.7.10"), Ordering::Less);
        assert_eq!(compare_versions("0.8.0", "0.7.99"), Ordering::Greater);
    }

    #[test]
    fn zero_pads_shorter_version() {
        assert_eq!(compare_versions("0.7", "0.7.0"), Ordering::Equal);
        assert_eq!(compare_versions("0.7.1", "0.7"), Ordering::Greater);
    }

    #[test]
    fn unparseable_never_greater() {
        assert_eq!(compare_versions("garbage", "0.7.0"), Ordering::Equal);
        assert_eq!(compare_versions("0.7.0", "garbage"), Ordering::Equal);
    }

    #[test]
    fn classify_broken_wins_over_version() {
        assert_eq!(
            InstanceStatus::classify(Some("0.7.5"), Some("0.7.5"), true),
            InstanceState::Broken
        );
    }

    #[test]
    fn classify_behind_and_uptodate() {
        // Fork 0.7.3 vs stable 0.7.5 ⇒ behind (yellow).
        assert_eq!(
            InstanceStatus::classify(Some("0.7.3"), Some("0.7.5"), false),
            InstanceState::Behind
        );
        // At latest ⇒ up to date (green).
        assert_eq!(
            InstanceStatus::classify(Some("0.7.5"), Some("0.7.5"), false),
            InstanceState::UpToDate
        );
        // Ahead of latest (fork can lead) ⇒ up to date (green).
        assert_eq!(
            InstanceStatus::classify(Some("0.8.0"), Some("0.7.5"), false),
            InstanceState::UpToDate
        );
    }

    #[test]
    fn classify_unknown_without_version() {
        assert_eq!(
            InstanceStatus::classify(None, Some("0.7.5"), false),
            InstanceState::Unknown
        );
        assert_eq!(
            InstanceStatus::classify(Some("0.7.5"), None, false),
            InstanceState::Unknown
        );
    }

    // ── the inert-button rule ──────────────────────────────────────────────
    // These are the tests that stand between a right-click on `hr` and a
    // destroyed plugin bridge. Every one of them asserts a REFUSAL.

    fn input_with_test_state(state: InstanceState) -> VersionRenderInput {
        VersionRenderInput {
            instances: vec![
                InstanceStatus {
                    label: "hr".into(),
                    version: Some("0.7.3".into()),
                    state: InstanceState::UpToDate,
                    ..Default::default()
                },
                InstanceStatus {
                    label: "test".into(),
                    version: Some("0.7.5".into()),
                    state,
                    ..Default::default()
                },
            ],
            latest: Some("0.7.5".into()),
            ..Default::default()
        }
    }

    #[test]
    fn hr_promotion_allowed_only_when_test_is_verified() {
        assert_eq!(
            input_with_test_state(InstanceState::Verified).hr_promotion_block(),
            None,
            "a verified test build is the one case that may be promoted"
        );
    }

    #[test]
    fn hr_promotion_blocked_for_every_unverified_test_state() {
        // UpToDate is the dangerous one: an unbridged upstream 0.7.5 reports a
        // version at/above latest and would classify UpToDate. It must NOT open
        // the promotion path.
        for state in [
            InstanceState::UpToDate,
            InstanceState::Behind,
            InstanceState::Broken,
            InstanceState::Unknown,
            InstanceState::Working,
            InstanceState::Escalated,
            InstanceState::AwaitingRestart,
        ] {
            assert!(
                input_with_test_state(state).hr_promotion_block().is_some(),
                "hr promotion must be blocked while test is {state:?}"
            );
        }
    }

    #[test]
    fn hr_promotion_blocked_when_no_test_status_exists() {
        let input = VersionRenderInput {
            instances: vec![InstanceStatus {
                label: "hr".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(input.hr_promotion_block().is_some());
    }

    #[test]
    fn hr_promotion_blocked_while_hr_awaits_its_restart() {
        // Pressing hr twice must not install over a build the user has not yet
        // booted — that would silently throw away the build the strip is asking
        // them to restart into.
        let mut input = input_with_test_state(InstanceState::Verified);
        for inst in &mut input.instances {
            if inst.label == "hr" {
                inst.state = InstanceState::AwaitingRestart;
            }
        }
        let block = input.hr_promotion_block();
        assert!(block.is_some(), "hr must be inert while awaiting its restart");
        assert!(block.unwrap_or_default().contains("restart"));
    }

    #[test]
    fn a_staged_binary_the_pipeline_never_gated_is_unsafe_not_pending() {
        // The 2026-07-27 shape: disk says 0.7.5, the server is executing 0.7.3,
        // and nothing verified the staged build. This must NOT read as
        // up_to_date, and must NOT read as a calm "press ctrl+opt+X".
        let s = InstanceStatus::classify_running(
            Some("0.7.5"), Some("0.7.5"), false, None, Verification::None, Some("0.7.3"),
        );
        assert_eq!(s, InstanceState::UnsafeRestart);
        assert_ne!(s, InstanceState::UpToDate, "this was the lie");
        assert_ne!(s, InstanceState::AwaitingRestart, "calm is the wrong register here");
    }

    #[test]
    fn a_staged_binary_with_a_passing_receipt_is_merely_pending() {
        let s = InstanceStatus::classify_running(
            Some("0.7.5"), Some("0.7.5"), false, None, Verification::Passed, Some("0.7.3"),
        );
        assert_eq!(s, InstanceState::AwaitingRestart);
    }

    #[test]
    fn running_equals_disk_classifies_normally() {
        assert_eq!(
            InstanceStatus::classify_running(
                Some("0.7.5"), Some("0.7.5"), false, None, Verification::None, Some("0.7.5")),
            InstanceState::UpToDate);
        assert_eq!(
            InstanceStatus::classify_running(
                Some("0.7.3"), Some("0.7.5"), false, None, Verification::None, Some("0.7.3")),
            InstanceState::Behind);
    }

    #[test]
    fn unknown_running_version_falls_back_and_never_invents_a_state() {
        // If the server cannot be asked, do not guess that a restart is safe.
        assert_eq!(
            InstanceStatus::classify_running(
                Some("0.7.5"), Some("0.7.5"), false, None, Verification::None, None),
            InstanceState::UpToDate);
    }

    #[test]
    fn hr_promotion_blocked_while_hr_has_an_ungated_staged_binary() {
        let mut input = input_with_test_state(InstanceState::Verified);
        for inst in &mut input.instances {
            if inst.label == "hr" {
                inst.state = InstanceState::UnsafeRestart;
            }
        }
        let block = input.hr_promotion_block();
        assert!(block.is_some(), "must not promote over an ungated staged binary");
        assert!(block.unwrap_or_default().contains("UNVERIFIED"));
    }

    #[test]
    fn a_running_pipeline_blocks_both_buttons() {
        let mut input = input_with_test_state(InstanceState::Verified);
        input.pipeline = Some(PipelineStatus {
            stage: Some(PipelineStage::Test),
            phase: PipelinePhase::Running,
            ..Default::default()
        });
        assert!(
            input.hr_promotion_block().is_some(),
            "no promotion while another run is in flight"
        );
        assert!(input.test_promotion_block().is_some());
    }

    #[test]
    fn test_button_is_available_when_idle_even_if_test_is_broken() {
        // Rebuilding herdrtest is the RECOVERY path, so a broken test must not
        // lock the user out of the button that fixes it.
        let input = input_with_test_state(InstanceState::Broken);
        assert_eq!(input.test_promotion_block(), None);
    }

    // ── classification precedence ──────────────────────────────────────────

    #[test]
    fn verification_failure_beats_a_healthy_version_string() {
        // The bridge-less 0.7.5 scenario: version says "current", the gate says
        // "no". The gate must win.
        assert_eq!(
            InstanceStatus::classify_full(
                Some("0.7.5"),
                Some("0.7.5"),
                false,
                None,
                Verification::Failed
            ),
            InstanceState::Broken
        );
    }

    #[test]
    fn passed_verification_upgrades_uptodate_to_verified() {
        assert_eq!(
            InstanceStatus::classify_full(
                Some("0.7.5"),
                Some("0.7.5"),
                false,
                None,
                Verification::Passed
            ),
            InstanceState::Verified
        );
        // …and without a receipt the very same version is only UpToDate.
        assert_eq!(
            InstanceStatus::classify_full(
                Some("0.7.5"),
                Some("0.7.5"),
                false,
                None,
                Verification::None
            ),
            InstanceState::UpToDate
        );
    }

    #[test]
    fn a_running_pipeline_wins_over_every_cached_fact() {
        assert_eq!(
            InstanceStatus::classify_full(
                Some("0.7.5"),
                Some("0.7.5"),
                true,
                Some(PipelinePhase::Running),
                Verification::Failed
            ),
            InstanceState::Working
        );
    }

    #[test]
    fn escalation_outranks_broken() {
        assert_eq!(
            InstanceStatus::classify_full(
                None,
                Some("0.7.5"),
                true,
                Some(PipelinePhase::Escalated),
                Verification::Failed
            ),
            InstanceState::Escalated
        );
    }

    #[test]
    fn awaiting_consent_maps_to_awaiting_restart() {
        assert_eq!(
            InstanceStatus::classify_full(
                Some("0.7.3"),
                Some("0.7.5"),
                false,
                Some(PipelinePhase::AwaitingConsent),
                Verification::None
            ),
            InstanceState::AwaitingRestart
        );
    }

    #[test]
    fn only_verified_is_promotable() {
        assert!(InstanceState::Verified.is_promotable());
        for state in [
            InstanceState::UpToDate,
            InstanceState::Behind,
            InstanceState::Broken,
            InstanceState::Unknown,
            InstanceState::Working,
            InstanceState::Escalated,
            InstanceState::AwaitingRestart,
        ] {
            assert!(!state.is_promotable(), "{state:?} must not be promotable");
        }
    }

    #[test]
    fn round_trips_through_json() {
        let input = VersionRenderInput {
            instances: vec![
                InstanceStatus {
                    label: "hr".into(),
                    version: Some("0.7.3".into()),
                    local_changes: Some(107),
                    detail: None,
                    state: InstanceState::Behind,
                },
                InstanceStatus {
                    label: "test".into(),
                    version: Some("0.7.5".into()),
                    local_changes: Some(0),
                    detail: None,
                    state: InstanceState::Verified,
                },
            ],
            latest: Some("0.7.5".into()),
            notes_url: Some("https://herdr.dev/changelog".into()),
            latest_date: Some("2026-07-20".into()),
            age_label: "3h".into(),
            pipeline: Some(PipelineStatus {
                stage: Some(PipelineStage::Test),
                phase: PipelinePhase::Verified,
                step: "verified".into(),
                elapsed_label: "0:28".into(),
                log_path: Some("/tmp/vd/run.log".into()),
                exit_code: Some(0),
                error: None,
                agent_ref: None,
            }),
            staged_build: Some(StagedBuild {
                path: "/tmp/vd/staged/herdr".into(),
                version: Some("0.7.5".into()),
                local_changes: Some(3),
                sha256: Some("deadbeef".into()),
                age_label: "12m".into(),
                bridge_ok: true,
            }),
            stale: false,
            error: None,
        };
        let json = serde_json::to_string(&input).unwrap();
        let back: VersionRenderInput = serde_json::from_str(&json).unwrap();
        assert_eq!(input, back);
    }
}
