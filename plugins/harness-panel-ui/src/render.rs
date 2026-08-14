//! Panel rendering.
//!
//! # Responsive rule, mirrored from the website audit
//!
//! `docs/mobile-responsive-report.md` fixed the GUI at two real breakpoints:
//! `≤700px` (phone: one nav row, a 2-column action grid, chips that wrap, no
//! horizontal page scroll) and the desktop layout above it (columns, inline
//! actions). The panel has the same problem in columns instead of pixels, so it
//! takes the same two decisions:
//!
//! | site | panel | behaviour |
//! |---|---|---|
//! | ≤700px | `< WIDE_COLS` | stacked rows: name on its own line, meta indented under it, actions on one row of short labels |
//! | >700px | `≥ WIDE_COLS` | columnar rows: name, status, ctx and sessions on one line, actions inline |
//!
//! The site's other two rules carry over literally: nothing may overflow the
//! panel horizontally (every row is truncated to the rect, never wrapped into
//! the neighbouring pane), and long values wrap or ellipsize rather than sliding
//! under the control to their right.

use crate::model::{Snapshot, VIEWS};
use herdr_ui_plugin_api::{UiHitRect, UiNode, UiTextSpan};

/// The panel-width breakpoint, in columns. Below this the panel stacks; at or
/// above it, rows go columnar. 44 is where `name + status + ctx + sessions`
/// stops fitting on one line without truncating the name to uselessness.
pub const WIDE_COLS: u16 = 44;

pub struct Ctx {
    pub width: u16,
    pub height: u16,
    pub hover: Option<usize>,
    pub scroll: usize,
}

impl Ctx {
    pub fn wide(&self) -> bool {
        self.width >= WIDE_COLS
    }
}

pub struct Painted {
    pub nodes: Vec<UiNode>,
    pub hits: Vec<UiHitRect>,
    /// Row count before scroll/clip, so the guest can bound scrolling.
    pub total_rows: usize,
}

fn span(text: impl Into<String>, role: Option<&str>, bold: bool) -> UiTextSpan {
    UiTextSpan {
        text: text.into(),
        role: role.map(str::to_string),
        bold,
        ..Default::default()
    }
}

fn line(spans: Vec<UiTextSpan>) -> UiNode {
    UiNode::Line { spans }
}

/// Truncate to `max` columns with an ellipsis. The panel must never render a
/// row wider than its rect: herdr clips, but a clipped row reads as a bug and
/// the site's own fix for this was to wrap or ellipsize rather than overflow.
pub fn fit(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if max == 0 {
        return String::new();
    }
    if count <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// The daemon's real status vocabulary, enumerated from a live fleet rather than
/// guessed: `stopped`, `idle`, `busy`, `profile`. An earlier version only knew
/// `running`/`live`, so every idle and busy harness fell through to the muted
/// "stopped" styling — a live fleet rendered as if nothing was up. `running` and
/// the error spellings are kept as aliases in case the daemon widens the set.
fn status_role(status: &str) -> Option<&'static str> {
    match status {
        "busy" => Some("warning"),
        "idle" | "running" | "live" => Some("success"),
        "profile" => Some("teal"),
        "starting" | "pending" => Some("warning"),
        "error" | "failed" => Some("danger"),
        _ => Some("muted"),
    }
}

fn status_glyph(status: &str) -> &'static str {
    // No images in a terminal, so status degrades to a glyph + colour — the
    // "terminal-capable" rule from the brief.
    match status {
        // Live and working vs live and waiting are the distinction an operator
        // scans for, so they get different glyphs, not just different colours.
        "busy" => "◉",
        "idle" | "running" | "live" => "●",
        "profile" => "◈",
        "starting" | "pending" => "◐",
        "error" | "failed" => "✖",
        _ => "○",
    }
}

/// Header: title, connection state and the view tabs. Tabs are hit rects, so
/// they are clickable exactly like the site's nav.
fn header(snap: &Snapshot, ctx: &Ctx, out: &mut Painted, y: &mut u16) {
    let live = if snap.connected { "live" } else { "offline" };
    let live_role = if snap.connected { "success" } else { "danger" };
    out.nodes.push(line(vec![
        span("harness fleet ", None, true),
        span(live, Some(live_role), false),
    ]));
    *y += 1;

    // Tab row. On a narrow panel the labels alone would overflow, so tabs wrap
    // onto a second row — the same decision the site made for its topbar nav.
    let mut x: u16 = 0;
    let active = snap.view_or_default();
    let mut spans: Vec<UiTextSpan> = Vec::new();
    for (id, label) in VIEWS.iter() {
        let cell = format!(" {label} ");
        let w = cell.chars().count() as u16;
        if x + w > ctx.width && !spans.is_empty() {
            out.nodes.push(line(std::mem::take(&mut spans)));
            *y += 1;
            x = 0;
        }
        let is_active = *id == active;
        spans.push(span(
            cell,
            Some(if is_active { "blue" } else { "subtext" }),
            is_active,
        ));
        out.hits.push(UiHitRect {
            x,
            y: *y,
            width: w,
            height: 1,
            action_id: format!("view:{id}"),
        });
        x += w;
    }
    if !spans.is_empty() {
        out.nodes.push(line(spans));
        *y += 1;
    }
}

/// Footer: provenance. The site has a connection dot; the panel says WHY the
/// data looks the way it does, which is the same information plus the honesty
/// about push vs poll.
fn footer(snap: &Snapshot, ctx: &Ctx, out: &mut Painted) {
    if let Some(op) = &snap.last_op {
        let role = if op.ok { "success" } else { "danger" };
        let mark = if op.ok { "✓" } else { "✖" };
        out.nodes.push(line(vec![span(
            fit(
                &format!("{mark} {} {}: {}", op.action, op.target, op.message),
                ctx.width as usize,
            ),
            Some(role),
            false,
        )]));
    }
    out.nodes.push(line(vec![span(
        fit(&format!("· {}", snap.reason), ctx.width as usize),
        Some("muted"),
        false,
    )]));
}

/// One fleet row, responsive. Returns how many lines it drew.
fn fleet_row(
    h: &crate::model::Harness,
    ctx: &Ctx,
    y: u16,
    hovered: bool,
    out: &mut Painted,
) -> u16 {
    let sel_role = if hovered { "blue" } else { "default" };
    let w = ctx.width as usize;
    if ctx.wide() {
        // Columnar: everything on one line, name truncated first because the
        // numbers are fixed width and the name is not.
        let tail = format!(" {}s {}p", h.sessions, h.pending);
        let name_w = w.saturating_sub(4 + tail.chars().count());
        out.nodes.push(line(vec![
            span(
                format!("{} ", status_glyph(&h.status)),
                status_role(&h.status),
                false,
            ),
            span(fit(&h.name, name_w), Some(sel_role), hovered),
            span(tail, Some("muted"), false),
        ]));
        out.hits.push(UiHitRect {
            x: 0,
            y,
            width: ctx.width,
            height: 1,
            action_id: format!("select:{}", h.name),
        });
        1
    } else {
        // Stacked: name on its own line, meta indented beneath — the panel's
        // version of the site's ≤700px stacking.
        out.nodes.push(line(vec![
            span(
                format!("{} ", status_glyph(&h.status)),
                status_role(&h.status),
                false,
            ),
            span(fit(&h.name, w.saturating_sub(2)), Some(sel_role), hovered),
        ]));
        out.hits.push(UiHitRect {
            x: 0,
            y,
            width: ctx.width,
            height: 1,
            action_id: format!("select:{}", h.name),
        });
        out.nodes.push(line(vec![span(
            fit(
                &format!("  {} · {}s · {}p", h.runtime, h.sessions, h.pending),
                w,
            ),
            Some("muted"),
            false,
        )]));
        2
    }
}

/// Action buttons for the selected harness.
///
/// They WRAP rather than overflow. The site hit this exact problem at ≤700px —
/// four buttons in one flex row pushed "Open in herdr" onto three lines — and
/// fixed it with a 2-column grid. The panel has no grid, so it lays the buttons
/// out left to right and breaks to a new row when the next one would cross the
/// rect. Returns the number of rows drawn so the caller can advance its cursor.
fn actions(target: &str, y: u16, width: u16, out: &mut Painted) -> u16 {
    const BUTTONS: [(&str, &str, &str); 3] = [
        ("[spawn]", "spawn", "success"),
        ("[stop]", "stop", "danger"),
        ("[refresh]", "refresh", "blue"),
    ];
    let mut x: u16 = 0;
    let mut row = y;
    let mut rows: u16 = 1;
    let mut spans: Vec<UiTextSpan> = Vec::new();
    for (label, action, role) in BUTTONS {
        let w = label.chars().count() as u16;
        // +1 for the space this button would be preceded by, when not first.
        let needed = if spans.is_empty() { w } else { w + 1 };
        if x + needed > width && !spans.is_empty() {
            out.nodes.push(line(std::mem::take(&mut spans)));
            row += 1;
            rows += 1;
            x = 0;
        }
        if !spans.is_empty() {
            spans.push(span(" ", None, false));
            x += 1;
        }
        spans.push(span(label, Some(role), true));
        out.hits.push(UiHitRect {
            x,
            y: row,
            width: w,
            height: 1,
            action_id: format!("{action}:{target}"),
        });
        x += w;
    }
    if !spans.is_empty() {
        out.nodes.push(line(spans));
    }
    rows
}

#[allow(unused_assignments)]
pub fn paint(snap: &Snapshot, ctx: &Ctx) -> Painted {
    let mut out = Painted {
        nodes: Vec::new(),
        hits: Vec::new(),
        total_rows: 0,
    };
    // Collapsed: one clickable line, and nothing else. This lives here rather
    // than in the ABI wrapper because it IS the rendered contract — a caller
    // that renders a closed snapshot must get the collapsed panel, and a test
    // that renders one must see it.
    //
    // Never an empty layer: a guest that stops emitting one is stranded, because
    // the host drops its carve and afterwards only ever sends a zero rect, so it
    // can never claim geometry again and freezes mid-screen.
    if !snap.open {
        out.nodes.push(line(vec![span(
            fit("▦ harness fleet — click to open", ctx.width as usize),
            Some("muted"),
            true,
        )]));
        out.hits.push(UiHitRect {
            x: 0,
            y: 0,
            width: ctx.width,
            height: 1,
            action_id: "toggle:panel".into(),
        });
        out.total_rows = 1;
        return out;
    }

    let mut y: u16 = 0;
    header(snap, ctx, &mut out, &mut y);

    let body_start = y;
    let w = ctx.width as usize;
    match snap.view_or_default() {
        "fleet" => {
            let rows: Vec<_> = snap.fleet.iter().skip(ctx.scroll).collect();
            out.total_rows = snap.fleet.len();
            for (i, h) in rows.iter().enumerate() {
                if y >= ctx.height.saturating_sub(2) {
                    break;
                }
                let hovered = ctx.hover == Some(i + ctx.scroll);
                y += fleet_row(h, ctx, y, hovered, &mut out);
                // Hover tooltip: the site shows a title attribute, the terminal
                // shows a line under the hovered row. Same information, the
                // affordance the medium has.
                if hovered {
                    out.nodes.push(line(vec![span(
                        fit(
                            &format!(
                                "  ↳ {} · {}",
                                h.template,
                                if h.host.is_empty() {
                                    "no host"
                                } else {
                                    &h.host
                                }
                            ),
                            w,
                        ),
                        Some("teal"),
                        false,
                    )]));
                    y += 1;
                }
            }
        }
        "detail" => match &snap.detail {
            Some(d) => {
                out.nodes.push(line(vec![
                    span(
                        format!("{} ", status_glyph(&d.status)),
                        status_role(&d.status),
                        false,
                    ),
                    span(fit(&d.name, w.saturating_sub(2)), None, true),
                ]));
                y += 1;
                for (k, v) in [
                    ("runtime", d.runtime.clone()),
                    ("template", d.template.clone()),
                    ("status", d.status.clone()),
                    ("pending", d.pending.to_string()),
                    ("sessions", d.sessions.len().to_string()),
                ] {
                    out.nodes.push(line(vec![
                        span(format!("{k:<9}"), Some("muted"), false),
                        span(fit(&v, w.saturating_sub(9)), None, false),
                    ]));
                    y += 1;
                }
                if !d.live_rules.is_empty() {
                    out.nodes
                        .push(line(vec![span("live rules", Some("subtext"), true)]));
                    y += 1;
                    for l in d.live_rules.lines().take(4) {
                        out.nodes
                            .push(line(vec![span(fit(l, w), Some("muted"), false)]));
                        y += 1;
                    }
                }
                y += actions(&d.name, y, ctx.width, &mut out);
            }
            None => {
                out.nodes.push(line(vec![span(
                    fit("pick a harness in Fleet", w),
                    Some("muted"),
                    false,
                )]));
                y += 1;
            }
        },
        "orchestrator" => {
            out.total_rows = snap.tree.len();
            for n in snap.tree.iter().skip(ctx.scroll) {
                if y >= ctx.height.saturating_sub(2) {
                    break;
                }
                // Parentage as indentation: the site draws a tree, the panel
                // draws the same tree with one level of indent.
                let indent = if n.parent.is_empty() { "" } else { "  ├ " };
                let live = if n.live { "◆" } else { " " };
                out.nodes.push(line(vec![
                    span(
                        format!("{live}{indent}"),
                        Some(if n.live { "success" } else { "muted" }),
                        false,
                    ),
                    span(
                        fit(&n.name, w.saturating_sub(indent.chars().count() + 1)),
                        None,
                        n.live,
                    ),
                ]));
                y += 1;
                if ctx.wide() && !n.task.is_empty() {
                    out.nodes.push(line(vec![span(
                        fit(&format!("     {}", n.task), w),
                        Some("muted"),
                        false,
                    )]));
                    y += 1;
                }
            }
        }
        "templates" => {
            out.total_rows = snap.templates.len();
            for t in snap.templates.iter().skip(ctx.scroll) {
                if y >= ctx.height.saturating_sub(2) {
                    break;
                }
                out.nodes.push(line(vec![
                    span(fit(&t.name, w.saturating_sub(6)), None, true),
                    span(format!(" v{}", t.version), Some("muted"), false),
                ]));
                y += 1;
                if !t.description.is_empty() {
                    out.nodes.push(line(vec![span(
                        fit(&format!("  {}", t.description), w),
                        Some("muted"),
                        false,
                    )]));
                    y += 1;
                }
            }
        }
        "models" => {
            if let Some(map) = snap.models.as_object() {
                out.total_rows = map.len();
                for (runtime, v) in map.iter().skip(ctx.scroll) {
                    if y >= ctx.height.saturating_sub(2) {
                        break;
                    }
                    out.nodes
                        .push(line(vec![span(fit(runtime, w), Some("blue"), true)]));
                    y += 1;
                    // /api/models returns OBJECTS per runtime, not strings:
                    //   {"claude":[{"id":"fable","enabled":true,"verified":true,…}]}
                    // Reading these with `as_str` yields "" for every entry, which
                    // renders as a runtime header followed by blank lines — the
                    // view looks broken while the data is perfectly fine. Accept a
                    // bare string too, so a runtime that reports the simpler shape
                    // still lists.
                    let items: Vec<String> = match v {
                        serde_json::Value::Array(a) => a
                            .iter()
                            .filter_map(|x| match x {
                                serde_json::Value::String(s) => Some(s.clone()),
                                serde_json::Value::Object(o) => {
                                    let id = o.get("id")?.as_str()?;
                                    // A disabled or unverified model is worth
                                    // seeing as such rather than silently listed
                                    // alongside working ones.
                                    let mark = match (
                                        o.get("enabled").and_then(serde_json::Value::as_bool),
                                        o.get("verified").and_then(serde_json::Value::as_bool),
                                    ) {
                                        (Some(false), _) => " off",
                                        (_, Some(false)) => " ?",
                                        _ => "",
                                    };
                                    Some(format!("{id}{mark}"))
                                }
                                _ => None,
                            })
                            .collect(),
                        serde_json::Value::Object(o) => o.keys().cloned().collect(),
                        _ => Vec::new(),
                    };
                    for m in items.iter().take(if ctx.wide() { 6 } else { 3 }) {
                        out.nodes.push(line(vec![span(
                            fit(&format!("  {m}"), w),
                            Some("muted"),
                            false,
                        )]));
                        y += 1;
                    }
                }
            } else {
                out.nodes.push(line(vec![span(
                    fit("no model catalog", w),
                    Some("muted"),
                    false,
                )]));
                y += 1;
            }
        }
        "memory" => match &snap.detail {
            Some(d) if !d.memories.is_empty() => {
                out.total_rows = d.memories.len();
                for m in d.memories.iter().skip(ctx.scroll) {
                    if y >= ctx.height.saturating_sub(2) {
                        break;
                    }
                    out.nodes
                        .push(line(vec![span(fit(&m.name, w), None, true)]));
                    y += 1;
                    for l in m.body.lines().take(2) {
                        out.nodes.push(line(vec![span(
                            fit(&format!("  {l}"), w),
                            Some("muted"),
                            false,
                        )]));
                        y += 1;
                    }
                }
            }
            Some(d) => {
                out.nodes.push(line(vec![span(
                    fit(&format!("no memories under {}", d.memory_dir), w),
                    Some("muted"),
                    false,
                )]));
                y += 1;
            }
            None => {
                out.nodes.push(line(vec![span(
                    fit("pick a harness first", w),
                    Some("muted"),
                    false,
                )]));
                y += 1;
            }
        },
        other => {
            out.nodes.push(line(vec![span(
                fit(&format!("unknown view {other}"), w),
                Some("danger"),
                false,
            )]));
            y += 1;
        }
    }
    let _ = body_start;
    footer(snap, ctx, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Harness;

    fn snap_with(fleet: Vec<Harness>) -> Snapshot {
        Snapshot {
            connected: true,
            view: "fleet".into(),
            fleet,
            ..Default::default()
        }
    }

    fn text_of(p: &Painted) -> String {
        p.nodes
            .iter()
            .map(|n| match n {
                UiNode::Line { spans } => spans.iter().map(|s| s.text.clone()).collect::<String>(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn harness(name: &str, status: &str) -> Harness {
        Harness {
            name: name.into(),
            status: status.into(),
            runtime: "claude".into(),
            sessions: 2,
            ..Default::default()
        }
    }

    #[test]
    fn fit_never_exceeds_the_panel_width() {
        for w in 0..12usize {
            assert!(
                fit("a-very-long-harness-name", w).chars().count() <= w,
                "width {w}"
            );
        }
    }

    #[test]
    fn narrow_stacks_and_wide_goes_columnar() {
        let s = snap_with(vec![harness("alpha", "running")]);
        let narrow = paint(
            &s,
            &Ctx {
                width: 30,
                height: 20,
                hover: None,
                scroll: 0,
            },
        );
        let wide = paint(
            &s,
            &Ctx {
                width: 60,
                height: 20,
                hover: None,
                scroll: 0,
            },
        );
        // Stacked draws the meta on its own indented line; columnar does not.
        assert!(
            text_of(&narrow).contains("\n  claude"),
            "narrow should stack: {}",
            text_of(&narrow)
        );
        assert!(
            !text_of(&wide).contains("\n  claude"),
            "wide should be columnar: {}",
            text_of(&wide)
        );
    }

    #[test]
    fn every_row_fits_the_rect_at_any_width() {
        let s = snap_with(vec![harness("a-really-quite-long-harness-name", "running")]);
        for w in [20u16, 30, 44, 80] {
            let p = paint(
                &s,
                &Ctx {
                    width: w,
                    height: 20,
                    hover: None,
                    scroll: 0,
                },
            );
            for l in text_of(&p).lines() {
                assert!(
                    l.chars().count() <= w as usize,
                    "line {l:?} exceeds width {w}"
                );
            }
        }
    }

    #[test]
    fn tabs_are_clickable_for_every_view() {
        let s = snap_with(vec![]);
        let p = paint(
            &s,
            &Ctx {
                width: 60,
                height: 20,
                hover: None,
                scroll: 0,
            },
        );
        for (id, _) in VIEWS.iter() {
            assert!(
                p.hits.iter().any(|h| h.action_id == format!("view:{id}")),
                "no hit rect for view {id}"
            );
        }
    }

    #[test]
    fn fleet_rows_are_selectable_and_hover_shows_a_tooltip() {
        let s = snap_with(vec![harness("alpha", "running")]);
        let plain = paint(
            &s,
            &Ctx {
                width: 60,
                height: 20,
                hover: None,
                scroll: 0,
            },
        );
        assert!(plain.hits.iter().any(|h| h.action_id == "select:alpha"));
        assert!(!text_of(&plain).contains('↳'));
        let hovered = paint(
            &s,
            &Ctx {
                width: 60,
                height: 20,
                hover: Some(0),
                scroll: 0,
            },
        );
        assert!(
            text_of(&hovered).contains('↳'),
            "hover must add a tooltip line"
        );
    }

    #[test]
    fn detail_view_offers_the_three_mutations() {
        let mut s = snap_with(vec![]);
        s.view = "detail".into();
        s.detail = Some(crate::model::Detail {
            name: "alpha".into(),
            status: "running".into(),
            ..Default::default()
        });
        let p = paint(
            &s,
            &Ctx {
                width: 50,
                height: 24,
                hover: None,
                scroll: 0,
            },
        );
        for a in ["spawn", "stop", "refresh"] {
            assert!(
                p.hits.iter().any(|h| h.action_id == format!("{a}:alpha")),
                "missing {a}"
            );
        }
    }

    #[test]
    fn scroll_skips_rows_rather_than_clipping_them_away() {
        let fleet: Vec<Harness> = (0..10)
            .map(|i| harness(&format!("h{i}"), "stopped"))
            .collect();
        let s = snap_with(fleet);
        let p = paint(
            &s,
            &Ctx {
                width: 60,
                height: 20,
                hover: None,
                scroll: 4,
            },
        );
        let t = text_of(&p);
        assert!(!t.contains("h0"), "scrolled-past row still drawn");
        assert!(t.contains("h4"), "scrolled-to row missing");
        assert_eq!(p.total_rows, 10, "total must reflect the unscrolled list");
    }

    #[test]
    fn an_offline_collector_still_renders_the_last_state() {
        let mut s = snap_with(vec![harness("alpha", "running")]);
        s.connected = false;
        let p = paint(
            &s,
            &Ctx {
                width: 50,
                height: 20,
                hover: None,
                scroll: 0,
            },
        );
        let t = text_of(&p);
        assert!(t.contains("offline"), "must say it is offline");
        assert!(t.contains("alpha"), "must still show the last known fleet");
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    /// The four statuses a live daemon actually emits, enumerated from
    /// /api/fleet: stopped, idle, busy, profile. Each must be visually distinct
    /// from "stopped", or a running fleet reads as a dead one.
    #[test]
    fn the_real_status_vocabulary_is_distinguished() {
        let stopped = (status_glyph("stopped"), status_role("stopped"));
        for live in ["idle", "busy", "profile"] {
            let got = (status_glyph(live), status_role(live));
            assert_ne!(
                got, stopped,
                "status {live:?} renders identically to stopped"
            );
        }
        // busy and idle are both live but mean different things to an operator.
        assert_ne!(
            (status_glyph("busy"), status_role("busy")),
            (status_glyph("idle"), status_role("idle")),
            "busy and idle must be tellable apart"
        );
    }

    #[test]
    fn an_unknown_status_degrades_quietly() {
        assert_eq!(status_glyph("something-new"), "○");
        assert_eq!(status_role("something-new"), Some("muted"));
    }
}
