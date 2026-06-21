//! The goal Model (goal-widget's `App`), carved from `src/main.rs` (~2106–3095).
//! Pure state + data/view methods + action handlers. The cmux focus layer is
//! replaced by [`Model::refresh`] taking a [`FocusInput`] from herdr, and the
//! `claude`-spawning / pane-driving actions push [`Effect`]s onto `self.effects`
//! for the host to run (instead of detached `spawn_*` re-execs).

// The Model's full field/method surface is consumed by the herdr host wiring
// (Phase 3) and the renderer; until the host lands, some accessors read as dead.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use ratatui::layout::Rect;

use dash_common::{Effect, FocusInput, HeadlessMode};

use crate::context::{compose_rebrief, rebrief_read_prompt, user_prompts};
use crate::infer::{has_new_user_prompt, transcript_line_count};
use crate::model::{
    has_real, norm, now_stamp, par_of, st_label, st_of, st_str, GoalConstraint, GoalDoc, GoalState,
    Par, PromptEntry, Proposal, St, Sub, Tree, ST_ORDER, SUB_PARENTS,
};
use crate::render::{goal_lines, MAIN_ICON, MAIN_LABEL};
use crate::store::{most_recent_session, move_goal_store, session_cwd, session_jsonl, GoalStore};
use crate::timegate::{
    add_minutes, fill_estimate, list_project_plans, parse_duration_min, plans_scope_key,
    plans_watermark, project_goal_file, short_time, state_dir, PlanMeta,
};
use crate::view::{Act, ChipSpec, EditTarget, GoalAction, HistTarget, Node, View};

pub struct App {
    pub(crate) current_sid: Option<String>,
    /// Effects queued by action handlers for the host to run (claude spawns,
    /// pane key sends). Drained by `take_effects()` after each `handle`.
    pub(crate) effects: Vec<Effect>,
    pub(crate) pinned_sid: Option<String>,
    pub(crate) focused: FocusInput,
    pub(crate) state: Option<GoalState>, // the MAIN goal (in-memory; persisted into the consolidated GoalDoc)
    pub(crate) tree: Tree,               // current goal + sub-goals (in-memory)
    pub(crate) store: GoalStore,         // the resolved goal file for current_sid
    pub(crate) goal_mtime: Option<SystemTime>,
    // Goal Observer state (carried so a persist never drops it)
    pub(crate) proposals: Vec<Proposal>,
    pub(crate) dismissed: Vec<String>,
    pub(crate) paraphrase: String,
    pub(crate) last_turn: u64,
    pub(crate) observe_pending: Option<u64>, // transcript line count an in-flight observer was dispatched for
    pub(crate) observe_at: Option<Instant>,  // when that dispatch happened (dead-child timeout)
    pub(crate) sort_pending: Option<Instant>, // a `--sort` subcommand is in flight; poll re-resolves the moved store
    pub(crate) inferred: std::collections::HashSet<String>, // sids whose MAIN goal we kicked infer for
    pub(crate) inferred_tree: std::collections::HashSet<String>, // sids whose tree we kicked infer for
    pub(crate) observed_baseline: std::collections::HashSet<String>, // sids whose observe-cursor we baselined on first sight
    pub(crate) status: String,
    // tree view
    pub(crate) nodes: Vec<Node>, // visible selectable rows, rebuilt every draw
    pub(crate) node_rects: Vec<(Rect, usize)>, // screen rect -> index into `nodes`
    pub(crate) cons_header_rects: Vec<(Rect, String)>, // screen rect of a `▸/▾ constraints (N)` row -> entity key
    pub(crate) sel_node: usize,
    pub(crate) sel_ctrl: usize, // selected chip under the current node (0..node_actions(sel).len())
    pub(crate) controls: Vec<(Rect, Act)>, // clickable session-bar rects
    pub(crate) chip_rects: Vec<(Rect, Act)>, // clickable contextual-chip rects under the selected node
    pub(crate) toggled: std::collections::HashSet<(Par, St)>, // sections flipped from their default collapse
    pub(crate) constraints: Vec<GoalConstraint>, // per-goal bounds (carried so a persist never drops them)
    pub(crate) cons_expanded: std::collections::HashSet<String>, // entity keys whose constraints section is unfolded
    pub(crate) cons_sel: Option<(String, usize)>, // (entity, index) of the constraint highlighted for delete
    // plans section (read-only, lives in the goal pane below the tree)
    pub(crate) project_key: String, // stable project id from the goal doc (plans scoping)
    pub(crate) plans_label: String, // resolved project key for display; "" ⇒ hide section
    pub(crate) plans: Vec<PlanMeta>, // project-scoped plans, newest first; rebuilt on reload
    pub(crate) plans_section_open: bool, // whole-section fold (▾ plans / ▸ plans)
    pub(crate) plans_expanded: std::collections::HashSet<String>, // basenames whose body is unfolded
    pub(crate) plans_current_folded: bool, // the newest "current" plan auto-expands unless the user folds it
    pub(crate) plans_body: std::collections::HashMap<String, String>, // lazily-read bodies of expanded plans
    pub(crate) plans_head_rect: Option<Rect>, // click rect of the `▾/▸ plans` header
    pub(crate) plans_row_rects: Vec<(Rect, String)>, // click rect of a compact plan row -> basename
    pub(crate) plans_mtime: Option<SystemTime>, // ~/.claude/plans dir mtime; poll re-globs when it changes
    // editing
    pub(crate) editing: bool,
    pub(crate) edit_buf: String,
    pub(crate) edit_target: EditTarget,
    // history view
    pub(crate) view: View,
    pub(crate) history: Vec<GoalState>,
    pub(crate) hist_rects: Vec<(Rect, usize)>,
    pub(crate) sel_hist: usize,
    pub(crate) hist_target: HistTarget,
    pub(crate) sel_prop: usize, // selected proposal in the Proposals view
    pub(crate) prop_expanded: std::collections::HashSet<usize>, // proposal indices whose reason dropdown is open
    pub(crate) plog: Vec<PromptEntry>, // loaded prompt-log for the PromptLog view
    pub(crate) plog_rects: Vec<(Rect, usize)>, // click rects for prompt-log rows
    pub(crate) prop_rects: Vec<(Rect, usize)>, // click rects for proposal rows
    pub(crate) banner_rect: Option<Rect>, // click rect for the proposals banner on the tree
    /// Hover position (col,row, 0-based) from the latest mouse Moved event; drives tooltips.
    pub(crate) hover: Option<(u16, u16)>,
    /// Hover regions → full text: a one-line-truncated row (MAIN goal, paraphrase footer) registers
    /// its rect + full text here so hovering pops the complete content in a floating tooltip.
    pub(crate) tip_rects: Vec<(Rect, String)>,
}

/// Sections are collapsed by default for Done/Discarded; the `toggled` set flips that.
/// All sub-sections start COLLAPSED — the pane opens short; the user expands what they need. (Was
/// Done|Discarded only; Active/Paused now collapse too per the compression redesign.) `is_collapsed`
/// XORs this with `toggled`, so toggling still opens any section.
fn default_collapsed(_st: St) -> bool {
    true
}

impl App {
    pub fn new(pinned_sid: Option<String>) -> Self {
        App {
            current_sid: None,
            effects: Vec::new(),
            pinned_sid,
            focused: FocusInput::default(),
            state: None,
            tree: Tree::default(),
            store: GoalStore::default(),
            goal_mtime: None,
            proposals: Vec::new(),
            dismissed: Vec::new(),
            paraphrase: String::new(),
            last_turn: 0,
            observe_pending: None,
            observe_at: None,
            sort_pending: None,
            inferred: std::collections::HashSet::new(),
            inferred_tree: std::collections::HashSet::new(),
            observed_baseline: std::collections::HashSet::new(),
            status: "resolving focused session…".into(),
            nodes: Vec::new(),
            node_rects: Vec::new(),
            cons_header_rects: Vec::new(),
            sel_node: 0,
            sel_ctrl: 0,
            controls: Vec::new(),
            chip_rects: Vec::new(),
            toggled: std::collections::HashSet::new(),
            constraints: Vec::new(),
            cons_expanded: std::collections::HashSet::new(),
            cons_sel: None,
            project_key: String::new(),
            plans_label: String::new(),
            plans: Vec::new(),
            plans_section_open: true,
            plans_expanded: std::collections::HashSet::new(),
            plans_current_folded: false,
            plans_body: std::collections::HashMap::new(),
            plans_head_rect: None,
            plans_row_rects: Vec::new(),
            plans_mtime: None,
            editing: false,
            edit_buf: String::new(),
            edit_target: EditTarget::Main,
            view: View::Tree,
            history: Vec::new(),
            hist_rects: Vec::new(),
            sel_hist: 0,
            hist_target: HistTarget::Main,
            sel_prop: 0,
            prop_expanded: std::collections::HashSet::new(),
            plog: Vec::new(),
            plog_rects: Vec::new(),
            prop_rects: Vec::new(),
            banner_rect: None,
            hover: None,
            tip_rects: Vec::new(),
        }
    }

    // ---- proposals (MAIN / user-owned-CURRENT suggestions the user accepts or dismisses) ----
    pub(crate) fn open_proposals(&mut self) {
        if self.proposals.is_empty() {
            self.status = "no proposals".into();
            return;
        }
        self.sel_prop = 0;
        self.prop_expanded.clear(); // every dropdown starts collapsed (compressed)
        self.view = View::Proposals;
    }
    /// Toggle the reason-dropdown of the selected proposal (collapsed ⇄ expanded).
    pub(crate) fn toggle_proposal(&mut self) {
        let idx = self.sel_prop;
        if idx >= self.proposals.len() {
            return;
        }
        if !self.prop_expanded.remove(&idx) {
            self.prop_expanded.insert(idx);
        }
    }
    /// Accept the selected proposal: apply it as a user-owned change, and remember it's resolved.
    pub(crate) fn accept_proposal(&mut self, idx: usize) {
        if idx >= self.proposals.len() {
            return;
        }
        let p = self.proposals.remove(idx);
        match p.kind.as_str() {
            "main" => self.set_main(&p.text, "user", true),
            "current" => self.set_current_goal(&p.text, "user", true),
            // Agent-suggested constraints land in the goal's list (now user-owned) only on accept.
            "constraint:main" => self.constraints.push(GoalConstraint {
                entity: "main".into(),
                text: p.text.clone(),
                source: "user".into(),
                ts: now_stamp(),
            }),
            "constraint:current" => self.constraints.push(GoalConstraint {
                entity: "current".into(),
                text: p.text.clone(),
                source: "user".into(),
                ts: now_stamp(),
            }),
            _ => {}
        }
        self.dismissed.push(norm(&p.text)); // never re-surface the same idea
        self.persist();
        self.prop_expanded.clear(); // indices shifted on removal — re-collapse all
        self.sel_prop = self.sel_prop.min(self.proposals.len().saturating_sub(1));
        if self.proposals.is_empty() {
            self.view = View::Tree;
        }
        self.status = "accepted".into();
    }
    /// Dismiss the selected proposal: drop it and remember it so the observer won't raise it again.
    pub(crate) fn dismiss_proposal(&mut self, idx: usize) {
        if idx >= self.proposals.len() {
            return;
        }
        let p = self.proposals.remove(idx);
        self.dismissed.push(norm(&p.text));
        self.persist();
        self.prop_expanded.clear(); // indices shifted on removal — re-collapse all
        self.sel_prop = self.sel_prop.min(self.proposals.len().saturating_sub(1));
        if self.proposals.is_empty() {
            self.view = View::Tree;
        }
        self.status = "dismissed".into();
    }

    // ---- time + constraints ----
    /// Entity key of the selected node when it's a goal that carries time/constraints, else None.
    pub(crate) fn selected_entity(&self) -> Option<String> {
        match self.selected()? {
            Node::Main => Some("main".into()),
            Node::Current => Some("current".into()),
            _ => None,
        }
    }

    /// `t` — begin editing the estimated time of the selected goal (deadline = set_at + estimate, pinned).
    pub(crate) fn begin_time_edit(&mut self) {
        let entity = match self.selected_entity() {
            Some(e) => e,
            None => {
                self.status = "select MAIN or CURRENT".into();
                return;
            }
        };
        let cur = if entity == "main" {
            self.state.as_ref().and_then(|s| s.est_minutes)
        } else {
            self.tree.current.est_minutes
        };
        self.edit_target = if entity == "main" {
            EditTarget::TimeMain
        } else {
            EditTarget::TimeCurrent
        };
        self.edit_buf = cur.map(|m| m.to_string()).unwrap_or_default();
        self.editing = true;
    }

    /// `C` — begin adding a user constraint to the selected goal.
    pub(crate) fn begin_constraint_edit(&mut self) {
        let entity = match self.selected_entity() {
            Some(e) => e,
            None => {
                self.status = "select MAIN or CURRENT".into();
                return;
            }
        };
        self.edit_target = if entity == "main" {
            EditTarget::ConstraintMain
        } else {
            EditTarget::ConstraintCurrent
        };
        self.edit_buf.clear();
        self.editing = true;
    }

    pub(crate) fn set_time(&mut self, entity: &str, input: &str) {
        let mins = match parse_duration_min(input) {
            Some(m) => m,
            None => {
                self.status = "bad duration — try 2h, 90m, 120".into();
                return;
            }
        };
        let apply = |st: &mut GoalState| {
            if st.set_at.is_empty() {
                st.set_at = now_stamp();
            }
            st.est_minutes = Some(mins);
            st.deadline = add_minutes(&st.set_at, mins).unwrap_or_default();
            st.time_pinned = true;
            st.updated_at = now_stamp();
        };
        if entity == "main" {
            if let Some(m) = self.state.as_mut() {
                apply(m);
            }
        } else {
            apply(&mut self.tree.current);
        }
        self.persist();
        self.status = "time set (pinned)".into();
    }

    pub(crate) fn add_constraint(&mut self, entity: &str, text: &str) {
        let t = text.trim();
        if t.is_empty() {
            return;
        }
        self.constraints.push(GoalConstraint {
            entity: entity.into(),
            text: t.into(),
            source: "user".into(),
            ts: now_stamp(),
        });
        let n = self
            .constraints
            .iter()
            .filter(|c| c.entity == entity)
            .count();
        self.cons_expanded.insert(entity.into());
        self.cons_sel = Some((entity.into(), n.saturating_sub(1)));
        self.persist();
        self.status = "constraint added".into();
    }

    /// Keep `cons_sel` consistent with the current node selection: highlight the first constraint of a
    /// selected+expanded goal, and drop the highlight when selection leaves such a goal.
    pub(crate) fn sync_cons_sel(&mut self) {
        match self.selected_entity() {
            Some(e) if self.cons_expanded.contains(&e) => {
                if self.cons_sel.as_ref().is_none_or(|(se, _)| *se != e) {
                    let n = self.cons_indices(&e).len();
                    self.cons_sel = if n > 0 { Some((e, 0)) } else { None };
                }
            }
            _ => self.cons_sel = None,
        }
    }

    /// `c` — fold/unfold the selected goal's constraints section.
    pub(crate) fn toggle_constraints(&mut self) {
        let entity = match self.selected_entity() {
            Some(e) => e,
            None => return,
        };
        self.toggle_constraints_entity(&entity);
    }
    /// Fold/unfold a specific entity's constraints section (used by `c` and by clicking the row).
    pub(crate) fn toggle_constraints_entity(&mut self, entity: &str) {
        if self.cons_expanded.remove(entity) {
            if self.cons_sel.as_ref().is_some_and(|(e, _)| e == entity) {
                self.cons_sel = None;
            }
        } else {
            let n = self
                .constraints
                .iter()
                .filter(|c| c.entity == entity)
                .count();
            self.cons_sel = if n > 0 {
                Some((entity.to_string(), 0))
            } else {
                None
            };
            self.cons_expanded.insert(entity.to_string());
        }
    }

    /// For a Main/Current node, the relative row index (within its rendered block) of the
    /// `▸/▾ constraints (N)` header + its entity key — used to make that row clickable. With the meta
    /// folded to ONE line (est·due·left), the header is now the 2nd meta line, at `goal_height + 1`.
    pub(crate) fn cons_header_info(&self, node: Node, width: usize) -> Option<(usize, String)> {
        match node {
            Node::Main => {
                let s = self.state.clone().unwrap_or_default();
                let gh = goal_lines(
                    MAIN_ICON, MAIN_LABEL, &s.text, &s.source, s.approved, width, 3, false,
                )
                .len();
                Some((gh + 1, "main".into()))
            }
            Node::Current => {
                let label =
                    if self.tree.current.source == "agent" && self.tree.current.reason.is_some() {
                        "CURRENT ↻ "
                    } else {
                        "CURRENT "
                    };
                let gh = goal_lines(
                    "◆ ",
                    label,
                    &self.tree.current.text,
                    &self.tree.current.source,
                    self.tree.current.approved,
                    width,
                    3,
                    true,
                )
                .len();
                Some((gh + 1, "current".into()))
            }
            _ => None,
        }
    }

    /// The est-line row (first meta line, at `goal_height`) + the "set X · chg Y" text shown on hover —
    /// the timestamps that used to occupy their own line now live in this tooltip. Main/Current only.
    pub(crate) fn meta_tip_info(&self, node: Node, width: usize) -> Option<(usize, String)> {
        let (gh, st) = match node {
            Node::Main => {
                let s = self.state.clone().unwrap_or_default();
                (
                    goal_lines(
                        MAIN_ICON, MAIN_LABEL, &s.text, &s.source, s.approved, width, 3, false,
                    )
                    .len(),
                    s,
                )
            }
            Node::Current => {
                let label =
                    if self.tree.current.source == "agent" && self.tree.current.reason.is_some() {
                        "CURRENT ↻ "
                    } else {
                        "CURRENT "
                    };
                (
                    goal_lines(
                        "◆ ",
                        label,
                        &self.tree.current.text,
                        &self.tree.current.source,
                        self.tree.current.approved,
                        width,
                        3,
                        true,
                    )
                    .len(),
                    self.tree.current.clone(),
                )
            }
            _ => return None,
        };
        let set = if st.set_at.is_empty() {
            "—".to_string()
        } else {
            short_time(&st.set_at)
        };
        let chg = if st.updated_at.is_empty() {
            "—".to_string()
        } else {
            short_time(&st.updated_at)
        };
        Some((gh, format!("set {set} · changed {chg}")))
    }

    /// Indices (into self.constraints) of the constraints belonging to `entity`, in display order.
    pub(crate) fn cons_indices(&self, entity: &str) -> Vec<usize> {
        self.constraints
            .iter()
            .enumerate()
            .filter(|(_, c)| c.entity == entity)
            .map(|(i, _)| i)
            .collect()
    }

    /// `d` on a goal whose constraints are expanded → delete the highlighted constraint. Returns true
    /// if it consumed the key (so the caller skips the default Discard action).
    pub(crate) fn delete_selected_constraint(&mut self) -> bool {
        let (entity, sel) = match self.cons_sel.clone() {
            Some(x) => x,
            None => return false,
        };
        if !self.cons_expanded.contains(&entity) {
            return false;
        }
        let idxs = self.cons_indices(&entity);
        let real = match idxs.get(sel) {
            Some(&r) => r,
            None => return false,
        };
        self.constraints.remove(real);
        let remaining = idxs.len() - 1;
        self.cons_sel = if remaining == 0 {
            None
        } else {
            Some((entity, sel.min(remaining - 1)))
        };
        self.persist();
        self.status = "constraint deleted".into();
        true
    }

    /// Move the constraint highlight within the expanded section. dir = +1 down / -1 up. Returns true
    /// if it consumed the navigation (selection stayed inside the constraints list).
    pub(crate) fn move_cons_sel(&mut self, dir: i32) -> bool {
        let (entity, sel) = match self.cons_sel.clone() {
            Some(x) => x,
            None => return false,
        };
        if !self.cons_expanded.contains(&entity) {
            return false;
        }
        let n = self.cons_indices(&entity).len();
        if n == 0 {
            return false;
        }
        if dir > 0 {
            if sel + 1 < n {
                self.cons_sel = Some((entity, sel + 1));
                return true;
            }
            return false; // at the end → let node navigation take over
        } else if sel > 0 {
            self.cons_sel = Some((entity, sel - 1));
            return true;
        }
        false
    }

    // ---- prompt-log (the per-prompt paraphrase trail) ----
    pub(crate) fn open_prompt_log(&mut self) {
        self.plog = self.store.read_prompt_log();
        if self.plog.is_empty() {
            self.status = "no prompts logged yet".into();
            return;
        }
        self.view = View::PromptLog;
    }

    pub(crate) fn load_session(&mut self, sid: &str) {
        self.store = GoalStore::resolve(sid);
        let doc = self.store.load();
        self.state = Some(doc.main);
        self.tree = Tree {
            current: doc.current,
            subs: doc.subs,
            next_id: doc.next_id,
        };
        self.proposals = doc.proposals;
        self.dismissed = doc.dismissed;
        self.paraphrase = doc.paraphrase;
        self.last_turn = doc.last_turn;
        self.constraints = doc.constraints;
        self.project_key = doc.project_key;
        self.observe_pending = None;
        self.observe_at = None;
        self.goal_mtime = self.store.mtime();
        self.editing = false;
        self.edit_buf.clear();
        self.view = View::Tree;
        self.sel_node = 0;
        // Fill est/deadline from the project's time-gate plan on open, so a freshly-opened goal shows
        // its estimate without needing a mutation first. Persist only if something actually changed.
        if self.refresh_estimates() {
            self.persist();
        }
        self.refresh_plans();
        // Auto-infer once per session when empty (main single-line + structured tree).
        if self.state.as_ref().is_none_or(|s| !has_real(&s.text))
            && self.inferred.insert(sid.to_string())
        {
            self.push_headless(HeadlessMode::Infer, sid.to_string());
        }
        if !has_real(&self.tree.current.text)
            && self.tree.subs.is_empty()
            && self.inferred_tree.insert(sid.to_string())
        {
            self.push_headless(HeadlessMode::InferTree, sid.to_string());
        }
        self.status.clear();
    }

    /// Snapshot of in-memory state as the consolidated doc.
    pub(crate) fn doc(&self) -> GoalDoc {
        GoalDoc {
            goal_id: self.store.id.clone(),
            schema: 1,
            main: self.state.clone().unwrap_or_default(),
            current: self.tree.current.clone(),
            subs: self.tree.subs.clone(),
            next_id: self.tree.next_id,
            proposals: self.proposals.clone(),
            dismissed: self.dismissed.clone(),
            paraphrase: self.paraphrase.clone(),
            last_turn: self.last_turn,
            constraints: self.constraints.clone(),
            project_key: self.project_key.clone(),
        }
    }
    /// Fill main+current est/deadline from the project's time-gate plan (no-op when pinned / no plan /
    /// no set_at). Returns true if anything changed, so callers can persist conditionally.
    pub(crate) fn refresh_estimates(&mut self) -> bool {
        let root = self.store.root();
        let snap = |s: &GoalState| (s.est_minutes, s.deadline.clone());
        let before = (self.state.as_ref().map(snap), snap(&self.tree.current));
        if let Some(m) = self.state.as_mut() {
            fill_estimate(m, &root);
        }
        fill_estimate(&mut self.tree.current, &root);
        let after = (self.state.as_ref().map(snap), snap(&self.tree.current));
        before != after
    }

    /// Rebuild the project-scoped plans list from `~/.claude/plans` (read-only). Uses the same root as
    /// `refresh_estimates` (the goal file's folder). A global goal under the state dir has no project, so
    /// it gets no plans. Drops cached bodies whose plan vanished. Called on load and on every reload.
    pub(crate) fn refresh_plans(&mut self) {
        let root = self.store.root();
        // Explicit project_key scopes plans even for a global/worktree goal; only a global goal with no
        // key has no project (see plans_scope_key). This is what makes worktree sessions show plans.
        let key = plans_scope_key(&root, &self.project_key);
        self.plans_label = key.clone().unwrap_or_default();
        self.plans = match &key {
            Some(k) => list_project_plans(k),
            None => Vec::new(),
        };
        let live: std::collections::HashSet<&String> =
            self.plans.iter().map(|p| &p.basename).collect();
        self.plans_expanded.retain(|b| live.contains(b));
        // The newest plan is the "current" one and auto-expands (unless the user folded it). Re-read the
        // body of EVERY open plan (current auto-expand + user-expanded) straight from disk on each refresh
        // so an in-place edit (same file, new content / new title) shows fresh — never a stale cache.
        let current = self.plans.first().map(|p| p.basename.clone());
        let mut keep: std::collections::HashSet<String> =
            self.plans_expanded.iter().cloned().collect();
        if let Some(cur) = &current {
            if !self.plans_current_folded {
                keep.insert(cur.clone());
            }
        }
        let mut bodies: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for b in keep.iter() {
            if let Some(p) = self.plans.iter().find(|p| &p.basename == b) {
                if let Ok(body) = std::fs::read_to_string(&p.path) {
                    bodies.insert(b.clone(), body);
                }
            }
        }
        self.plans_body = bodies;
        self.plans_mtime = plans_watermark(); // watermark for the cheap poll re-glob
    }

    /// Cheap per-tick check: when `~/.claude/plans` changes (a plan approved/created/UPDATED/removed),
    /// re-glob so the change appears within a poll tick — no `r`/re-infer needed. The watermark folds in
    /// the newest plan file's mtime, so an in-place plan EDIT (re-approval, new estimate) triggers a
    /// refresh too, not only added/removed files. Also re-pulls the time-gate estimate (`est · due ·
    /// time-left`) since an updated plan can carry a new `p50`, and persists it if it actually changed.
    pub(crate) fn refresh_plans_if_changed(&mut self) {
        if self.current_sid.is_none() {
            return;
        }
        if plans_watermark() != self.plans_mtime {
            self.refresh_plans();
            if self.refresh_estimates() {
                self.persist();
            }
        }
    }

    /// Fold/unfold one plan's body (read lazily on expand, dropped on collapse). Used by ⏎ and clicks.
    pub(crate) fn toggle_plan(&mut self, basename: &str) {
        if self.plans_expanded.remove(basename) {
            self.plans_body.remove(basename);
        } else {
            self.plans_expanded.insert(basename.to_string());
            if let Some(p) = self.plans.iter().find(|p| p.basename == basename) {
                if let Ok(body) = std::fs::read_to_string(&p.path) {
                    self.plans_body.insert(basename.to_string(), body);
                }
            }
        }
    }

    /// True when `basename` is the newest "current" plan (auto-expanded). Clicks on it fold differently.
    pub(crate) fn is_current_plan(&self, basename: &str) -> bool {
        self.plans.first().is_some_and(|p| p.basename == basename)
    }

    /// Fold/unfold the current (newest) plan — separate from `plans_expanded` so re-seeding on refresh
    /// doesn't undo a user fold. Loads/drops its body to match.
    pub(crate) fn toggle_current_plan_fold(&mut self) {
        self.plans_current_folded = !self.plans_current_folded;
        if let Some(cur) = self.plans.first().map(|p| p.basename.clone()) {
            if self.plans_current_folded {
                self.plans_body.remove(&cur);
            } else if let Some(p) = self.plans.iter().find(|p| p.basename == cur) {
                if let Ok(body) = std::fs::read_to_string(&p.path) {
                    self.plans_body.insert(cur, body);
                }
            }
        }
    }

    /// Persist the whole doc and refresh the mtime watermark.
    pub(crate) fn persist(&mut self) {
        // Keep est/deadline current from the project's time-gate plan (no-op when pinned / no plan / no set_at).
        let root = self.store.root();
        if let Some(m) = self.state.as_mut() {
            fill_estimate(m, &root);
        }
        fill_estimate(&mut self.tree.current, &root);
        let _ = self.store.save(&self.doc());
        self.goal_mtime = self.store.mtime();
    }
    /// Set the MAIN goal, persist, log to its history.
    pub(crate) fn set_main(&mut self, text: &str, source: &str, approved: bool) {
        let st = GoalState {
            text: text.to_string(),
            source: source.to_string(),
            approved,
            state: String::new(),
            updated_at: now_stamp(),
            set_at: now_stamp(),
            reason: None,
            ..Default::default()
        };
        self.state = Some(st.clone());
        self.persist();
        self.store.append_history("main", &st);
    }
    /// Set the CURRENT goal, persist, log to its history.
    pub(crate) fn set_current_goal(&mut self, text: &str, source: &str, approved: bool) {
        self.tree.current = GoalState {
            text: text.to_string(),
            source: source.to_string(),
            approved,
            state: String::new(),
            updated_at: now_stamp(),
            set_at: now_stamp(),
            reason: None,
            ..Default::default()
        };
        let snap = self.tree.current.clone();
        self.persist();
        self.store.append_history("current", &snap);
    }
    /// Add a sub-goal (returns its id), persist, log to the new sub's history.
    pub(crate) fn add_sub_goal(
        &mut self,
        text: &str,
        source: &str,
        approved: bool,
        parent: &str,
    ) -> u64 {
        let id = self.tree.next_id;
        self.tree.next_id += 1;
        let sub = Sub {
            id,
            text: text.to_string(),
            source: source.to_string(),
            approved,
            state: "active".into(),
            parent: parent.to_string(),
            created_at: now_stamp(),
            updated_at: now_stamp(),
            pinned: source == "user",
        };
        let snap = sub.snapshot();
        self.tree.subs.push(sub);
        self.persist();
        self.store.append_history(&format!("sub-{id}"), &snap);
        id
    }
    /// Mutate one sub by id, persist, log its new snapshot.
    pub(crate) fn update_sub_goal<F: FnOnce(&mut Sub)>(&mut self, id: u64, f: F) {
        if let Some(s) = self.tree.subs.iter_mut().find(|s| s.id == id) {
            f(s);
            s.updated_at = now_stamp();
            let snap = s.snapshot();
            self.persist();
            self.store.append_history(&format!("sub-{id}"), &snap);
        }
    }

    /// Save this session's goal file + its history into the session's working folder
    /// (`<cwd>/.goal.json` + `<cwd>/.goal.history/`), so the goals travel with the project and other
    /// sessions in that folder adopt them. No-op if the session's cwd can't be resolved.
    pub(crate) fn save_to_project(&mut self) {
        let Some(sid) = self.current_sid.clone() else {
            self.status = "no focused session".into();
            return;
        };
        let Some(cwd) = session_cwd(&sid) else {
            self.status = "can't resolve working folder".into();
            return;
        };
        self.save_to_folder(&cwd);
    }

    /// Move this session's goal docs into `dest_root` and repoint the live store (shared by `w`/save and
    /// the in-TUI result of a sort). No-op when already there. Uses the same move core as `--sort`.
    pub(crate) fn save_to_folder(&mut self, dest_root: &Path) {
        let dest = project_goal_file(dest_root);
        if dest == self.store.path {
            self.status = "already saved in project".into();
            return;
        }
        let doc = self.doc();
        match move_goal_store(&self.store, &doc, dest_root) {
            Ok(new) => {
                self.store = new;
                self.goal_mtime = self.store.mtime();
                self.refresh_plans();
                self.status = format!("saved to {}", dest.display());
            }
            Err(e) => self.status = format!("save failed: {e}"),
        }
    }

    /// Update the focused session from herdr's [`FocusInput`] (replaces the cmux
    /// `resolve_focus()` discovery). `Some(session_id)` ⇒ load it; `is_self_focus`
    /// ⇒ sticky (keep showing, seeding from the most-recent session on a fresh
    /// open); otherwise no agent is focused ⇒ clear.
    pub fn refresh(&mut self, focus: FocusInput) {
        self.focused = focus.clone();
        if let Some(sid) = self.pinned_sid.clone() {
            if self.current_sid.as_deref() != Some(&sid) {
                self.current_sid = Some(sid.clone());
                self.load_session(&sid);
            }
            return;
        }
        match focus.session_id {
            Some(sid) => {
                if self.current_sid.as_deref() != Some(&sid) {
                    self.current_sid = Some(sid.clone());
                    self.load_session(&sid);
                }
            }
            None if focus.is_self_focus => {
                // Sticky: the dashboard's own region is focused — never wipe what we
                // show. On a fresh open (nothing tracked) seed from the most-recently
                // active session so a real goal shows immediately.
                if self.current_sid.is_none() {
                    if let Some(sid) = most_recent_session()
                        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
                    {
                        self.current_sid = Some(sid.clone());
                        self.load_session(&sid);
                    }
                }
            }
            None => {
                if self.current_sid.is_some() {
                    self.current_sid = None;
                    self.state = None;
                    self.tree = Tree::default();
                    self.editing = false;
                    self.status = "no agent session on the focused surface".into();
                }
            }
        }
    }

    fn push_headless(&mut self, mode: HeadlessMode, sid: String) {
        self.effects.push(Effect::SpawnHeadless {
            agent: self.focused.agent,
            mode,
            sid,
            session_ref: self.focused.session_ref.clone(),
            cwd: self.focused.cwd.clone(),
        });
    }

    /// Drain queued host effects (backend spawns, pane key sends) after a `handle`.
    pub(crate) fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    pub(crate) fn refresh_if_changed(&mut self) {
        if self.current_sid.is_none() {
            return;
        }
        let mt = self.store.mtime();
        if mt != self.goal_mtime {
            self.goal_mtime = mt;
            let doc = self.store.load();
            self.state = Some(doc.main);
            self.tree = Tree {
                current: doc.current,
                subs: doc.subs,
                next_id: doc.next_id,
            };
            self.proposals = doc.proposals;
            self.dismissed = doc.dismissed;
            self.paraphrase = doc.paraphrase;
            self.last_turn = doc.last_turn;
            self.constraints = doc.constraints;
            self.project_key = doc.project_key;
            self.refresh_plans();
        }
    }

    /// Watcher: on a new user prompt, dispatch a detached Goal Observer run — one at a time per
    /// session, never blocking the UI. Recovers if a dispatched run died (150s timeout).
    pub(crate) fn maybe_observe(&mut self) {
        let Some(sid) = self.current_sid.clone() else {
            return;
        };
        let Some(path) = session_jsonl(&sid) else {
            return;
        };
        // First sight of a session: baseline the observe-cursor to "now" and DON'T spawn. Opening the
        // widget starts curation from the next prompt onward — never a retroactive Opus run over the
        // whole pre-existing transcript (which would be a surprise cost and a thundering herd at start).
        if self.observed_baseline.insert(sid.clone()) {
            let total = transcript_line_count(&path) as u64;
            if total > self.last_turn {
                self.last_turn = total;
                let mut doc = self.store.load();
                doc.last_turn = total;
                let _ = self.store.save(&doc);
            }
            return;
        }
        if let Some(pending) = self.observe_pending {
            let timed_out = self
                .observe_at
                .is_none_or(|t| t.elapsed() > Duration::from_secs(150));
            if self.last_turn >= pending || timed_out {
                self.observe_pending = None;
                self.observe_at = None;
            }
        }
        if self.observe_pending.is_some() {
            return;
        } // one observer at a time
        if !has_new_user_prompt(&path, self.last_turn as usize) {
            return;
        }
        let total = transcript_line_count(&path) as u64;
        self.push_headless(HeadlessMode::Observe, sid.clone());
        self.observe_pending = Some(total);
        self.observe_at = Some(Instant::now());
    }

    // ---- tree structure ----
    pub(crate) fn subs_in(&self, par: Par, st: St) -> Vec<u64> {
        self.tree
            .subs
            .iter()
            .filter(|s| par_of(&s.parent) == par && st_of(&s.state) == st)
            .map(|s| s.id)
            .collect()
    }
    pub(crate) fn is_collapsed(&self, par: Par, st: St) -> bool {
        default_collapsed(st) ^ self.toggled.contains(&(par, st))
    }
    /// Rebuild the flattened visible-node list (Main, its sections; Current, its sections; Independent).
    pub(crate) fn rebuild_nodes(&mut self) {
        let mut nodes = vec![Node::Main];
        for st in ST_ORDER {
            if !self.subs_in(Par::Main, st).is_empty() {
                nodes.push(Node::Section(Par::Main, st));
                if !self.is_collapsed(Par::Main, st) {
                    for id in self.subs_in(Par::Main, st) {
                        nodes.push(Node::Sub(id));
                    }
                }
            }
        }
        nodes.push(Node::Current);
        for st in ST_ORDER {
            if !self.subs_in(Par::Current, st).is_empty() {
                nodes.push(Node::Section(Par::Current, st));
                if !self.is_collapsed(Par::Current, st) {
                    for id in self.subs_in(Par::Current, st) {
                        nodes.push(Node::Sub(id));
                    }
                }
            }
        }
        for st in ST_ORDER {
            if !self.subs_in(Par::Indep, st).is_empty() {
                nodes.push(Node::Section(Par::Indep, st));
                if !self.is_collapsed(Par::Indep, st) {
                    for id in self.subs_in(Par::Indep, st) {
                        nodes.push(Node::Sub(id));
                    }
                }
            }
        }
        self.nodes = nodes;
        if self.sel_node >= self.nodes.len() {
            self.sel_node = self.nodes.len().saturating_sub(1);
        }
    }
    pub(crate) fn selected(&self) -> Option<Node> {
        self.nodes.get(self.sel_node).copied()
    }
    pub(crate) fn sub_ref(&self, id: u64) -> Option<&Sub> {
        self.tree.subs.iter().find(|s| s.id == id)
    }

    // ---- actions on the selected node ----
    pub(crate) fn applicable(&self, node: Node, act: Act) -> bool {
        if matches!(act, Act::ClearRebrief | Act::Sort | Act::AddSub) {
            return true;
        } // session-global, not node-scoped
        match node {
            Node::Section(..) => false,
            Node::Main | Node::Current => !matches!(act, Act::State | Act::Discard),
            Node::Sub(_) => true,
        }
    }

    /// A node is "agent-inferred & unapproved" when its goal text came from the agent and the user
    /// hasn't accepted it yet — the only case where an Approve chip should appear.
    pub(crate) fn node_is_unapproved_agent(&self, node: Node) -> bool {
        match node {
            Node::Main => self
                .state
                .as_ref()
                .is_some_and(|s| s.source == "agent" && !s.approved && has_real(&s.text)),
            Node::Current => {
                let c = &self.tree.current;
                c.source == "agent" && !c.approved && has_real(&c.text)
            }
            Node::Sub(id) => self
                .sub_ref(id)
                .is_some_and(|s| s.source == "agent" && !s.approved && has_real(&s.text)),
            Node::Section(..) => false,
        }
    }

    /// The contextual chips for a node, in display order. Approve leads only when unapproved-agent;
    /// Main/Current get edit·history·re-suggest, a Sub gets edit·state·remove. Sections have no chips.
    pub(crate) fn node_actions(&self, node: Node) -> Vec<Act> {
        match node {
            Node::Section(..) => Vec::new(),
            Node::Main | Node::Current => {
                let mut v = Vec::new();
                if self.node_is_unapproved_agent(node) {
                    v.push(Act::Approve);
                }
                v.extend([Act::Edit, Act::Log, Act::Copy, Act::Infer]);
                v
            }
            Node::Sub(_) => {
                let mut v = Vec::new();
                if self.node_is_unapproved_agent(node) {
                    v.push(Act::Approve);
                }
                v.extend([Act::Edit, Act::Copy, Act::State, Act::Discard]);
                v
            }
        }
    }

    /// Icon glyph + plain-word hover tooltip for a chip. The State chip is dynamic: its glyph is the
    /// state pressing it MOVES TO (the next state in the cycle), so the user sees what will happen.
    pub(crate) fn chip_glyph_tip(&self, node: Node, act: Act) -> (String, String) {
        match act {
            Act::Approve => ("✓".into(), "approve — accept this as yours".into()),
            Act::Edit => ("✎".into(), "edit — rewrite this goal".into()),
            Act::Log => ("☰".into(), "history — see past versions".into()),
            Act::Copy => ("⧉".into(), "copy — copy this goal text".into()),
            Act::Infer => ("↻".into(), "re-suggest — let the agent redraft it".into()),
            Act::Discard => ("🗑".into(), "remove — discard this subgoal".into()),
            Act::State => {
                let cur = if let Node::Sub(id) = node {
                    self.sub_ref(id)
                        .map(|s| st_of(&s.state))
                        .unwrap_or(St::Active)
                } else {
                    St::Active
                };
                let next = ST_ORDER
                    [(ST_ORDER.iter().position(|x| *x == cur).unwrap_or(0) + 1) % ST_ORDER.len()];
                let g = match next {
                    St::Active => "▶",
                    St::Paused => "⏸",
                    St::Done => "✓",
                    St::Discarded => "🗑",
                };
                (g.into(), format!("set state → {}", st_label(next)))
            }
            Act::AddSub => ("＋".into(), "add a subgoal".into()),
            Act::ClearRebrief => ("⟲".into(), "clear & re-brief the session".into()),
            Act::Sort => ("⌖".into(), "auto-sort the goal tree".into()),
        }
    }

    /// How many chips the selected node has (for keyboard cycling / clamping `sel_ctrl`).
    pub(crate) fn ctrl_count(&self) -> usize {
        self.selected()
            .map(|n| self.node_actions(n).len())
            .unwrap_or(0)
    }

    /// Full chip specs (action + glyph + tooltip) for the selected-node chip row.
    pub(crate) fn chip_specs(&self, node: Node) -> Vec<ChipSpec> {
        self.node_actions(node)
            .into_iter()
            .map(|act| {
                let (glyph, tip) = self.chip_glyph_tip(node, act);
                ChipSpec { act, glyph, tip }
            })
            .collect()
    }

    pub(crate) fn activate_selected(&mut self) {
        // Enter/Space: a section toggles; otherwise run the selected chip on the node.
        match self.selected() {
            Some(Node::Section(par, st)) => self.toggle_section(par, st),
            Some(node) => {
                let acts = self.node_actions(node);
                if let Some(act) = acts.get(self.sel_ctrl).copied() {
                    self.run_act(node, act);
                }
            }
            None => {}
        }
    }

    pub(crate) fn run_act(&mut self, node: Node, act: Act) {
        if self.current_sid.is_none() {
            self.status = "no focused session".into();
            return;
        }
        if !self.applicable(node, act) {
            self.status = "not available here".into();
            return;
        }
        match act {
            Act::Approve => self.approve_node(node),
            Act::Edit => self.begin_edit(node),
            Act::Infer => self.infer_node(node),
            Act::Log => self.open_log(node),
            Act::Copy => self.copy_node(node),
            Act::AddSub => self.begin_add_sub(),
            Act::State => self.cycle_state(node),
            Act::Discard => self.discard(node),
            Act::ClearRebrief => self.clear_rebrief(),
            Act::Sort => self.fetch_sort(),
        }
    }

    /// TRUE `/clear` the attached Claude conversation, then paste a carry-forward briefing built from
    /// the goals + recent prompts the widget already holds. The briefing is composed BEFORE any clear
    /// is sent (the clear wipes the conversation). NEVER `/compact`.
    pub(crate) fn clear_rebrief(&mut self) {
        let Some(sid) = self.current_sid.clone() else {
            self.status = "no focused session".into();
            return;
        };
        // 1. Compose FIRST — read in-memory goals + the pre-clear transcript.
        let transcript = session_jsonl(&sid).unwrap_or_else(|| PathBuf::from(&sid));
        let cwd = session_cwd(&sid).unwrap_or_else(|| PathBuf::from("."));
        let prompts = user_prompts(&transcript, 5);
        let main = self
            .state
            .as_ref()
            .map(|s| s.text.as_str())
            .filter(|t| has_real(t));
        let cur = self.tree.current.text.as_str();
        let current = if has_real(cur) { cur } else { "" };
        let brief = compose_rebrief(main, current, &prompts, &transcript, &cwd);
        // 2. Stage the brief in a FILE — it CANNOT be pasted inline. `cmux send` AND `cmux paste-buffer`
        //    both convert every newline to Enter (no bracketed paste; proven against a real surface:
        //    a multi-line brief comes out `<rebrief>\rLINE\r\r…` and each \r submits, so the Overall/
        //    Current/Recent-prompts sections were lost). A single-line "read this file" prompt has no
        //    newline to shred; Claude reads the staged brief in full after the clear.
        let brief_path = state_dir().join(format!("{sid}.rebrief.md"));
        if let Err(e) = fs::create_dir_all(state_dir()).and_then(|_| fs::write(&brief_path, &brief))
        {
            self.status = format!("couldn't stage rebrief: {e}");
            return;
        }
        // 3. Emit the clear-then-brief sequence for the host to send to the focused
        //    pane (a TRUE /clear, then a one-line "read this file" prompt — never
        //    chained, so the multi-line brief is never shredded by newline→Enter).
        self.effects.push(Effect::SendPaneKeys {
            lines: vec![
                "/clear".to_string(),
                rebrief_read_prompt(&brief_path.to_string_lossy()),
            ],
        });
        self.status = "cleared & re-briefed".into();
    }

    /// Kick the AI SORTER: spawn `goal-widget --sort <sid>` detached. It infers which existing project
    /// folder this session's goal docs belong to, sets `project_key`, and MOVES the docs there. The poll
    /// loop (`maybe_finish_sort`) re-resolves the moved store once the link repoints. Never blocks the UI.
    pub(crate) fn fetch_sort(&mut self) {
        let Some(sid) = self.current_sid.clone() else {
            self.status = "no focused session".into();
            return;
        };
        if self.sort_pending.is_some() {
            self.status = "already sorting…".into();
            return;
        }
        self.push_headless(HeadlessMode::Sort, sid.clone());
        self.sort_pending = Some(Instant::now());
        self.status = "sorting…".into();
    }

    /// Poll companion to `fetch_sort`: while a sort is in flight, re-resolve the goal store via the global
    /// link (the subcommand repoints it after moving). When the resolved path changes, adopt the moved
    /// location (`load_session` re-reads + `refresh_plans`); give up after ~90s (refusal / no-op run).
    pub(crate) fn maybe_finish_sort(&mut self) {
        let Some(started) = self.sort_pending else {
            return;
        };
        let Some(sid) = self.current_sid.clone() else {
            self.sort_pending = None;
            return;
        };
        if GoalStore::resolve(&sid).path != self.store.path {
            self.load_session(&sid);
            self.sort_pending = None;
            self.status = format!("sorted → {}", self.store.root().display());
        } else if started.elapsed() > Duration::from_secs(90) {
            self.sort_pending = None; // agent refused / picked nothing — leave the goal untouched
            self.status = "sort: no change".into();
        }
    }

    pub(crate) fn toggle_section(&mut self, par: Par, st: St) {
        if self.toggled.contains(&(par, st)) {
            self.toggled.remove(&(par, st));
        } else {
            self.toggled.insert((par, st));
        }
    }

    pub(crate) fn approve_node(&mut self, node: Node) {
        if self.current_sid.is_none() {
            return;
        }
        match node {
            Node::Main => {
                if self.state.as_ref().is_some_and(|s| has_real(&s.text)) {
                    let t = self.state.as_ref().unwrap().text.clone();
                    self.set_main(&t, "user", true);
                    self.status = "approved".into();
                } else {
                    self.status = "nothing to approve — e to set".into();
                }
            }
            Node::Current => {
                if has_real(&self.tree.current.text) {
                    let t = self.tree.current.text.clone();
                    self.set_current_goal(&t, "user", true);
                    self.status = "approved".into();
                } else {
                    self.status = "nothing to approve — e to set".into();
                }
            }
            Node::Sub(id) => {
                let real = self.sub_ref(id).is_some_and(|s| has_real(&s.text));
                if real {
                    self.update_sub_goal(id, |s| {
                        s.source = "user".into();
                        s.approved = true;
                    });
                    self.status = "approved".into();
                } else {
                    self.status = "empty sub".into();
                }
            }
            Node::Section(..) => {}
        }
    }

    pub(crate) fn begin_edit(&mut self, node: Node) {
        let prefill = match node {
            Node::Main => self
                .state
                .as_ref()
                .map(|s| s.text.clone())
                .unwrap_or_default(),
            Node::Current => self.tree.current.text.clone(),
            Node::Sub(id) => self.sub_ref(id).map(|s| s.text.clone()).unwrap_or_default(),
            Node::Section(..) => return,
        };
        self.edit_target = match node {
            Node::Main => EditTarget::Main,
            Node::Current => EditTarget::Current,
            Node::Sub(id) => EditTarget::Sub(id),
            Node::Section(..) => return,
        };
        self.editing = true;
        self.edit_buf = if prefill.starts_with('(') {
            String::new()
        } else {
            prefill
        };
        self.status = "type, Enter to set, Esc to cancel".into();
    }

    pub(crate) fn begin_add_sub(&mut self) {
        if self.current_sid.is_none() {
            self.status = "no focused session".into();
            return;
        }
        self.edit_target = EditTarget::NewSub;
        self.editing = true;
        self.edit_buf.clear();
        self.status = "new sub-goal (under current), Enter to add".into();
    }

    pub(crate) fn commit_edit(&mut self) {
        if self.current_sid.is_none() {
            self.editing = false;
            return;
        }
        let new = self.edit_buf.trim().to_string();
        let target = self.edit_target.clone();
        self.editing = false;
        self.edit_buf.clear();
        if new.is_empty() {
            self.status = "cancelled".into();
            return;
        }
        match target {
            EditTarget::Main => self.set_main(&new, "user", true),
            EditTarget::Current => self.set_current_goal(&new, "user", true),
            EditTarget::Sub(id) => self.update_sub_goal(id, |s| {
                s.text = new.clone();
                s.source = "user".into();
                s.approved = true;
            }),
            EditTarget::NewSub => {
                self.add_sub_goal(&new, "user", true, "current");
                // reveal the section the new sub lands in (current · active) so it isn't hidden behind
                // the compressed default. `is_collapsed = default(true) ^ toggled`, so inserting flips it open.
                self.toggled.insert((Par::Current, St::Active));
            }
            EditTarget::TimeMain => self.set_time("main", &new),
            EditTarget::TimeCurrent => self.set_time("current", &new),
            EditTarget::ConstraintMain => self.add_constraint("main", &new),
            EditTarget::ConstraintCurrent => self.add_constraint("current", &new),
        }
        self.status = "saved".into();
    }

    pub(crate) fn copy_node(&mut self, node: Node) {
        let text = match node {
            Node::Main => self.state.as_ref().map(|s| s.text.clone()).unwrap_or_default(),
            Node::Current => self.tree.current.text.clone(),
            Node::Sub(id) => self.sub_ref(id).map(|s| s.text.clone()).unwrap_or_default(),
            Node::Section(..) => String::new(),
        };
        if !has_real(&text) {
            self.status = "nothing to copy".into();
            return;
        }
        self.effects.push(Effect::CopyText(text));
        self.status = "copied".into();
    }

    pub(crate) fn infer_node(&mut self, node: Node) {
        let Some(sid) = self.current_sid.clone() else {
            return;
        };
        match node {
            Node::Main => {
                self.inferred.remove(&sid);
                self.push_headless(HeadlessMode::Infer, sid.clone());
                self.status = "inferring main…".into();
            }
            _ => {
                self.inferred_tree.remove(&sid);
                self.push_headless(HeadlessMode::InferTree, sid.clone());
                self.status = "inferring current + subs…".into();
            }
        }
        // Re-glob the project's plans so a plan written since load shows immediately (infer also refreshes plans).
        self.refresh_plans();
    }

    pub(crate) fn cycle_state(&mut self, node: Node) {
        if self.current_sid.is_none() {
            return;
        }
        if let Node::Sub(id) = node {
            let cur = self
                .sub_ref(id)
                .map(|s| st_of(&s.state))
                .unwrap_or(St::Active);
            let next = ST_ORDER
                [(ST_ORDER.iter().position(|x| *x == cur).unwrap_or(0) + 1) % ST_ORDER.len()];
            self.update_sub_goal(id, |s| s.state = st_str(next).into());
            self.status = format!("→ {}", st_label(next));
        }
    }

    pub(crate) fn discard(&mut self, node: Node) {
        if self.current_sid.is_none() {
            return;
        }
        if let Node::Sub(id) = node {
            self.update_sub_goal(id, |s| s.state = "discarded".into());
            self.status = "discarded".into();
        }
    }

    pub(crate) fn cycle_parent(&mut self) {
        if self.current_sid.is_none() {
            return;
        }
        if let Some(Node::Sub(id)) = self.selected() {
            let cur = self
                .sub_ref(id)
                .map(|s| s.parent.clone())
                .unwrap_or_else(|| "current".into());
            let i = SUB_PARENTS.iter().position(|x| *x == cur).unwrap_or(0);
            let next = SUB_PARENTS[(i + 1) % SUB_PARENTS.len()];
            self.update_sub_goal(id, |s| s.parent = next.into());
            self.status = format!("parent → {next}");
        } else {
            self.status = "select a sub-goal".into();
        }
    }

    // ---- history (☰ Log) per entity ----
    pub(crate) fn open_log(&mut self, node: Node) {
        if self.current_sid.is_none() {
            return;
        }
        let (entity, target): (String, HistTarget) = match node {
            Node::Main => ("main".into(), HistTarget::Main),
            Node::Current => ("current".into(), HistTarget::Current),
            Node::Sub(id) => (format!("sub-{id}"), HistTarget::Sub(id)),
            Node::Section(..) => return,
        };
        self.history = self.store.read_history(&entity);
        if self.history.is_empty() {
            self.status = "no changes recorded yet".into();
            return;
        }
        self.sel_hist = self.history.len() - 1;
        self.hist_target = target;
        self.view = View::History;
        self.status.clear();
    }

    pub(crate) fn close_history(&mut self) {
        self.view = View::Tree;
        self.status.clear();
    }

    pub(crate) fn restore(&mut self, idx: usize) {
        if self.current_sid.is_none() {
            return;
        }
        let Some(entry) = self.history.get(idx).cloned() else {
            return;
        };
        match self.hist_target.clone() {
            HistTarget::Main => self.set_main(&entry.text, &entry.source, entry.approved),
            HistTarget::Current => {
                self.set_current_goal(&entry.text, &entry.source, entry.approved)
            }
            HistTarget::Sub(id) => self.update_sub_goal(id, |s| {
                s.text = entry.text.clone();
                s.source = entry.source.clone();
                s.approved = entry.approved;
            }),
        }
        self.status = if entry.approved {
            "restored (approved)".into()
        } else {
            "restored".into()
        };
        self.view = View::Tree;
    }
}

// `dim_rule` and `short_time` render helpers live in `render.rs`.

impl App {
    /// True when the panel is at its top-level tree (not in a sub-view or an
    /// edit). The host uses this to decide whether Esc backs out of a sub-view
    /// or releases keyboard focus back to the pane.
    pub fn is_tree_root(&self) -> bool {
        self.view == View::Tree && !self.editing
    }

    /// Route an abstract UI intent to the active view's handler, then drain the
    /// queued host effects. Mirrors goal-widget's `event_loop` key routing so the
    /// host only forwards intents and never reimplements per-view behavior.
    pub fn handle(&mut self, action: GoalAction) -> Vec<Effect> {
        use GoalAction as A;
        match action {
            A::Hover(c, r) => {
                self.hover = Some((c, r));
                return self.take_effects();
            }
            A::Click(c, r) => {
                self.handle_click(c, r);
                return self.take_effects();
            }
            _ => {}
        }
        match self.view {
            View::History => match action {
                A::Back => self.close_history(),
                A::Up => {
                    if self.sel_hist + 1 < self.history.len() {
                        self.sel_hist += 1; // toward newest (top)
                    }
                }
                A::Down => self.sel_hist = self.sel_hist.saturating_sub(1),
                A::Enter | A::Space => self.restore(self.sel_hist),
                _ => {}
            },
            View::Proposals => match action {
                A::Back => {
                    self.view = View::Tree;
                    self.status.clear();
                }
                A::Up => self.sel_prop = self.sel_prop.saturating_sub(1),
                A::Down => {
                    self.sel_prop = (self.sel_prop + 1).min(self.proposals.len().saturating_sub(1))
                }
                A::Enter | A::Space => self.toggle_proposal(),
                A::Char('y') => self.accept_proposal(self.sel_prop),
                A::Char('x') => self.dismiss_proposal(self.sel_prop),
                _ => {}
            },
            View::PromptLog => match action {
                A::Back | A::Char('m') => {
                    self.view = View::Tree;
                    self.status.clear();
                }
                _ => {}
            },
            View::Tree if self.editing => match action {
                A::Back => {
                    self.editing = false;
                    self.edit_buf.clear();
                    self.status = "cancelled".into();
                }
                A::Enter => self.commit_edit(),
                A::Backspace => {
                    self.edit_buf.pop();
                }
                A::Space => self.edit_buf.push(' '),
                A::Char(c) => self.edit_buf.push(c),
                _ => {}
            },
            View::Tree => match action {
                A::Up => {
                    if !self.move_cons_sel(-1) {
                        self.sel_node = self.sel_node.saturating_sub(1);
                        self.sel_ctrl = 0;
                        self.sync_cons_sel();
                    }
                }
                A::Down => {
                    if !self.move_cons_sel(1) {
                        self.sel_node = (self.sel_node + 1).min(self.nodes.len().saturating_sub(1));
                        self.sel_ctrl = 0;
                        self.sync_cons_sel();
                    }
                }
                A::Left => {
                    let n = self.ctrl_count();
                    if n > 0 {
                        self.sel_ctrl = (self.sel_ctrl + n - 1) % n;
                    }
                }
                A::Right => {
                    let n = self.ctrl_count();
                    if n > 0 {
                        self.sel_ctrl = (self.sel_ctrl + 1) % n;
                    }
                }
                A::Enter | A::Space => self.activate_selected(),
                A::Char('a') => {
                    if let Some(n) = self.selected() {
                        self.run_act(n, Act::Approve)
                    }
                }
                A::Char('e') => {
                    if let Some(n) = self.selected() {
                        self.run_act(n, Act::Edit)
                    }
                }
                A::Char('r') => {
                    if let Some(n) = self.selected() {
                        self.run_act(n, Act::Infer)
                    }
                }
                A::Char('l') => {
                    if let Some(n) = self.selected() {
                        self.run_act(n, Act::Log)
                    }
                }
                A::Char('n') => self.begin_add_sub(),
                A::Char('s') => {
                    if let Some(n) = self.selected() {
                        self.run_act(n, Act::State)
                    }
                }
                A::Char('d') => {
                    if !self.delete_selected_constraint() {
                        if let Some(n) = self.selected() {
                            self.run_act(n, Act::Discard)
                        }
                    }
                }
                A::Char('p') => self.cycle_parent(),
                A::Char('w') => self.save_to_project(),
                A::Char('x') => self.clear_rebrief(),
                A::Char('f') => self.fetch_sort(),
                A::Char('v') => self.open_proposals(),
                A::Char('m') => self.open_prompt_log(),
                A::Char('t') => self.begin_time_edit(),
                A::Char('c') => self.toggle_constraints(),
                A::Char('C') => self.begin_constraint_edit(),
                A::Char('P') => self.plans_section_open = !self.plans_section_open,
                _ => {}
            },
        }
        self.take_effects()
    }

    /// Resolve a left-click against the last frame's hit-rects (mirrors the
    /// `event_loop` mouse branch). `handle` drains the effects afterward.
    fn handle_click(&mut self, col: u16, row: u16) {
        let hit = |r: &Rect| row == r.y && col >= r.x && col < r.x + r.width;
        match self.view {
            View::History => {
                if let Some(idx) = self
                    .hist_rects
                    .iter()
                    .find(|(r, _)| hit(r))
                    .map(|(_, i)| *i)
                {
                    self.sel_hist = idx;
                    self.restore(idx);
                }
            }
            View::Proposals => {
                if let Some(idx) = self
                    .prop_rects
                    .iter()
                    .find(|(r, _)| hit(r))
                    .map(|(_, i)| *i)
                {
                    let was = self.sel_prop;
                    self.sel_prop = idx;
                    if was == idx {
                        self.toggle_proposal();
                    }
                }
            }
            View::PromptLog => {}
            View::Tree => {
                if self.editing {
                    return;
                }
                if let Some(br) = self.banner_rect {
                    if hit(&br) {
                        self.open_proposals();
                        return;
                    }
                }
                if let Some(ent) = self
                    .cons_header_rects
                    .iter()
                    .find(|(r, _)| hit(r))
                    .map(|(_, e)| e.clone())
                {
                    if let Some(idx) = self
                        .node_rects
                        .iter()
                        .find(|(r, _)| hit(r))
                        .map(|(_, i)| *i)
                    {
                        self.sel_node = idx;
                    }
                    self.toggle_constraints_entity(&ent);
                    return;
                }
                if let Some(hr) = self.plans_head_rect {
                    if hit(&hr) {
                        self.plans_section_open = !self.plans_section_open;
                        return;
                    }
                }
                if let Some(b) = self
                    .plans_row_rects
                    .iter()
                    .find(|(r, _)| hit(r))
                    .map(|(_, b)| b.clone())
                {
                    if self.is_current_plan(&b) {
                        self.toggle_current_plan_fold();
                    } else {
                        self.toggle_plan(&b);
                    }
                    return;
                }
                if let Some(act) = self
                    .chip_rects
                    .iter()
                    .find(|(r, _)| hit(r))
                    .map(|(_, a)| *a)
                {
                    if let Some(n) = self.selected() {
                        self.run_act(n, act);
                    }
                    return;
                }
                if let Some(idx) = self
                    .node_rects
                    .iter()
                    .find(|(r, _)| hit(r))
                    .map(|(_, i)| *i)
                {
                    if idx != self.sel_node {
                        self.sel_ctrl = 0; // new node → reset chip cursor
                    }
                    self.sel_node = idx;
                    if let Some(Node::Section(par, st)) = self.selected() {
                        self.toggle_section(par, st);
                    }
                } else if let Some(act) =
                    self.controls.iter().find(|(r, _)| hit(r)).map(|(_, c)| *c)
                {
                    let n = self.selected().unwrap_or(Node::Main);
                    self.run_act(n, act);
                }
            }
        }
    }

    /// Periodic tick the host calls (~every 250ms): re-read the goal/plan files,
    /// dispatch the observer when new prompts land, and adopt a sorted/moved
    /// store. Returns any effects (e.g. a queued observer `SpawnClaude`).
    pub fn poll(&mut self) -> Vec<Effect> {
        self.refresh_if_changed();
        self.refresh_plans_if_changed();
        self.maybe_observe();
        self.maybe_finish_sort();
        self.take_effects()
    }
}

#[cfg(test)]
mod handle_tests {
    use super::*;
    use crate::view::GoalAction as A;

    #[test]
    fn p_toggles_plans_section_with_no_effects() {
        let mut app = App::new(None);
        assert!(app.plans_section_open);
        let fx = app.handle(A::Char('P'));
        assert!(!app.plans_section_open);
        assert!(fx.is_empty());
        app.handle(A::Char('P'));
        assert!(app.plans_section_open);
    }

    #[test]
    fn editing_mode_accepts_text_space_backspace_and_cancels() {
        let mut app = App::new(None);
        app.editing = true; // simulate an in-progress edit (no fs needed)
        app.edit_buf.clear();
        app.handle(A::Char('h'));
        app.handle(A::Space);
        app.handle(A::Char('i'));
        assert_eq!(app.edit_buf, "h i");
        app.handle(A::Backspace);
        assert_eq!(app.edit_buf, "h ");
        app.handle(A::Back); // Esc cancels
        assert!(!app.editing);
        assert!(app.edit_buf.is_empty());
    }

    #[test]
    fn hover_records_position_and_emits_no_effects() {
        let mut app = App::new(None);
        let fx = app.handle(A::Hover(5, 3));
        assert_eq!(app.hover, Some((5, 3)));
        assert!(fx.is_empty());
    }

    #[test]
    fn opening_proposals_with_none_stays_on_tree() {
        let mut app = App::new(None);
        app.handle(A::Char('v'));
        assert_eq!(app.view, View::Tree);
        assert_eq!(app.status, "no proposals");
    }

    #[test]
    fn infer_uses_focused_backend_and_session_ref() {
        let mut app = App::new(None);
        app.current_sid = Some("sid-123".into());
        app.focused = FocusInput {
            session_id: Some("sid-123".into()),
            session_ref: Some("/tmp/pi-session.jsonl".into()),
            cwd: Some(PathBuf::from("/tmp")),
            agent: dash_common::AgentKind::Pi,
            is_self_focus: false,
        };
        app.infer_node(Node::Main);
        let fx = app.take_effects();
        assert_eq!(
            fx,
            vec![Effect::SpawnHeadless {
                agent: dash_common::AgentKind::Pi,
                mode: HeadlessMode::Infer,
                sid: "sid-123".into(),
                session_ref: Some("/tmp/pi-session.jsonl".into()),
                cwd: Some(PathBuf::from("/tmp")),
            }]
        );
    }

    #[test]
    fn copy_main_goal_emits_clipboard_effect() {
        let mut app = App::new(None);
        app.current_sid = Some("sid-123".into());
        app.state = Some(GoalState {
            text: "Ship the native goal pane".into(),
            ..Default::default()
        });
        app.copy_node(Node::Main);
        let fx = app.take_effects();
        assert_eq!(fx, vec![Effect::CopyText("Ship the native goal pane".into())]);
        assert_eq!(app.status, "copied");
    }

    #[test]
    fn clicking_copy_chip_emits_clipboard_effect() {
        let mut app = App::new(None);
        app.current_sid = Some("sid-123".into());
        app.state = Some(GoalState {
            text: "Ship the native goal pane".into(),
            ..Default::default()
        });
        app.nodes = vec![Node::Main];
        app.sel_node = 0;
        app.chip_rects = vec![(Rect { x: 3, y: 2, width: 4, height: 1 }, Act::Copy)];
        let fx = app.handle(A::Click(3, 2));
        assert_eq!(fx, vec![Effect::CopyText("Ship the native goal pane".into())]);
    }
}
