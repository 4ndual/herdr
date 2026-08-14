//! Byte-level contract between the Go collector and the wasm guest.
//!
//! The two halves are different languages, built separately, that meet only at
//! one artifact: the bytes the collector writes to `harness_panel_input.json`
//! and the host hands the guest as `HERDR_PLUGIN_UI_INPUT_JSON`. Unit tests on
//! either side can both pass while that contract is broken — which is exactly
//! what happened once already: `/api/fleet` reports `sessions` as a COUNT and
//! `/api/harness/<name>` as an ARRAY, and a single mistyped field silently
//! failed the whole detail decode.
//!
//! So these tests do not construct a snapshot. Every fixture under `testdata/`
//! is a file the REAL collector wrote while connected to the REAL daemon, one
//! per view, copied byte for byte. If the collector's output shape drifts from
//! what the guest parses, these fail.
//!
//! Regenerate with the collector running against a live daemon:
//!
//! ```text
//! harness-panel-collector op -action view -view <view>   # or -action select -target <name>
//! cp "$HERDR_PLUGIN_STATE_DIR/harness_panel_input.json" testdata/<view>.json
//! ```

use harness_panel_ui::testing::{parse_snapshot, render_text, RenderSize};

/// Every fixture, and the view it was captured in.
const FIXTURES: [(&str, &str); 6] = [
    ("fleet.json", "fleet"),
    ("detail.json", "detail"),
    ("orchestrator.json", "orchestrator"),
    ("templates.json", "templates"),
    ("models.json", "models"),
    ("memory.json", "memory"),
];

fn fixture(name: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/");
    std::fs::read_to_string(format!("{path}{name}"))
        .unwrap_or_else(|err| panic!("fixture {name} unreadable: {err}"))
}

#[test]
fn every_fixture_parses_as_the_guest_reads_it() {
    for (file, view) in FIXTURES {
        let raw = fixture(file);
        let snap = parse_snapshot(&raw)
            .unwrap_or_else(|| panic!("{file}: collector output did not parse as a Snapshot"));
        assert_eq!(snap.view, view, "{file}: captured view mismatch");
        assert!(
            snap.connected,
            "{file}: fixture should have been captured live"
        );
        assert!(
            !snap.fleet.is_empty(),
            "{file}: fleet is push-fed and must be present in every view"
        );
    }
}

/// The decode that actually broke once: `sessions` is an int in the fleet list
/// and an array of objects in the detail payload.
#[test]
fn the_sessions_shape_clash_stays_decoded() {
    let snap = parse_snapshot(&fixture("detail.json")).expect("detail parses");
    let detail = snap.detail.as_ref().expect("detail payload present");
    assert!(
        !detail.sessions.is_empty(),
        "detail.sessions must decode as a list — an int here silently drops the whole document"
    );
    assert!(
        snap.fleet.iter().any(|h| h.sessions >= 0),
        "fleet.sessions must still decode as a count"
    );
    assert!(!detail.name.is_empty(), "detail must name its harness");
}

#[test]
fn each_view_renders_its_own_data_from_real_bytes() {
    let wide = RenderSize {
        width: 60,
        height: 40,
    };

    let fleet = render_text(&fixture("fleet.json"), wide);
    assert!(fleet.contains("Fleet"), "fleet view missing its tab row");
    assert!(
        fleet.lines().count() > 3,
        "fleet view rendered no rows:\n{fleet}"
    );

    let detail = render_text(&fixture("detail.json"), wide);
    assert!(
        detail.contains("e2e-claude"),
        "detail view lost its harness:\n{detail}"
    );
    assert!(
        detail.contains("runtime"),
        "detail view missing its field table"
    );

    let orch = render_text(&fixture("orchestrator.json"), wide);
    assert!(
        orch.lines().count() > 3,
        "orchestrator drew no tree nodes:\n{orch}"
    );

    let tmpl = render_text(&fixture("templates.json"), wide);
    assert!(
        tmpl.contains('v'),
        "templates view lost its version column:\n{tmpl}"
    );

    let models = render_text(&fixture("models.json"), wide);
    assert!(
        ["claude", "codex", "pi", "jcode"]
            .iter()
            .any(|r| models.contains(r)),
        "models view listed no runtime:\n{models}"
    );

    // Memory renders either the memories or an explicit empty state — never a
    // blank panel, which would read as a broken view.
    let memory = render_text(&fixture("memory.json"), wide);
    assert!(
        memory.lines().count() > 2,
        "memory view rendered nothing at all:\n{memory}"
    );
}

/// The responsive rule, checked against real data rather than a toy fleet: at
/// no width may a row bleed past the panel rect into the neighbouring pane.
#[test]
fn no_row_overflows_the_panel_at_any_width_on_real_data() {
    for (file, _) in FIXTURES {
        let raw = fixture(file);
        for width in [20u16, 28, 34, 44, 60, 80] {
            let text = render_text(&raw, RenderSize { width, height: 40 });
            for line in text.lines() {
                assert!(
                    line.chars().count() <= width as usize,
                    "{file} at {width} cols: {line:?} is {} wide",
                    line.chars().count()
                );
            }
        }
    }
}

/// Both layouts must actually differ on real data — a breakpoint that renders
/// the same thing on both sides is not a breakpoint.
#[test]
fn the_breakpoint_changes_the_layout_on_real_data() {
    let raw = fixture("fleet.json");
    let narrow = render_text(
        &raw,
        RenderSize {
            width: 30,
            height: 40,
        },
    );
    let wide = render_text(
        &raw,
        RenderSize {
            width: 60,
            height: 40,
        },
    );
    assert_ne!(
        narrow, wide,
        "narrow and wide rendered identically — the responsive rule is dead"
    );
    // Stacked puts the runtime on its own indented line; columnar does not.
    assert!(
        narrow.contains("\n  claude") || narrow.contains("\n  codex"),
        "narrow should stack the meta under the name:\n{narrow}"
    );
}

/// A truncated or corrupt input must degrade to an empty panel, never a panic:
/// the guest runs in wasm with `panic = "abort"`, so a parse panic takes the
/// module down and the host has to re-instantiate it.
#[test]
fn corrupt_input_degrades_instead_of_trapping() {
    let raw = fixture("fleet.json");
    for cut in [0usize, 1, 17, 250, raw.len() / 2] {
        let truncated = &raw[..cut.min(raw.len())];
        let _ = parse_snapshot(truncated);
        let _ = render_text(
            truncated,
            RenderSize {
                width: 40,
                height: 20,
            },
        );
    }
    let _ = render_text(
        "null",
        RenderSize {
            width: 40,
            height: 20,
        },
    );
    let _ = render_text(
        "{}",
        RenderSize {
            width: 40,
            height: 20,
        },
    );
}
