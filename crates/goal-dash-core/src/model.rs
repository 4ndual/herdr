//! Data model ported VERBATIM from goal-widget's `main.rs` (~lines 24–400): the goal tree
//! (`GoalState`, `Sub`, `Tree`), the consolidated portable document (`GoalDoc`), `Proposal`,
//! `GoalConstraint`, `PromptEntry`, the session→goal link index (`Links`), and the small
//! pure helpers (`norm`, `clip_inline`, `now_stamp`, parent/state enums). No logic changed.

use serde::{Deserialize, Serialize};

// ----------------------------- shared state -----------------------------

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct GoalState {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub source: String, // "agent" | "user"
    #[serde(default)]
    pub approved: bool,
    /// For sub-goals: "active"|"paused"|"done"|"discarded". Empty for main/current goals.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub state: String,
    #[serde(rename = "updatedAt", default)]
    pub updated_at: String,
    /// Why this change happened (set by the Goal Observer when it auto-changes CURRENT or a sub).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// When this goal TEXT became current (RFC3339).
    #[serde(rename = "setAt", default, skip_serializing_if = "String::is_empty")]
    pub set_at: String,
    /// Estimated minutes to reach this goal — sourced from the time-gate plan's p50 (or a user override).
    #[serde(
        rename = "estMinutes",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub est_minutes: Option<u32>,
    /// Deadline (RFC3339) the goal should be reached by — set_at + the plan's p90 (or a user override).
    #[serde(rename = "deadline", default, skip_serializing_if = "String::is_empty")]
    pub deadline: String,
    /// User pinned the time fields → infer/observe stops touching them.
    #[serde(rename = "timePinned", default, skip_serializing_if = "is_false")]
    pub time_pinned: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

pub fn now_stamp() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%S%z")
        .to_string()
}

#[allow(dead_code)] // referenced by #[cfg(test)] units below
pub const SUB_STATES: [&str; 4] = ["active", "paused", "done", "discarded"];
pub const SUB_PARENTS: [&str; 3] = ["current", "main", "independent"];

/// Hard ceiling for any single non-collapsible text the agent may define.
pub const MAX_INLINE: usize = 240;

/// Clip an agent-defined inline string to MAX_INLINE chars (char-safe; never splits a multibyte char).
pub fn clip_inline(s: &str) -> String {
    s.chars().take(MAX_INLINE).collect()
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Sub {
    pub id: u64,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub source: String, // agent | user
    #[serde(default)]
    pub approved: bool,
    #[serde(default = "default_active")]
    pub state: String, // active | paused | done | discarded
    #[serde(default = "default_current")]
    pub parent: String, // current | main | independent
    #[serde(rename = "createdAt", default)]
    pub created_at: String,
    #[serde(rename = "updatedAt", default)]
    pub updated_at: String,
    /// User ownership pin: any manual action sets it true; the Goal Observer then leaves this sub alone.
    #[serde(default)]
    pub pinned: bool,
}
fn default_active() -> String {
    "active".into()
}
fn default_current() -> String {
    "current".into()
}

impl Sub {
    /// The goal-fields snapshot logged to this sub's history.
    pub fn snapshot(&self) -> GoalState {
        GoalState {
            text: self.text.clone(),
            source: self.source.clone(),
            approved: self.approved,
            state: self.state.clone(),
            updated_at: self.updated_at.clone(),
            reason: None,
            ..Default::default()
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Tree {
    #[serde(default)]
    pub current: GoalState,
    #[serde(default)]
    pub subs: Vec<Sub>,
    #[serde(default = "one_u64")]
    pub next_id: u64,
}
pub fn one_u64() -> u64 {
    1
}

/// Normalize goal text for dedup (case/space-insensitive).
pub fn norm(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

// ----------------------- portable goal store doc ---------------------------

pub const GOAL_FILE: &str = ".goal.json";

fn schema_v() -> u32 {
    1
}

/// The consolidated goal document — the standard portable artifact.
#[derive(Serialize, Deserialize, Clone)]
pub struct GoalDoc {
    #[serde(rename = "goalId", default)]
    pub goal_id: String,
    #[serde(default = "schema_v")]
    pub schema: u32,
    #[serde(default)]
    pub main: GoalState,
    #[serde(default)]
    pub current: GoalState,
    #[serde(default)]
    pub subs: Vec<Sub>,
    #[serde(rename = "nextId", default = "one_u64")]
    pub next_id: u64,
    /// Pending agent suggestions the user must accept/dismiss. Never auto-applied.
    #[serde(default)]
    pub proposals: Vec<Proposal>,
    /// Normalized texts of dismissed proposals.
    #[serde(default)]
    pub dismissed: Vec<String>,
    /// The latest prompt's ≤240-char paraphrase, shown in the footer.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub paraphrase: String,
    /// Last transcript line index the observer has processed (watcher cursor).
    #[serde(rename = "lastTurn", default)]
    pub last_turn: u64,
    /// Per-goal bounds. User-authored persist; agent suggestions arrive as Proposals.
    #[serde(default)]
    pub constraints: Vec<GoalConstraint>,
    /// Optional stable project identifier for plans scoping.
    #[serde(
        rename = "projectKey",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub project_key: String,
}
impl Default for GoalDoc {
    fn default() -> Self {
        GoalDoc {
            goal_id: String::new(),
            schema: 1,
            main: GoalState::default(),
            current: GoalState::default(),
            subs: Vec::new(),
            next_id: 1,
            proposals: Vec::new(),
            dismissed: Vec::new(),
            paraphrase: String::new(),
            last_turn: 0,
            constraints: Vec::new(),
            project_key: String::new(),
        }
    }
}

/// A pending agent suggestion (high-blast-radius change the user must confirm).
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Proposal {
    #[serde(default)]
    pub kind: String, // "main" | "current" | "constraint:main" | "constraint:current"
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(rename = "ts", default)]
    pub ts: String,
}

/// One bound on a goal. `entity` = "main" | "current" | "sub-<id>". `source` = "user" | "agent".
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct GoalConstraint {
    #[serde(default)]
    pub entity: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub source: String,
    #[serde(rename = "ts", default)]
    pub ts: String,
}

/// One entry in the per-session prompt-log (the "did it understand me" trail).
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct PromptEntry {
    #[serde(default)]
    pub ts: String,
    #[serde(default)]
    pub paraphrase: String,
    /// noop | current | sub | main | mixed — drives the marker in the prompt-log view.
    #[serde(default)]
    pub action: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

/// Global session→goal link index.
#[derive(Serialize, Deserialize, Default)]
pub struct Links {
    #[serde(default)]
    pub sessions: std::collections::HashMap<String, String>, // sid -> goalId
    #[serde(default)]
    pub goals: std::collections::HashMap<String, String>, // goalId -> abs path
}

// ----------------------- parent / state enums ---------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Par {
    Current,
    Main,
    Indep,
}
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum St {
    Active,
    Paused,
    Done,
    Discarded,
}
pub const ST_ORDER: [St; 4] = [St::Active, St::Paused, St::Done, St::Discarded];

pub fn par_str(p: Par) -> &'static str {
    match p {
        Par::Current => "current",
        Par::Main => "main",
        Par::Indep => "independent",
    }
}
pub fn par_of(s: &str) -> Par {
    match s {
        "main" => Par::Main,
        "independent" => Par::Indep,
        _ => Par::Current,
    }
}
pub fn st_str(s: St) -> &'static str {
    match s {
        St::Active => "active",
        St::Paused => "paused",
        St::Done => "done",
        St::Discarded => "discarded",
    }
}
pub fn st_of(s: &str) -> St {
    match s {
        "paused" => St::Paused,
        "done" => St::Done,
        "discarded" => St::Discarded,
        _ => St::Active,
    }
}
pub fn st_label(s: St) -> &'static str {
    match s {
        St::Active => "Active",
        St::Paused => "Paused",
        St::Done => "Done",
        St::Discarded => "Discarded",
    }
}

pub fn normalize_parent(s: &str) -> String {
    par_str(par_of(s)).to_string()
}

/// True when a goal has real (non-placeholder) text.
pub fn has_real(text: &str) -> bool {
    !text.is_empty() && !text.starts_with('(')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn norm_dedups_case_and_spacing() {
        assert_eq!(norm("  Ship  The   Widget "), "ship the widget");
        assert_eq!(norm("ship the widget"), norm("SHIP THE WIDGET"));
        assert_ne!(norm("ship a"), norm("ship b"));
    }

    #[test]
    fn state_and_parent_round_trip() {
        for s in SUB_STATES {
            assert_eq!(st_str(st_of(s)), s);
        }
        for p in SUB_PARENTS {
            assert_eq!(par_str(par_of(p)), p);
        }
        assert_eq!(st_str(st_of("bogus")), "active");
        assert_eq!(par_str(par_of("bogus")), "current");
    }

    #[test]
    fn tree_serde_round_trip() {
        let t = Tree {
            current: GoalState {
                text: "c".into(),
                source: "agent".into(),
                approved: false,
                state: String::new(),
                updated_at: "t".into(),
                reason: None,
                ..Default::default()
            },
            subs: vec![Sub {
                id: 3,
                text: "s".into(),
                source: "user".into(),
                approved: true,
                state: "done".into(),
                parent: "main".into(),
                created_at: "x".into(),
                updated_at: "y".into(),
                pinned: false,
            }],
            next_id: 4,
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: Tree = serde_json::from_str(&json).unwrap();
        assert_eq!(back.next_id, 4);
        assert_eq!(back.subs[0].state, "done");
        assert_eq!(back.subs[0].parent, "main");
        assert_eq!(back.current.text, "c");
    }

    #[test]
    fn links_serde_round_trip() {
        let mut l = Links::default();
        l.sessions.insert("sid-1".into(), "goal-9".into());
        l.goals.insert("goal-9".into(), "/p/.goal.json".into());
        let json = serde_json::to_string(&l).unwrap();
        let back: Links = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.sessions.get("sid-1").map(String::as_str),
            Some("goal-9")
        );
        assert_eq!(
            back.goals.get("goal-9").map(String::as_str),
            Some("/p/.goal.json")
        );
        let empty: Links = serde_json::from_str("{}").unwrap();
        assert!(empty.sessions.is_empty() && empty.goals.is_empty());
    }
}
