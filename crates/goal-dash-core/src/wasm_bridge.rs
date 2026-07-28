//! Host↔guest bridge for the WASM goal-dash UI plugin.
//!
//! [`App::build_render_model`] projects the live native [`App`] into the
//! wasm-safe [`goal_dash_model::GoalRenderModel`] each frame — precomputing
//! every clock/filesystem-derived label (the guest has no chrono / `std::fs`).
//! [`App::apply_hits`] writes the guest-computed [`goal_dash_model::GoalHits`]
//! back into `App`'s pub(crate) rect caches so the existing
//! `App::handle(GoalAction::Click)` resolves clicks unchanged.

use ratatui::layout::Rect;

use goal_dash_model as gm;

use crate::model::{Par, St};
use crate::timegate::{est_label, fmt_updated, minutes_until, short_time};
use crate::view::{Act, EditTarget, Node, View};

// ---- enum mappings (native → model) ----------------------------------------

fn view_to_model(v: View) -> gm::View {
    match v {
        View::Tree => gm::View::Tree,
        View::History => gm::View::History,
        View::Proposals => gm::View::Proposals,
        View::PromptLog => gm::View::PromptLog,
    }
}

fn par_to_model(p: Par) -> gm::Par {
    match p {
        Par::Current => gm::Par::Current,
        Par::Main => gm::Par::Main,
        Par::Indep => gm::Par::Indep,
    }
}

fn st_to_model(s: St) -> gm::St {
    match s {
        St::Active => gm::St::Active,
        St::Paused => gm::St::Paused,
        St::Done => gm::St::Done,
        St::Discarded => gm::St::Discarded,
    }
}

fn node_to_model(n: Node) -> gm::Node {
    match n {
        Node::Main => gm::Node::Main,
        Node::Current => gm::Node::Current,
        Node::Section(p, s) => gm::Node::Section(par_to_model(p), st_to_model(s)),
        Node::Sub(id) => gm::Node::Sub(id),
    }
}

fn act_to_tag(a: Act) -> gm::ActTag {
    match a {
        Act::Approve => gm::ActTag::Approve,
        Act::Edit => gm::ActTag::Edit,
        Act::Infer => gm::ActTag::Infer,
        Act::Log => gm::ActTag::Log,
        Act::Copy => gm::ActTag::Copy,
        Act::AddSub => gm::ActTag::AddSub,
        Act::State => gm::ActTag::State,
        Act::Discard => gm::ActTag::Discard,
        Act::ClearRebrief => gm::ActTag::ClearRebrief,
        Act::Sort => gm::ActTag::Sort,
    }
}

fn tag_to_act(t: gm::ActTag) -> Act {
    match t {
        gm::ActTag::Approve => Act::Approve,
        gm::ActTag::Edit => Act::Edit,
        gm::ActTag::Infer => Act::Infer,
        gm::ActTag::Log => Act::Log,
        gm::ActTag::Copy => Act::Copy,
        gm::ActTag::AddSub => Act::AddSub,
        gm::ActTag::State => Act::State,
        gm::ActTag::Discard => Act::Discard,
        gm::ActTag::ClearRebrief => Act::ClearRebrief,
        gm::ActTag::Sort => Act::Sort,
    }
}

fn edit_target_to_model(t: &EditTarget) -> gm::EditTarget {
    match t {
        EditTarget::Main => gm::EditTarget::Main,
        EditTarget::Current => gm::EditTarget::Current,
        EditTarget::Sub(id) => gm::EditTarget::Sub(*id),
        EditTarget::NewSub => gm::EditTarget::NewSub,
        EditTarget::TimeMain => gm::EditTarget::TimeMain,
        EditTarget::TimeCurrent => gm::EditTarget::TimeCurrent,
        EditTarget::ConstraintMain => gm::EditTarget::ConstraintMain,
        EditTarget::ConstraintCurrent => gm::EditTarget::ConstraintCurrent,
    }
}

fn data_to_rect(d: &gm::RectData) -> Rect {
    Rect::new(d.x, d.y, d.width, d.height)
}

// ---- the two host contract methods -----------------------------------------

impl crate::state::App {
    /// Project the live `App` into the wasm-safe per-frame render model. All
    /// chrono/filesystem-derived labels are precomputed here (the guest can't
    /// reach a clock or the disk).
    pub fn build_render_model(&self) -> gm::GoalRenderModel {
        let main_state = self.state.clone().unwrap_or_default();
        // MAIN always projects (the renderer shows an unset placeholder when empty).
        let main = Some(goal_node_data(&main_state, false));

        let cur = &self.tree.current;
        let agent_changed = cur.source == "agent" && cur.reason.is_some();
        let current = goal_node_data(cur, agent_changed);

        let subs: Vec<gm::SubData> = self
            .tree
            .subs
            .iter()
            .map(|s| gm::SubData {
                id: s.id,
                text: s.text.clone(),
                source: s.source.clone(),
                approved: s.approved,
                state: s.state.clone(),
                parent: s.parent.clone(),
            })
            .collect();

        let toggled: Vec<(gm::Par, gm::St)> = self
            .toggled
            .iter()
            .map(|(p, s)| (par_to_model(*p), st_to_model(*s)))
            .collect();

        let constraints: Vec<gm::ConstraintData> = self
            .constraints
            .iter()
            .map(|c| gm::ConstraintData {
                entity: c.entity.clone(),
                text: c.text.clone(),
                source: c.source.clone(),
            })
            .collect();

        let cons_expanded: Vec<String> = self.cons_expanded.iter().cloned().collect();

        let proposals: Vec<gm::ProposalData> = self
            .proposals
            .iter()
            .map(|p| gm::ProposalData {
                kind: p.kind.clone(),
                text: p.text.clone(),
                reason: p.reason.clone(),
            })
            .collect();

        let prop_expanded: Vec<usize> = self.prop_expanded.iter().copied().collect();

        let plog: Vec<gm::PromptEntryData> = self
            .plog
            .iter()
            .map(|e| gm::PromptEntryData {
                ts: short_time(&e.ts),
                paraphrase: e.paraphrase.clone(),
                action: e.action.clone(),
                reason: e.reason.clone(),
            })
            .collect();

        let history: Vec<gm::HistoryData> = self
            .history
            .iter()
            .map(|h| gm::HistoryData {
                text: h.text.clone(),
                source: h.source.clone(),
                approved: h.approved,
                updated_at: short_time(&h.updated_at),
                reason: h.reason.clone(),
            })
            .collect();

        let nodes: Vec<gm::Node> = self.nodes.iter().map(|n| node_to_model(*n)).collect();

        // Chip specs for the selected node (mirrors render's `chip_specs(sel)`).
        let sel_chips: Vec<gm::ChipData> = match self.nodes.get(self.sel_node).copied() {
            Some(n) => self
                .chip_specs(n)
                .into_iter()
                .map(|c| gm::ChipData {
                    act: act_to_tag(c.act),
                    glyph: c.glyph,
                    tip: c.tip,
                })
                .collect(),
            None => Vec::new(),
        };

        // Plans rows with host-precomputed labels + per-row expansion state.
        let plans: Vec<gm::PlanData> = self
            .plans
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let is_current = i == 0;
                let expanded = if is_current {
                    !self.plans_current_folded
                } else {
                    self.plans_expanded.contains(&p.basename)
                };
                gm::PlanData {
                    basename: p.basename.clone(),
                    title: p.title.clone(),
                    updated_label: fmt_updated(p.mtime_ms),
                    est_label: est_label(p.p50),
                    is_current,
                    expanded,
                    body: if expanded {
                        self.plans_body.get(&p.basename).cloned()
                    } else {
                        None
                    },
                }
            })
            .collect();

        gm::GoalRenderModel {
            view: view_to_model(self.view),
            has_session: self.current_sid.is_some(),
            main,
            current,
            subs,
            toggled,
            constraints,
            cons_expanded,
            cons_sel: self.cons_sel.clone(),
            proposals,
            sel_prop: self.sel_prop,
            prop_expanded,
            plog,
            history,
            sel_hist: self.sel_hist,
            nodes,
            sel_node: self.sel_node,
            sel_ctrl: self.sel_ctrl,
            sel_chips,
            plans_label: self.plans_label.clone(),
            plans,
            plans_section_open: self.plans_section_open,
            paraphrase: self.paraphrase.clone(),
            status: self.status.clone(),
            editing: self.editing,
            edit_target: edit_target_to_model(&self.edit_target),
            edit_buf: self.edit_buf.clone(),
            hover: self.hover,
        }
    }

    /// Write the guest-computed hit-rect caches back onto `App` so the existing
    /// `handle(GoalAction::Click)` resolves clicks unchanged.
    pub fn apply_hits(&mut self, hits: gm::GoalHits) {
        self.node_rects = hits
            .node_rects
            .into_iter()
            .map(|(r, i)| (data_to_rect(&r), i))
            .collect();
        self.cons_header_rects = hits
            .cons_header_rects
            .into_iter()
            .map(|(r, e)| (data_to_rect(&r), e))
            .collect();
        self.chip_rects = hits
            .chip_rects
            .into_iter()
            .map(|(r, a)| (data_to_rect(&r), tag_to_act(a)))
            .collect();
        self.controls = hits
            .controls
            .into_iter()
            .map(|(r, a)| (data_to_rect(&r), tag_to_act(a)))
            .collect();
        self.plans_head_rect = hits.plans_head_rect.map(|r| data_to_rect(&r));
        self.plans_row_rects = hits
            .plans_row_rects
            .into_iter()
            .map(|(r, b)| (data_to_rect(&r), b))
            .collect();
        self.plog_rects = hits
            .plog_rects
            .into_iter()
            .map(|(r, i)| (data_to_rect(&r), i))
            .collect();
        self.prop_rects = hits
            .prop_rects
            .into_iter()
            .map(|(r, i)| (data_to_rect(&r), i))
            .collect();
        self.hist_rects = hits
            .hist_rects
            .into_iter()
            .map(|(r, i)| (data_to_rect(&r), i))
            .collect();
        self.banner_rect = hits.banner_rect.map(|r| data_to_rect(&r));
        self.tip_rects = hits
            .tip_rects
            .into_iter()
            .map(|(r, t)| (data_to_rect(&r), t))
            .collect();
    }
}

/// Project one `GoalState` into a `GoalNodeData`, precomputing the time labels
/// the native `meta_lines` renders (the guest has no chrono).
fn goal_node_data(st: &crate::model::GoalState, agent_changed: bool) -> gm::GoalNodeData {
    gm::GoalNodeData {
        text: st.text.clone(),
        source: st.source.clone(),
        approved: st.approved,
        set_label: if st.set_at.is_empty() {
            "—".to_string()
        } else {
            short_time(&st.set_at)
        },
        changed_label: if st.updated_at.is_empty() {
            "—".to_string()
        } else {
            short_time(&st.updated_at)
        },
        est_label: est_label(st.est_minutes),
        due_label: if st.deadline.is_empty() {
            "—".to_string()
        } else {
            short_time(&st.deadline)
        },
        minutes_until: minutes_until(&st.deadline),
        agent_changed,
    }
}
