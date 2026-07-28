//! wasm-safe serde data model for the usage dashboard.
//!
//! Extracted from `usage-dash-core::model` so it can be shared, unchanged, by
//! both the native host (data collection) and the WASM render plugin. This crate
//! is a pure-data leaf: **no I/O, no ratatui, no chrono** — only `serde`. Color
//! mapping lives in the renderer (here every type carries plain data; the plugin
//! maps `ThreadState` etc. to role strings).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cli {
    Claude,
    Codex,
    Pi,
}

impl Cli {
    pub fn label(&self) -> &'static str {
        match self {
            Cli::Claude => "Claude",
            Cli::Codex => "Codex",
            Cli::Pi => "Pi",
        }
    }
}

/// What the focused agent's main thread is doing, inferred from its transcript
/// tail (see `usage-dash-core::state`). Color mapping lives in the renderer.
/// Invariant: never `Done` while anything is pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ThreadState {
    Working,
    Waiting,
    AwaitingReview,
    Blocked,
    Degraded,
    Done,
    #[default]
    Unknown,
}

impl ThreadState {
    pub fn label(&self) -> &'static str {
        match self {
            ThreadState::Working => "Working",
            ThreadState::Waiting => "Waiting",
            ThreadState::AwaitingReview => "Awaiting Review",
            ThreadState::Blocked => "Blocked",
            ThreadState::Degraded => "Degraded",
            ThreadState::Done => "Done",
            ThreadState::Unknown => "—",
        }
    }
}

/// One rolling rate-limit window (5-hour or weekly).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Window {
    pub used_pct: Option<f32>,
    pub resets_at: Option<i64>, // unix seconds
    pub shared: bool,           // true when mirrored from another CLI (pi ← codex)
}

/// Per-session statistics derived from the focused session's own transcript
/// (NOT global account usage). Every count is for *this session only*. Built
/// incrementally in `usage-dash-core::stats`; `None` on `UsageSnapshot` when
/// unavailable (e.g. Codex/pi) so the view renders dashes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionStats {
    pub autocompacts: u32,      // AC — count of compact_boundary events
    pub freed_tokens: u64,      // Σ (preTokens - postTokens) across compactions
    pub agents_running: u32,    // AR — Agent/Task tool_use with no result yet
    pub agents_done: u32,       // AD — toolUseResult records with an agentId
    pub agent_types: u32,       // AU — distinct agentType values seen
    pub out_tokens: u64,        // Σ assistant message.usage.output_tokens
    pub ctx_tokens: u64,        // latest assistant context size (in+cache)
    pub cache_pct: Option<f32>, // cache_read / (cache_read + cache_creation)
    pub assistant_turns: u32,
    pub user_turns: u32,
    pub tool_calls: u32,
    pub age_secs: u64, // last_ts - first_ts
    pub busy_secs: u64,
    pub idle_secs: u64,
}


/// How a running/installed CLI version compares to the latest published one.
///
/// `Unknown` is deliberately a first-class state rather than being folded into
/// `Outdated`. A failed or never-completed fetch must NOT paint the version red:
/// "we could not check" and "you are behind" are different facts and the strip
/// has to show them differently, or a dead network reads as a pending update
/// forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Current,
    Outdated,
    Unknown,
}

/// A CLI's running/installed version alongside the latest published one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliVersion {
    /// For Claude Code: the version the pane is actually RUNNING, read from the
    /// transcript envelope — not whatever is installed on disk, which may be
    /// newer and is exactly the gap this feature exists to show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// Latest published version. `None` until a fetch has ever succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// Age of the `latest` value in seconds. `None` when never fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_age_secs: Option<u64>,
}

impl CliVersion {
    /// Compare `current` against `latest`, dotted-numeric segment by segment.
    ///
    /// Any missing side, or any non-numeric segment, yields [`Freshness::Unknown`]
    /// rather than a guess — see the type docs on why that matters.
    pub fn freshness(&self) -> Freshness {
        let (Some(cur), Some(latest)) = (self.current.as_deref(), self.latest.as_deref()) else {
            return Freshness::Unknown;
        };
        match (parse_semver(cur), parse_semver(latest)) {
            (Some(c), Some(l)) if c >= l => Freshness::Current,
            (Some(_), Some(_)) => Freshness::Outdated,
            _ => Freshness::Unknown,
        }
    }
}

/// Parse a dotted numeric version into comparable segments. Returns `None` if
/// any segment is not a plain number, so exotic version strings degrade to
/// `Unknown` instead of comparing wrongly.
fn parse_semver(v: &str) -> Option<Vec<u64>> {
    let core = v.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let parts: Vec<u64> = core
        .split('.')
        .map(|p| p.parse::<u64>().ok())
        .collect::<Option<Vec<u64>>>()?;
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

/// Which clickable version token in the caption is being referred to.
///
/// Lives in the model crate (not the native core) because the WASM guest needs
/// the same identity to route hover and clicks — a second, parallel definition
/// is exactly the kind of duplication that lets the two renderers drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionToken {
    /// The Claude Code version this pane is running.
    Claude,
    /// The installed codex CLI version (cx panes only).
    Codex,
}

/// Versions shown in the usage caption.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CliVersions {
    /// Claude Code — always present, since every pane the usage dash tracks runs it.
    #[serde(default)]
    pub claude: CliVersion,
    /// The machine's installed `codex` CLI. Populated only for cx panes: in a
    /// native Claude pane the codex binary is not in the loop and showing its
    /// version would just be noise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<CliVersion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub cli: Cli,
    pub model: Option<String>,
    pub ctx_used_tokens: Option<u64>,
    pub ctx_window: Option<u64>,
    pub ctx_used_pct: Option<f32>,
    pub ctx_approx: bool,
    pub five_hour: Window,
    pub weekly: Window,
    /// Percentage points of the 100% weekly cap consumed *today* (delta since
    /// midnight baseline). Target: stay under `DAILY_BUDGET_PCT` (12%).
    pub daily_used_pct: Option<f32>,
    pub plan: Option<String>,
    pub source_age_secs: u64,
    /// State of the focused agent's main thread (inferred from its transcript).
    pub state: ThreadState,
    /// Short human-readable note: what it's running / waiting for / errors seen.
    pub state_note: Option<String>,
    /// Untruncated status text (the full last message) shown when the state row
    /// is expanded via click. `None` when there's nothing more than the brief note.
    pub state_full: Option<String>,
    /// Per-session STATS for the focused session. `None` when the source can't
    /// derive them yet (Codex/pi) → STATS view renders dashes, never fabricates.
    pub stats: Option<SessionStats>,
    /// Running/latest CLI versions rendered as clickable tokens in the caption.
    #[serde(default)]
    pub versions: CliVersions,
}

impl UsageSnapshot {
    pub fn empty(cli: Cli) -> Self {
        UsageSnapshot {
            cli,
            model: None,
            ctx_used_tokens: None,
            ctx_window: None,
            ctx_used_pct: None,
            ctx_approx: false,
            five_hour: Window::default(),
            weekly: Window::default(),
            daily_used_pct: None,
            plan: None,
            source_age_secs: 0,
            state: ThreadState::Unknown,
            state_note: None,
            state_full: None,
            stats: None,
            versions: CliVersions::default(),
        }
    }

    /// Derive ctx % from used/window when not provided natively.
    pub fn fill_ctx_pct(&mut self) {
        if self.ctx_used_pct.is_none() {
            if let (Some(u), Some(w)) = (self.ctx_used_tokens, self.ctx_window) {
                if w > 0 {
                    self.ctx_used_pct = Some((u as f32 / w as f32) * 100.0);
                }
            }
        }
    }

    /// A session whose underlying file hasn't changed in this many seconds is stale.
    pub fn is_stale(&self) -> bool {
        self.source_age_secs >= 600
    }
}

/// The complete, wasm-safe render input the host hands the usage WASM module
/// each frame (serialized to one env key). Bundles the data snapshot with the
/// host-owned interactive view fields and the two absolute reset-time strings
/// the host precomputes (the guest can't read the local timezone). The renderer
/// consumes this and returns clickable hit-rects; it never reaches for I/O.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRenderInput {
    pub snapshot: Option<UsageSnapshot>,
    #[serde(default)]
    pub expanded: bool,
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default = "default_true")]
    pub stats_shown: bool,
    #[serde(default)]
    pub scroll: u16,
    #[serde(default)]
    pub now: i64,
    /// Local-time label for the 5-hour window reset (e.g. "11:57"); "" when none.
    #[serde(default)]
    pub five_hour_abs: String,
    /// Local-time label for the weekly window reset (e.g. "Sat"); "" when none.
    #[serde(default)]
    pub weekly_abs: String,
}

fn default_true() -> bool {
    true
}

// ---- pure helpers (unit-tested) ----

/// pi sanitizes a cwd into its sessions dir name: `/Users/andual` -> `--Users-andual--`.
/// Each maximal run of non-alphanumeric chars collapses to a single `-`; the
/// result is trimmed of leading/trailing `-` and wrapped in `--` … `--`.
/// Verified against the real `~/.pi/agent/sessions/` dirs (2026-06-17).
pub fn sanitize_cwd(cwd: &str) -> String {
    let mut mid = String::new();
    let mut prev_dash = false;
    for ch in cwd.chars() {
        if ch.is_ascii_alphanumeric() {
            mid.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            mid.push('-');
            prev_dash = true;
        }
    }
    let trimmed = mid.trim_matches('-');
    format!("--{}--", trimmed)
}

/// Self-imposed daily budget: max this % of the weekly cap per day (from claude-dash).
pub const DAILY_BUDGET_PCT: f32 = 12.0;

/// Compact token count (matches statusline.sh `fmt_k`): integer k, then M.
/// 68000 -> "68k", 15877 -> "16k", 1_200_000 -> "1.2M".
pub fn fmt_k(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{}k", (n as f64 / 1000.0).round() as u64)
    } else {
        n.to_string()
    }
}

/// Countdown label from now to a future unix ts: "in 2h14m", "in 3d", or "overdue".
pub fn countdown(now: i64, resets_at: i64) -> String {
    let d = resets_at - now;
    if d <= 0 {
        return "overdue".to_string();
    }
    let mins = d / 60;
    if mins < 60 {
        format!("in {}m", mins.max(1))
    } else if mins < 60 * 24 {
        format!("in {}h{:02}m", mins / 60, mins % 60)
    } else {
        format!("in {}d", mins / (60 * 24))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_cwd_matches_pi_layout() {
        assert_eq!(sanitize_cwd("/Users/andual"), "--Users-andual--");
        assert_eq!(
            sanitize_cwd("/private/tmp/pi-bridge/live-monitor"),
            "--private-tmp-pi-bridge-live-monitor--"
        );
    }

    #[test]
    fn fmt_k_buckets() {
        assert_eq!(fmt_k(950), "950");
        assert_eq!(fmt_k(15877), "16k");
        assert_eq!(fmt_k(68000), "68k");
        assert_eq!(fmt_k(258400), "258k");
        assert_eq!(fmt_k(1_200_000), "1.2M");
    }

    #[test]
    fn countdown_future_and_past() {
        assert_eq!(countdown(1000, 1000 + 30 * 60), "in 30m");
        assert_eq!(countdown(1000, 1000 + (2 * 60 + 14) * 60), "in 2h14m");
        assert_eq!(countdown(1000, 1000 + 3 * 24 * 3600), "in 3d");
        assert_eq!(countdown(1000, 500), "overdue");
    }

    #[test]
    fn ctx_pct_derived_from_tokens() {
        let mut s = UsageSnapshot::empty(Cli::Codex);
        s.ctx_used_tokens = Some(15877);
        s.ctx_window = Some(258400);
        s.fill_ctx_pct();
        assert!((s.ctx_used_pct.unwrap() - 6.14).abs() < 0.1);
    }

    #[test]
    fn snapshot_json_round_trips() {
        let mut s = UsageSnapshot::empty(Cli::Claude);
        s.model = Some("Opus 4.8".into());
        s.ctx_used_pct = Some(29.0);
        s.five_hour.used_pct = Some(20.0);
        s.weekly.used_pct = Some(8.0);
        s.daily_used_pct = Some(6.0);
        s.state = ThreadState::Working;
        s.stats = Some(SessionStats {
            autocompacts: 1,
            agents_done: 4,
            agent_types: 1,
            ..Default::default()
        });
        let json = serde_json::to_string(&s).unwrap();
        let back: UsageSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.model.as_deref(), Some("Opus 4.8"));
        assert_eq!(back.state, ThreadState::Working);
        assert_eq!(back.stats.unwrap().agents_done, 4);
    }
}

#[cfg(test)]
mod version_tests {
    use super::*;

    fn v(current: &str, latest: &str) -> CliVersion {
        CliVersion {
            current: Some(current.to_string()),
            latest: Some(latest.to_string()),
            checked_age_secs: Some(0),
        }
    }

    #[test]
    fn equal_versions_are_current() {
        assert_eq!(v("2.1.220", "2.1.220").freshness(), Freshness::Current);
    }

    #[test]
    fn behind_is_outdated_and_ahead_is_current() {
        assert_eq!(v("2.1.219", "2.1.220").freshness(), Freshness::Outdated);
        // Segment-wise, not lexicographic: "2.1.9" vs "2.1.10" is the classic
        // string-compare trap and must resolve numerically.
        assert_eq!(v("2.1.9", "2.1.10").freshness(), Freshness::Outdated);
        assert_eq!(v("2.2.0", "2.1.220").freshness(), Freshness::Current);
        // A local build ahead of the registry is not "outdated".
        assert_eq!(v("2.1.221", "2.1.220").freshness(), Freshness::Current);
    }

    #[test]
    fn differing_segment_counts_compare_by_segment() {
        assert_eq!(v("2.1", "2.1.1").freshness(), Freshness::Outdated);
        assert_eq!(v("2.2", "2.1.9").freshness(), Freshness::Current);
    }

    #[test]
    fn prerelease_and_v_prefix_are_tolerated() {
        assert_eq!(v("v2.1.220", "2.1.220").freshness(), Freshness::Current);
        assert_eq!(v("2.1.220-beta.1", "2.1.220").freshness(), Freshness::Current);
    }

    /// The important negative case: a missing or unparseable version must never
    /// paint the token red. "could not check" is not "you are behind".
    #[test]
    fn unknown_never_masquerades_as_outdated() {
        assert_eq!(CliVersion::default().freshness(), Freshness::Unknown);
        assert_eq!(
            CliVersion {
                current: Some("2.1.220".into()),
                latest: None,
                checked_age_secs: None,
            }
            .freshness(),
            Freshness::Unknown
        );
        assert_eq!(v("nightly", "2.1.220").freshness(), Freshness::Unknown);
        assert_eq!(v("2.1.220", "unknown").freshness(), Freshness::Unknown);
    }

    /// Old `usage_input.json` written before this feature must still load, or a
    /// host/guest running the previous build would fail to parse every frame.
    /// Derived by round-tripping a real snapshot and deleting the new key, so
    /// the fixture cannot drift out of sync with the struct.
    #[test]
    fn snapshot_without_versions_deserializes() {
        let snap = UsageSnapshot::empty(Cli::Claude);
        let mut value = serde_json::to_value(&snap).expect("serialize");
        let obj = value.as_object_mut().expect("snapshot is a json object");
        assert!(
            obj.remove("versions").is_some(),
            "the field under test must actually be present before removal"
        );

        let legacy: UsageSnapshot =
            serde_json::from_value(value).expect("legacy snapshot still deserializes");
        assert_eq!(legacy.versions, CliVersions::default());
        assert_eq!(legacy.versions.claude.freshness(), Freshness::Unknown);
    }
}
