//! Shared, wasm-safe types for the tidus subagent bar.
//!
//! The collector (native) produces a [`BarInput`]; the guest (wasm) renders it.
//! Keeping the shape here means the two cannot drift silently — a field rename
//! is a compile error on both sides rather than a token that quietly stops
//! appearing on screen.
//!
//! ## What a chip means
//!
//! The bar shows the **ancestor chain of the selected node pinned left, then
//! the selected node itself, then its BROTHERS** (the other children of the
//! same parent), with the selected node's own DIRECT children on a second row
//! underneath it — never uncles or cousins.
//!
//! Selecting a brother re-roots the same shape on it: the brother set is a
//! property of the PARENT, not of the selection, so navigating sideways never
//! makes the row you navigated from disappear. That was the old behaviour and
//! it cost the operator their place: clicking a child replaced the row with
//! that child's own children, so the siblings you were comparing vanished at
//! the moment you compared them.
//!
//! Parentage is a fleet fact, not herdr tab order, and it has three sources
//! (`docs/tidus-ui-plugin-brief.md` §3):
//!
//! ```text
//! ~/harnesses/<child>/state/dispatch.json     → parent, while a job is in flight
//! ~/harnesses/<child>/harness.json  → team    → persistent declared attachment
//! ~/harnesses/<child>/state/last-result.json  → parent of the most recent job
//! ```
//!
//! `dispatch.json` disappears the moment a task completes, so it alone would
//! make children blink out of existence between jobs. The brief is explicit
//! that children stay **visually attached to their parent at all times**, so a
//! `team` declaration and the last finished job keep the chip on the bar; the
//! status dot, not the chip's presence, is what carries "this one is done".

pub mod codename;

use serde::{Deserialize, Serialize};

/// Snapshot schema version. Bumped when the guest would misread an older file;
/// the guest renders nothing rather than guess at a shape it does not know.
///
/// Deliberately NOT bumped for the codename fields: they are `#[serde(default)]`
/// additions, so a new guest reads an old snapshot (blank codename → the chip
/// falls back to its name) and an old guest reads a new one (unknown fields
/// ignored). A bump here would blank the bar on every build skew instead.
pub const SCHEMA: u32 = 1;

/// Maximum ancestors walked upward before we declare the graph pathological.
/// Nesting 5–6 deep is normal fleet operation (`maxInterceptDepth` in
/// harness-fleet), so this is a cycle backstop, not a policy.
pub const MAX_ANCESTRY: usize = 8;

/// Job-id suffix length used to disambiguate two dispatches to the same child
/// (brief §7: lineage tabs are `parent▸child:<jobid-tail6>`).
pub const JOB_TAIL: usize = 6;

/// Where a chip sits in the lineage. The order in [`BarInput::chips`] is already
/// the render order; the role drives styling and the pinned/scrollable split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChipRole {
    /// An ancestor of the selected node, root-most first. Pinned left.
    Ancestor,
    /// The selected node itself. Drawn FIRST after the lineage, carrying the
    /// selected indicator.
    Current,
    /// Another child of the selected node's parent — a brother. Drawn on the
    /// same row, immediately after [`ChipRole::Current`], and deliberately
    /// still there after the selection moves to one of them.
    Sibling,
    /// A direct child of the selected node. Drawn on the second row, indented
    /// to start under the selected chip.
    Child,
}

/// How a child is attached to the focused orchestrator. Kept on the chip so the
/// detail toast can say *why* something is on the bar — a live job and a
/// declared team affiliation are very different claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// `dispatch.json` — a routed task is in flight right now.
    Dispatch,
    /// `harness.json → team` — declared, persistent affiliation.
    Team,
    /// `last-result.json` — the most recent job came from this parent.
    History,
}

/// Live condition of the harness behind a chip.
///
/// The vocabulary is the fleet's own (`statusOf`, go/lifecycle.go:30 →
/// stopped|starting|idle|busy|refresh-queued) merged with herdr's pane truth,
/// exactly as the brief §6 prescribes: *files = fleet truth, herdr events =
/// pane truth*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChipStatus {
    /// Stuck on an interactive prompt — the silent-stall failure mode the brief
    /// says to surface LOUDLY. Ranks above every other state.
    Attention,
    /// Supervisor alive, no idle marker: working.
    Busy,
    /// Supervisor alive, idle marker present: waiting for input.
    Idle,
    /// Spawn claimed but the supervisor has not reported yet.
    Starting,
    /// A refresh is queued (`state/refresh-requested`).
    RefreshQueued,
    /// Last job finished successfully.
    Done,
    /// Last job failed.
    Failed,
    /// No runtime process. Reaped children live here and must stay on the bar.
    Stopped,
    /// Present, but nothing said anything about it.
    Unknown,
}

impl ChipStatus {
    /// The dot glyph is shared by the renderer and the detail toast, so a chip
    /// and its explanation can never disagree about what was shown.
    pub fn dot(self) -> &'static str {
        match self {
            ChipStatus::Attention => "\u{25C6}",     // ◆ needs a human
            ChipStatus::Busy => "\u{25CF}",          // ●
            ChipStatus::Idle => "\u{25CB}",          // ○
            ChipStatus::Starting => "\u{25D4}",      // ◔
            ChipStatus::RefreshQueued => "\u{21BB}", // ↻
            ChipStatus::Done => "\u{2713}",          // ✓
            ChipStatus::Failed => "\u{2717}",        // ✗
            ChipStatus::Stopped => "\u{00B7}",       // ·
            ChipStatus::Unknown => "\u{2022}",       // •
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ChipStatus::Attention => "attention",
            ChipStatus::Busy => "busy",
            ChipStatus::Idle => "idle",
            ChipStatus::Starting => "starting",
            ChipStatus::RefreshQueued => "refresh-queued",
            ChipStatus::Done => "done",
            ChipStatus::Failed => "failed",
            ChipStatus::Stopped => "stopped",
            ChipStatus::Unknown => "unknown",
        }
    }

    /// A child that is doing nothing has no claim on the operator's eye; one
    /// that is blocked has the strongest. Used for styling weight.
    pub fn is_alarming(self) -> bool {
        matches!(self, ChipStatus::Attention | ChipStatus::Failed)
    }

    /// Greyed-out states: present and attached, but not running.
    pub fn is_dormant(self) -> bool {
        matches!(
            self,
            ChipStatus::Stopped | ChipStatus::Done | ChipStatus::Unknown
        )
    }
}

/// One rendered chip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chip {
    /// Harness name — what the operator calls this agent.
    pub name: String,
    pub role: ChipRole,
    pub status: ChipStatus,
    /// How this child hangs off the focused orchestrator. `None` for the
    /// lineage chips, which are ancestors rather than attachments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<EdgeKind>,
    /// Tab to focus on click. `None` when the harness has no tab in THIS herdr
    /// instance (a `light` child that never had a pane, a reaped one, or a
    /// stale `state/herdr-tab` from another instance) — the chip then reports
    /// instead of navigating, rather than focusing a stranger's tab.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// The agent's session id — durable across pane relaunches. Used to keep
    /// the same chip and codename when a pane is relaunched with a new pane_id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// True when the tab is hidden and a click must `tab.show` rather than
    /// `tab.focus`. Always false until the host's create-hidden work lands;
    /// carried now so phase 2 is a collector change, not a protocol change.
    #[serde(default)]
    pub hidden: bool,
    /// The dispatched task, for the detail toast.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Last `JOB_TAIL` chars of the job id. Two dispatches to the same child
    /// are otherwise indistinguishable on the bar (brief §7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_tail: Option<String>,
    /// `harness.json → exec`. `light` children run inside `conductor` and have
    /// no PTY of their own, so their chip can only ever report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<String>,
    /// Runtime declared by `harness.json` (claude/codex/pi/jcode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// `<harness>/workspace`, canonicalised. Carried onto the chip so the
    /// context-menu "Copy metadata" action can report a folder without a
    /// second round trip to the collector's own node table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// The instance's canonical synchronized name — session title = pane
    /// label = tab name (titlesync, harness-fleet bcfd73f/51b3323) — resolved
    /// pane label first, then the pane's stripped terminal title, falling
    /// back to `name` (the harness identity) when neither is set. This is
    /// what the bar DRAWS; `name` stays the harness identity for everything
    /// else (Copy metadata, lineage lookups).
    #[serde(default)]
    pub display_name: String,
    /// The instance codename — the unique id the fleet gave this pane, as
    /// displayed (`"Bulbasur"`). This is what the bar DRAWS: the spec asks for
    /// the bare id on the chip, with the real name revealed on hover.
    ///
    /// Empty for a pane the fleet has not named (titlesync not running, or a
    /// pane it deliberately leaves alone), in which case the chip falls back
    /// to [`Chip::label`]'s older behaviour and shows the instance name.
    #[serde(default)]
    pub codename: String,
    /// The real name behind the codename — `display_name` with the codename
    /// and lineage prefix taken off. This is the tooltip's text.
    ///
    /// Carried as its own field rather than re-parsed in the guest so the two
    /// halves of a name are decided ONCE, by the process that can also consult
    /// the fleet's ledger. A guest that re-derived it would be a second
    /// implementation of the grammar with less information than the first.
    #[serde(default)]
    pub instance_title: String,
    /// This child's dispatch has FINISHED and the chip is being held on the bar
    /// through its retention grace (see `DISPATCH_GRACE` in the collector).
    ///
    /// Rendered greyed. A short job — a `codex-gp` dispatch answers in about
    /// seven seconds — otherwise appears and vanishes between two glances, so
    /// the bar reports that nothing happened at exactly the moment something
    /// did. The chip is dimmed rather than dropped so "ran recently" and
    /// "running now" stay tellable apart at a glance.
    #[serde(default)]
    pub finished: bool,
}

impl Chip {
    /// A chip navigates only when we hold a tab id validated against this
    /// instance. Rendering a dead click target is worse than rendering none.
    pub fn clickable(&self) -> bool {
        self.tab_id.is_some()
    }

    /// What the bar DRAWS for this chip.
    ///
    /// The instance codename — the id — because the bar is a navigation row of
    /// buttons packed shoulder to shoulder, and an id is short, stable and
    /// unique, while an instance name is long, changes under the cursor every
    /// time the agent retitles itself, and is frequently the same words as its
    /// brother's. The real name is one hover away ([`Chip::detail`]).
    ///
    /// Falls back through the resolved instance name to the harness identity,
    /// so a pane the fleet has not named — or a snapshot from a collector that
    /// predates codenames — still draws something rather than a blank chip.
    pub fn label(&self) -> &str {
        if !self.codename.is_empty() {
            &self.codename
        } else if !self.display_name.is_empty() {
            &self.display_name
        } else {
            &self.name
        }
    }

    /// What the hover tooltip says: the real name behind the id.
    ///
    /// Falls back the same way `label` does, and returns `""` only for a chip
    /// with nothing at all to say — which the renderer treats as "no detail
    /// available" rather than drawing an empty tooltip.
    pub fn detail(&self) -> &str {
        if !self.instance_title.is_empty() {
            &self.instance_title
        } else if !self.display_name.is_empty() && self.display_name != self.codename {
            &self.display_name
        } else if self.name != self.codename {
            &self.name
        } else {
            ""
        }
    }

    /// The chip's full identity for a tooltip line: the id, its lineage, and
    /// the real name. The lineage is deliberately absent from the CHIP (spec
    /// 4: "do NOT show the `Hexagon >` prefix — just `Bulbasur`") but belongs
    /// in the detail, which is where a question about parentage gets answered.
    pub fn tooltip(&self) -> String {
        let name = self.label();
        match self.detail() {
            "" => name.to_string(),
            detail => format!("{name} — {detail}"),
        }
    }
}

/// Why the bar is empty. Shown in the toast, never on the strip — an empty bar
/// should occupy no space, but "why" must still be answerable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmptyReason {
    /// The focused pane is not attributable to any fleet harness or profile.
    NotTidus,
    /// No fleet at all under `$HARNESS_ROOT`.
    NoFleet,
}

/// The whole snapshot the guest renders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BarInput {
    #[serde(default)]
    pub schema: u32,
    /// `$HOME`, so the guest can shorten paths without reading the environment
    /// (it has none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// The focus event was the plugin's own surface, not a pane. The guest
    /// repaints its previous snapshot instead of blanking, so clicking a chip
    /// does not erase the bar you just clicked.
    #[serde(default)]
    pub self_focus: bool,
    /// Harness or profile the focused pane runs inside, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_harness: Option<String>,
    /// Ancestors (root-most first), then the selected node, then its brothers,
    /// then its children. The order IS the render order, and click handling
    /// addresses this vector by index, so a producer must not reshuffle it.
    #[serde(default)]
    pub chips: Vec<Chip>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_reason: Option<EmptyReason>,
    /// How the focused pane was attributed, for the toast. An attribution you
    /// cannot argue with is one you can only disbelieve.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// Collector wall time, microseconds. The brief's budget is 50 ms and a
    /// claim like that should be checkable from the artefact itself.
    #[serde(default)]
    pub collect_us: u64,
}

impl Default for BarInput {
    fn default() -> Self {
        Self {
            schema: SCHEMA,
            home: None,
            self_focus: false,
            focus_harness: None,
            chips: Vec::new(),
            empty_reason: Some(EmptyReason::NotTidus),
            evidence: None,
            collect_us: 0,
        }
    }
}

impl BarInput {
    /// Nothing to draw. The guest emits no layer at all in this case so the
    /// host carves no band — an empty row of background is not neutral, it is
    /// a permanently stolen line.
    pub fn is_empty(&self) -> bool {
        self.chips.is_empty()
    }

    pub fn children(&self) -> impl Iterator<Item = &Chip> {
        self.chips.iter().filter(|c| c.role == ChipRole::Child)
    }

    /// The brothers of the selected node. Named separately from `lineage` so a
    /// caller cannot accidentally treat them as ancestors: they share the top
    /// row, but a brother is a click target that re-roots the bar, while an
    /// ancestor is a step back up it.
    pub fn siblings(&self) -> impl Iterator<Item = &Chip> {
        self.chips.iter().filter(|c| c.role == ChipRole::Sibling)
    }

    pub fn lineage(&self) -> impl Iterator<Item = &Chip> {
        self.chips
            .iter()
            .filter(|c| matches!(c.role, ChipRole::Ancestor | ChipRole::Current))
    }

    /// The loudest thing on the bar, for the summary in the detail toast.
    /// Brothers count: they are on the bar and clickable, so a blocked one is
    /// news whether or not it is the selected node's own child.
    pub fn needs_attention(&self) -> bool {
        self.children()
            .chain(self.siblings())
            .any(|c| c.status == ChipStatus::Attention)
    }
}

/// Last [`JOB_TAIL`] characters of a job id, which is what disambiguates two
/// dispatches to the same child.
pub fn job_tail(id: &str) -> Option<String> {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        return None;
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let start = chars.len().saturating_sub(JOB_TAIL);
    Some(chars[start..].iter().collect())
}

/// Replace a `$HOME` prefix with `~`. Shared so the collector and the toast
/// shorten paths identically.
pub fn tilde(path: &str, home: &str) -> String {
    if home.is_empty() || !path.starts_with(home) {
        return path.to_string();
    }
    let rest = &path[home.len()..];
    if rest.is_empty() {
        return "~".to_string();
    }
    if let Some(stripped) = rest.strip_prefix('/') {
        return format!("~/{stripped}");
    }
    path.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chip(name: &str, role: ChipRole) -> Chip {
        Chip {
            name: name.to_string(),
            role,
            status: ChipStatus::Idle,
            edge: None,
            tab_id: Some("w1:t2".into()),
            pane_id: None,
            session_id: None,
            hidden: false,
            finished: false,
            task: None,
            job_tail: None,
            exec: None,
            runtime: None,
            folder: None,
            display_name: name.to_string(),
            codename: String::new(),
            instance_title: String::new(),
        }
    }

    /// The bar draws the ID and hovers the NAME — the whole point of the
    /// codename layer, pinned at the model so neither the collector nor the
    /// renderer can quietly invert it.
    #[test]
    fn a_named_chip_draws_its_id_and_hovers_its_real_name() {
        let mut c = chip("worker", ChipRole::Child);
        c.display_name = "Hexagon > Bulbasur (add tab to context menu)".into();
        c.codename = "Bulbasur".into();
        c.instance_title = "add tab to context menu".into();
        assert_eq!(c.label(), "Bulbasur", "the chip carries the bare id");
        assert_eq!(c.detail(), "add tab to context menu");
        assert_eq!(c.tooltip(), "Bulbasur — add tab to context menu");
    }

    /// A collector that predates codenames, or a pane the fleet has not named:
    /// the bar must still draw something useful rather than a blank chip.
    #[test]
    fn an_unnamed_chip_falls_back_to_the_instance_name_then_the_harness() {
        let mut c = chip("worker", ChipRole::Child);
        c.display_name = "Ship the daemon".into();
        assert_eq!(c.label(), "Ship the daemon");
        assert_eq!(c.detail(), "Ship the daemon");

        c.display_name = String::new();
        assert_eq!(c.label(), "worker");
        // Nothing to add: a tooltip repeating the chip is noise, not detail.
        assert_eq!(c.detail(), "worker");
    }

    #[test]
    fn lineage_siblings_and_children_partition_the_chips() {
        let input = BarInput {
            chips: vec![
                chip("main", ChipRole::Ancestor),
                chip("mid", ChipRole::Current),
                chip("bro", ChipRole::Sibling),
                chip("kid", ChipRole::Child),
            ],
            ..Default::default()
        };
        assert_eq!(input.lineage().count(), 2);
        assert_eq!(input.siblings().count(), 1);
        assert_eq!(input.children().count(), 1);
        // Every chip lands in exactly one bucket — a brother counted as lineage
        // would be drawn as an ancestor and click like one.
        assert_eq!(
            input.lineage().count() + input.siblings().count() + input.children().count(),
            input.chips.len()
        );
        assert!(!input.is_empty());
    }

    #[test]
    fn a_chip_without_a_validated_tab_is_not_clickable() {
        let mut c = chip("kid", ChipRole::Child);
        assert!(c.clickable());
        c.tab_id = None;
        assert!(!c.clickable());
    }

    #[test]
    fn attention_outranks_everything_and_is_alarming() {
        assert!(ChipStatus::Attention.is_alarming());
        assert!(ChipStatus::Failed.is_alarming());
        assert!(!ChipStatus::Busy.is_alarming());
        // A reaped child is dormant, not alarming: it is expected, and the
        // brief wants it greyed and still attached.
        assert!(ChipStatus::Stopped.is_dormant());
        assert!(ChipStatus::Done.is_dormant());
        assert!(!ChipStatus::Idle.is_dormant());
    }

    #[test]
    fn attention_is_reported_at_bar_level() {
        let mut kid = chip("kid", ChipRole::Child);
        kid.status = ChipStatus::Attention;
        let input = BarInput {
            chips: vec![chip("main", ChipRole::Current), kid],
            ..Default::default()
        };
        assert!(input.needs_attention());

        // A blocked BROTHER is equally on the bar and equally stuck.
        let mut bro = chip("bro", ChipRole::Sibling);
        bro.status = ChipStatus::Attention;
        let input = BarInput {
            chips: vec![chip("main", ChipRole::Current), bro],
            ..Default::default()
        };
        assert!(input.needs_attention());
    }

    #[test]
    fn job_tail_takes_the_last_six_and_survives_short_ids() {
        assert_eq!(
            job_tail("main-1786360945768895251").as_deref(),
            Some("895251")
        );
        assert_eq!(job_tail("abc").as_deref(), Some("abc"));
        assert_eq!(job_tail("  "), None);
        assert_eq!(job_tail(""), None);
    }

    #[test]
    fn tilde_shortens_only_a_real_home_prefix() {
        assert_eq!(tilde("/home/a/x", "/home/a"), "~/x");
        assert_eq!(tilde("/home/a", "/home/a"), "~");
        // A sibling directory that merely starts with the same bytes must not
        // be rewritten into someone else's home.
        assert_eq!(tilde("/home/andual2/x", "/home/andual"), "/home/andual2/x");
        assert_eq!(tilde("/etc/x", "/home/a"), "/etc/x");
    }

    #[test]
    fn every_status_has_a_distinct_dot() {
        let all = [
            ChipStatus::Attention,
            ChipStatus::Busy,
            ChipStatus::Idle,
            ChipStatus::Starting,
            ChipStatus::RefreshQueued,
            ChipStatus::Done,
            ChipStatus::Failed,
            ChipStatus::Stopped,
            ChipStatus::Unknown,
        ];
        let mut dots: Vec<&str> = all.iter().map(|s| s.dot()).collect();
        dots.sort_unstable();
        dots.dedup();
        assert_eq!(dots.len(), all.len(), "status dots must be distinguishable");
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let mut kid = chip("kid", ChipRole::Child);
        kid.edge = Some(EdgeKind::Team);
        kid.job_tail = Some("895251".into());
        kid.exec = Some("light".into());
        let input = BarInput {
            focus_harness: Some("main".into()),
            chips: vec![chip("main", ChipRole::Current), kid],
            empty_reason: None,
            ..Default::default()
        };
        let json = serde_json::to_string(&input).expect("encode");
        let back: BarInput = serde_json::from_str(&json).expect("decode");
        assert_eq!(input, back);
    }
}
