//! fork(+128): render-time compositor for plugin-owned `pane.text` layers.
//!
//! ## Why this is a compositor and not an editor
//!
//! The obvious reading of "let plugins edit the text in a pane" is a mutation
//! API: hand the plugin a way to change what the terminal holds. That was
//! rejected. A pane's contents are what an agent printed, and the same bytes are
//! read back by `pane.read`, by copy, by search, by agent detection, and by an
//! agent resuming its own session. A plugin that could rewrite them could make
//! an agent believe it had said something it never said, and nothing downstream
//! would be able to tell the difference.
//!
//! So overlays composite instead. Every layer is painted into the ratatui buffer
//! for one frame, on top of pane content, and discarded. Nothing in `PaneState`
//! or scrollback is touched. The user sees the annotation; every reader still
//! sees the truth. This is the property `.local/prd/127-plugin-owned-images.md`
//! reserved for this stage, and it is the reason the API is `pane.text.set`
//! rather than `pane.write`.
//!
//! ## Relationship to `pane_graphics_paint`
//!
//! Same geometry, deliberately: `layer_area` is shared rather than
//! reimplemented, so an image layer and a text layer given the same placement
//! land on exactly the same cells. Text is painted AFTER images in the frame
//! (see the call site in `app/mod.rs`), which makes captions over images work
//! regardless of the order the two API calls arrived in.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use unicode_width::UnicodeWidthChar;

use crate::app::state::{AppState, PaneTextLayer};
use crate::pane_graphics_paint::layer_area;

/// Paint every retained text overlay of the active tab into `buf`.
pub(crate) fn paint_pane_text(app: &AppState, buf: &mut Buffer) {
    // Same suppression rule as the graphics painter: these run after the frame
    // is drawn, so without this an overlay would paint straight over an open
    // modal.
    if app.overlay_suppresses_host_images() {
        return;
    }
    if app.pane_text_layers.is_empty() {
        return;
    }
    for info in &app.view.pane_infos {
        let Some(layers) = app.pane_text_layers.get(&info.id) else {
            continue;
        };
        for layer in layers {
            paint_layer(layer, info.inner_rect, buf);
        }
    }
}

fn paint_layer(layer: &PaneTextLayer, inner: Rect, buf: &mut Buffer) {
    let Some(area) = layer_area_for_text(layer, inner) else {
        return;
    };
    let buf_area = buf.area;
    for (row_index, line) in layer.lines.iter().enumerate() {
        let Ok(row_offset) = u16::try_from(row_index) else {
            break;
        };
        if row_offset >= area.height {
            // Lines past the box are dropped, not wrapped. Wrapping would let a
            // layer paint over cells the caller never asked for, and a plugin
            // that wants wrapping can compute it — it knows its own box.
            break;
        }
        let y = area.y + row_offset;
        if y >= buf_area.bottom() {
            break;
        }
        let mut x = area.x;
        for span in &line.spans {
            let fg = named_color(&span.fg);
            let bg = named_color(&span.bg);
            for ch in span.text.chars() {
                let width = u16::try_from(ch.width().unwrap_or(0)).unwrap_or(0);
                if width == 0 {
                    // Zero-width characters (combining marks, and anything that
                    // slipped past the control-character strip) would advance
                    // nothing and overwrite the previous cell. Skipping keeps
                    // the caller's column arithmetic honest.
                    continue;
                }
                if x >= area.right() || x >= buf_area.right() {
                    break;
                }
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char(ch);
                    // An unset colour leaves the cell's own, which is what makes
                    // a span composite over pane text instead of blanking it.
                    if let Some(fg) = fg {
                        cell.set_fg(fg);
                    }
                    if let Some(bg) = bg {
                        cell.set_bg(bg);
                    }
                    if span.bold {
                        cell.modifier.insert(Modifier::BOLD);
                    }
                }
                // A wide glyph occupies two cells; blank the second so the
                // pane's original character does not show through its right
                // half as a stray fragment.
                if width == 2 {
                    if let Some(next) = x.checked_add(1) {
                        if next < area.right() && next < buf_area.right() {
                            if let Some(cell) = buf.cell_mut((next, y)) {
                                cell.set_char(' ');
                                if let Some(bg) = bg {
                                    cell.set_bg(bg);
                                }
                            }
                        }
                    }
                }
                x = x.saturating_add(width);
            }
        }
    }
}

/// The layer's cell box, reusing the graphics geometry so identical placements
/// land identically. Text layers borrow `PaneGraphicsLayer`'s rules through a
/// shared helper rather than a second implementation — two implementations of
/// the same placement contract is how a plugin ends up painting in different
/// places on different surfaces.
fn layer_area_for_text(layer: &PaneTextLayer, inner: Rect) -> Option<Rect> {
    layer_area(&layer.render, inner)
}

/// Map a named colour to ratatui's ANSI palette.
///
/// Named only, never `Rgb`. Herdr remaps buffer colours to the active theme via
/// `recolor_to_palette`, and truecolor passes through that remap untouched — so
/// an overlay allowed to pick a hex value would be the one thing on screen that
/// ignores the user's theme, and would do it without any error. An unrecognised
/// name yields `None`, meaning "inherit", so a typo costs the styling and not
/// the text.
fn named_color(name: &str) -> Option<Color> {
    match name.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "dark_gray" | "dark_grey" | "bright_black" => Some(Color::DarkGray),
        "bright_red" => Some(Color::LightRed),
        "bright_green" => Some(Color::LightGreen),
        "bright_yellow" => Some(Color::LightYellow),
        "bright_blue" => Some(Color::LightBlue),
        "bright_magenta" => Some(Color::LightMagenta),
        "bright_cyan" => Some(Color::LightCyan),
        "white" | "bright_white" => Some(Color::White),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{PaneGraphicsPlacementParams, PaneTextLineParams, PaneTextSpanParams};

    const INNER: Rect = Rect {
        x: 2,
        y: 1,
        width: 20,
        height: 5,
    };

    fn span(text: &str, fg: &str, bg: &str) -> PaneTextSpanParams {
        PaneTextSpanParams {
            text: text.to_string(),
            fg: fg.to_string(),
            bg: bg.to_string(),
            bold: false,
        }
    }

    fn layer(lines: Vec<PaneTextLineParams>, render: PaneGraphicsPlacementParams) -> PaneTextLayer {
        PaneTextLayer {
            lines,
            render,
            owner: Some("plugin:test".into()),
            layer_id: String::new(),
        }
    }

    fn placement(col: i32, row: i32, cols: u32, rows: u32) -> PaneGraphicsPlacementParams {
        PaneGraphicsPlacementParams {
            viewport_col: col,
            viewport_row: row,
            grid_cols: Some(cols),
            grid_rows: Some(rows),
        }
    }

    fn row_text(buf: &Buffer, y: u16, from: u16, len: u16) -> String {
        (from..from + len)
            .map(|x| buf.cell((x, y)).expect("cell").symbol().to_string())
            .collect()
    }

    #[test]
    fn a_span_paints_at_the_placement_origin_inside_the_pane() {
        let l = layer(
            vec![PaneTextLineParams {
                spans: vec![span("hi", "", "")],
            }],
            placement(3, 2, 10, 2),
        );
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        paint_layer(&l, INNER, &mut buf);
        assert_eq!(row_text(&buf, 3, 5, 2), "hi");
    }

    /// The invariant that makes this an overlay: cells the layer does not cover
    /// keep whatever was under them.
    #[test]
    fn cells_outside_the_span_are_left_untouched() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        for x in 0..40 {
            buf.cell_mut((x, 3)).expect("cell").set_char('#');
        }
        let l = layer(
            vec![PaneTextLineParams {
                spans: vec![span("hi", "", "")],
            }],
            placement(3, 2, 10, 2),
        );
        paint_layer(&l, INNER, &mut buf);
        assert_eq!(row_text(&buf, 3, 3, 6), "##hi##");
    }

    #[test]
    fn named_colours_are_applied_and_an_unset_colour_inherits() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        buf.cell_mut((2, 1)).expect("cell").set_bg(Color::Blue);
        let l = layer(
            vec![PaneTextLineParams {
                spans: vec![span("x", "red", "")],
            }],
            placement(0, 0, 4, 1),
        );
        paint_layer(&l, INNER, &mut buf);
        let cell = buf.cell((2, 1)).expect("cell");
        assert_eq!(cell.fg, Color::Red);
        assert_eq!(cell.bg, Color::Blue, "unset bg must inherit the cell's own");
    }

    /// A theme-breaking colour must never reach the buffer, and the failure mode
    /// has to be "unstyled", not "refused" or "black".
    #[test]
    fn an_unknown_colour_name_loses_the_styling_but_keeps_the_text() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        buf.cell_mut((2, 1)).expect("cell").set_fg(Color::Green);
        let l = layer(
            vec![PaneTextLineParams {
                spans: vec![span("x", "#ff00ff", "")],
            }],
            placement(0, 0, 4, 1),
        );
        paint_layer(&l, INNER, &mut buf);
        let cell = buf.cell((2, 1)).expect("cell");
        assert_eq!(cell.symbol(), "x");
        assert_eq!(cell.fg, Color::Green);
    }

    #[test]
    fn no_named_colour_maps_to_a_truecolor_value() {
        for name in [
            "black",
            "red",
            "green",
            "yellow",
            "blue",
            "magenta",
            "cyan",
            "gray",
            "dark_gray",
            "bright_red",
            "bright_green",
            "bright_yellow",
            "bright_blue",
            "bright_magenta",
            "bright_cyan",
            "white",
        ] {
            let color = named_color(name).expect("known colour");
            assert!(
                !matches!(color, Color::Rgb(..)),
                "{name} must not map to Rgb: truecolor bypasses recolor_to_palette"
            );
        }
    }

    #[test]
    fn text_is_clipped_to_the_box_rather_than_spilling_across_the_pane() {
        let l = layer(
            vec![PaneTextLineParams {
                spans: vec![span("abcdefgh", "", "")],
            }],
            placement(0, 0, 3, 1),
        );
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        paint_layer(&l, INNER, &mut buf);
        assert_eq!(row_text(&buf, 1, 2, 5), "abc  ");
    }

    #[test]
    fn lines_past_the_box_height_are_dropped_not_wrapped() {
        let l = layer(
            vec![
                PaneTextLineParams {
                    spans: vec![span("one", "", "")],
                },
                PaneTextLineParams {
                    spans: vec![span("two", "", "")],
                },
            ],
            placement(0, 0, 10, 1),
        );
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        paint_layer(&l, INNER, &mut buf);
        assert_eq!(row_text(&buf, 1, 2, 3), "one");
        assert_eq!(row_text(&buf, 2, 2, 3), "   ");
    }

    #[test]
    fn consecutive_spans_continue_on_the_same_row() {
        let l = layer(
            vec![PaneTextLineParams {
                spans: vec![span("ab", "red", ""), span("cd", "green", "")],
            }],
            placement(0, 0, 10, 1),
        );
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        paint_layer(&l, INNER, &mut buf);
        assert_eq!(row_text(&buf, 1, 2, 4), "abcd");
        assert_eq!(buf.cell((2, 1)).expect("cell").fg, Color::Red);
        assert_eq!(buf.cell((4, 1)).expect("cell").fg, Color::Green);
    }

    /// A wide glyph takes two cells. If the second is not blanked, the pane's
    /// original character shows through the right half as a stray fragment.
    #[test]
    fn a_wide_glyph_claims_both_of_its_cells() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        buf.cell_mut((3, 1)).expect("cell").set_char('X');
        let l = layer(
            vec![PaneTextLineParams {
                spans: vec![span("漢", "", "")],
            }],
            placement(0, 0, 10, 1),
        );
        paint_layer(&l, INNER, &mut buf);
        assert_eq!(buf.cell((2, 1)).expect("cell").symbol(), "漢");
        assert_eq!(buf.cell((3, 1)).expect("cell").symbol(), " ");
    }

    #[test]
    fn a_layer_fully_outside_the_pane_paints_nothing() {
        let l = layer(
            vec![PaneTextLineParams {
                spans: vec![span("hi", "", "")],
            }],
            placement(40, 0, 4, 1),
        );
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        paint_layer(&l, INNER, &mut buf);
        assert_eq!(row_text(&buf, 1, 2, 4), "    ");
    }
}
