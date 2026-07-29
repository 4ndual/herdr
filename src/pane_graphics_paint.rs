//! fork(+127): half-block compositor for plugin-owned `pane.graphics` layers.
//!
//! ## Why this file exists
//!
//! Before +127 a retained `pane.graphics.set` layer had exactly one way to reach
//! the screen: `kitty_graphics::paint_local_pane_graphics`, called from the
//! `kitty_graphics_enabled` arm of the render loop (`app/mod.rs`). Turning the
//! kitty renderer off did not make a plugin's layer render differently — it made
//! it not render at all. Combined with `require_pane_graphics_enabled` refusing
//! the API outright while kitty was off, a plugin's off-switch was meaningless
//! unless an unrelated experimental renderer flag happened to be on.
//!
//! This module is the missing half: the same layer list, the same geometry,
//! rasterised into the ratatui buffer as `▀` cells. After it, `pane.graphics`
//! is renderer-independent, and `experimental.kitty_graphics` goes back to its
//! one honest job — picking the emission backend.
//!
//! ## Relationship to `host_image.rs`
//!
//! None, deliberately. `host_image` is the `~/.claude/image-cache` reader gated
//! behind `host-images` (default off). This module is compiled unconditionally
//! and reads no fixed path: every byte it paints was handed to the host by a
//! `pane.graphics.set` call that already passed capability and `graphics_roots`
//! checks. That separation is what lets a `host-images`-off build still show
//! images, provided a plugin supplies them.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::app::state::{AppState, PaneGraphicsLayer};
use crate::image_decode::{decode_rgba, halfblock_cells, RgbaImage, HALF_BLOCK};

/// Paint every retained pane-graphics layer of the active tab into `buf`.
///
/// Call site is the non-kitty arm of the render loop, after `render()` has drawn
/// pane content — layers composite ON TOP of text, which is what makes them
/// overlays rather than replacements. The terminal buffer's text is overwritten
/// only in the cells the layer covers, and only for this frame; nothing in
/// `PaneState`/scrollback is touched, so search, copy and agent-resume keep
/// reading the real output. (That property is why +127 ships overlay and defers
/// text mutation to 128 — see `.local/prd/127-plugin-owned-images.md`.)
pub(crate) fn paint_pane_graphics_halfblock(app: &AppState, buf: &mut Buffer) {
    // Same suppression rule the kitty path uses: a layer is painted after the
    // frame, so while a modal is open it would draw straight over the dialog.
    if app.overlay_suppresses_host_images() {
        return;
    }
    if app.pane_graphics_layers.is_empty() {
        return;
    }
    for info in &app.view.pane_infos {
        let Some(layers) = app.pane_graphics_layers.get(&info.id) else {
            continue;
        };
        // Already in composite order (see `pane_graphics_layers`); painting in
        // vec order means later layers win the same cells, matching the kitty
        // path's bottom-to-top upload order.
        for layer in layers {
            paint_layer(layer, info.inner_rect, buf);
        }
    }
}

/// Resolve one layer's cell box inside `inner` and rasterise it.
///
/// Geometry is deliberately identical to `kitty_graphics::pane_graphics_host_placement`:
/// origin `inner + (viewport_col, viewport_row)`, size `grid_cols × grid_rows`
/// cells, with `0` meaning "the whole pane". If the two ever disagree, the same
/// plugin would paint in different places on different terminals, which is
/// exactly the class of bug a plugin author cannot debug from outside.
fn paint_layer(layer: &PaneGraphicsLayer, inner: Rect, buf: &mut Buffer) {
    let Some(area) = layer_area(&layer.render, inner) else {
        return;
    };
    let Some(img) = layer_rgba(layer) else {
        // A layer whose bytes will not decode is a plugin bug, and a silent
        // return here is exactly the failure mode that cost hours: the plugin
        // looks enabled, `plugin list` agrees, and nothing appears. Log it with
        // the owner so `plugin.ui.status` has something to attribute.
        tracing::debug!(
            owner = layer.owner.as_deref().unwrap_or("<socket>"),
            format = ?layer.format,
            image_width = layer.image_width,
            image_height = layer.image_height,
            data_len = layer.data.len(),
            "pane graphics layer could not be decoded for half-block paint"
        );
        return;
    };

    let cells = halfblock_cells(&img, area.width, area.height);
    let buf_area = buf.area;
    for (index, cell) in cells.iter().enumerate() {
        let cx = area.x + (index as u16 % area.width);
        let cy = area.y + (index as u16 / area.width);
        if cx >= buf_area.right() || cy >= buf_area.bottom() {
            continue;
        }
        if let Some(target) = buf.cell_mut((cx, cy)) {
            target.set_char(HALF_BLOCK);
            target.set_fg(ratatui::style::Color::Rgb(
                cell.top.0, cell.top.1, cell.top.2,
            ));
            target.set_bg(ratatui::style::Color::Rgb(
                cell.bottom.0,
                cell.bottom.1,
                cell.bottom.2,
            ));
        }
    }
}

/// The on-screen cell rect for `layer`, clipped to `inner`. `None` when the
/// layer is fully outside the pane or would occupy no cells.
///
/// Negative `viewport_col`/`viewport_row` are legal (kitty allows a placement to
/// start off the left/top edge); the visible remainder is what gets painted.
///
/// fork(+128): takes the PLACEMENT rather than the layer, so the text painter
/// resolves its box through this exact function instead of a second copy of the
/// rules. Two implementations of one placement contract is how a plugin ends up
/// painting an image and its caption in different places.
pub(crate) fn layer_area(
    render: &crate::api::schema::PaneGraphicsPlacementParams,
    inner: Rect,
) -> Option<Rect> {
    if inner.width == 0 || inner.height == 0 {
        return None;
    }
    // An OMITTED extent means "the whole pane in that axis". A PRESENT zero is
    // rejected at the API boundary (`PaneGraphicsPlacement::validate`) rather
    // than reaching here, because the two used to be indistinguishable: every
    // field defaulted to 0 and 0 meant whole-pane, so one misspelled key turned
    // into a full-pane opaque overlay that reported success.
    let cols = render.grid_cols.unwrap_or_else(|| u32::from(inner.width));
    let rows = render.grid_rows.unwrap_or_else(|| u32::from(inner.height));
    if cols == 0 || rows == 0 {
        return None;
    }

    // Clip a possibly-negative origin: shrink the box by the hidden part rather
    // than shifting it, so the image keeps its scale.
    let (x_off, cols) = clip_start(render.viewport_col, cols)?;
    let (y_off, rows) = clip_start(render.viewport_row, rows)?;

    let x = inner.x.checked_add(u16::try_from(x_off).ok()?)?;
    let y = inner.y.checked_add(u16::try_from(y_off).ok()?)?;
    if x >= inner.right() || y >= inner.bottom() {
        return None;
    }
    let width = u16::try_from(cols)
        .unwrap_or(u16::MAX)
        .min(inner.right() - x);
    let height = u16::try_from(rows)
        .unwrap_or(u16::MAX)
        .min(inner.bottom() - y);
    if width == 0 || height == 0 {
        return None;
    }
    Some(Rect {
        x,
        y,
        width,
        height,
    })
}

/// Clip one axis: returns `(offset_within_inner, visible_extent)`.
fn clip_start(start: i32, extent: u32) -> Option<(u32, u32)> {
    if start >= 0 {
        return Some((u32::try_from(start).ok()?, extent));
    }
    let hidden = u32::try_from(start.unsigned_abs()).ok()?;
    if hidden >= extent {
        return None;
    }
    Some((0, extent - hidden))
}

/// Decode a layer's bytes into RGBA. PNG goes through the shared decoder; the
/// raw formats are already pixels and only need an alpha channel added.
///
/// Raw layers are validated at the API boundary (`pane_graphics_expected_data_len`)
/// so the length check here is a belt-and-braces guard against a layer that
/// entered state through some other path, not the primary validation.
fn layer_rgba(layer: &PaneGraphicsLayer) -> Option<RgbaImage> {
    use crate::api::schema::PaneGraphicsFormat;
    let (w, h) = (layer.image_width, layer.image_height);
    match layer.format {
        PaneGraphicsFormat::Png => decode_rgba(&layer.data),
        PaneGraphicsFormat::Rgba => {
            let expected = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
            if layer.data.len() < expected {
                return None;
            }
            Some(RgbaImage {
                width: w,
                height: h,
                data: layer.data[..expected].to_vec(),
            })
        }
        PaneGraphicsFormat::Rgb => {
            let expected = (w as usize).checked_mul(h as usize)?.checked_mul(3)?;
            if layer.data.len() < expected {
                return None;
            }
            let mut data = Vec::with_capacity(expected / 3 * 4);
            for px in layer.data[..expected].chunks_exact(3) {
                data.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            Some(RgbaImage {
                width: w,
                height: h,
                data,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{PaneGraphicsFormat, PaneGraphicsPlacementParams};

    fn layer(
        format: PaneGraphicsFormat,
        w: u32,
        h: u32,
        data: Vec<u8>,
        render: PaneGraphicsPlacementParams,
    ) -> PaneGraphicsLayer {
        PaneGraphicsLayer::new(format, w, h, data, render, Some("plugin:test".into()))
    }

    fn placement(col: i32, row: i32, cols: u32, rows: u32) -> PaneGraphicsPlacementParams {
        PaneGraphicsPlacementParams {
            viewport_col: col,
            viewport_row: row,
            grid_cols: Some(cols),
            grid_rows: Some(rows),
        }
    }

    const INNER: Rect = Rect {
        x: 10,
        y: 5,
        width: 40,
        height: 20,
    };

    /// Replaces `zero_grid_dims_mean_the_whole_pane`, which pinned the sentinel
    /// this change exists to remove. OMITTED extents are now the way to ask for
    /// the whole pane; a PRESENT zero is a caller error, not a synonym for
    /// "everything" (see the pair below).
    #[test]
    fn omitted_grid_dims_mean_the_whole_pane() {
        let l = layer(
            PaneGraphicsFormat::Rgb,
            1,
            1,
            vec![0; 3],
            PaneGraphicsPlacementParams::whole_pane(),
        );
        assert_eq!(layer_area(&l.render, INNER), Some(INNER));
    }

    /// The other half of that boundary, stated explicitly rather than left to
    /// the `cols == 0` guard's shape. A present zero must NOT resolve to the
    /// whole pane: that equivalence is what turned one misspelled key into a
    /// full-pane opaque overlay that reported success.
    #[test]
    fn a_present_zero_extent_paints_nothing_rather_than_everything() {
        let l = layer(
            PaneGraphicsFormat::Rgb,
            1,
            1,
            vec![0; 3],
            PaneGraphicsPlacementParams {
                grid_cols: Some(0),
                grid_rows: Some(0),
                ..Default::default()
            },
        );
        assert_eq!(layer_area(&l.render, INNER), None);
        assert_ne!(layer_area(&l.render, INNER), Some(INNER));
    }

    #[test]
    fn explicit_box_is_offset_from_the_pane_origin() {
        let l = layer(
            PaneGraphicsFormat::Rgb,
            1,
            1,
            vec![0; 3],
            placement(3, 2, 8, 4),
        );
        assert_eq!(
            layer_area(&l.render, INNER),
            Some(Rect {
                x: 13,
                y: 7,
                width: 8,
                height: 4
            })
        );
    }

    #[test]
    fn box_is_clipped_to_the_pane_rather_than_overflowing() {
        let l = layer(
            PaneGraphicsFormat::Rgb,
            1,
            1,
            vec![0; 3],
            placement(35, 18, 20, 20),
        );
        assert_eq!(
            layer_area(&l.render, INNER),
            Some(Rect {
                x: 45,
                y: 23,
                width: 5,
                height: 2
            })
        );
    }

    #[test]
    fn negative_origin_shrinks_instead_of_shifting() {
        // Two columns hidden off the left edge: the box starts at the pane edge
        // and is two cells narrower, so the image keeps its on-screen scale.
        let l = layer(
            PaneGraphicsFormat::Rgb,
            1,
            1,
            vec![0; 3],
            placement(-2, -1, 8, 4),
        );
        assert_eq!(
            layer_area(&l.render, INNER),
            Some(Rect {
                x: 10,
                y: 5,
                width: 6,
                height: 3
            })
        );
    }

    #[test]
    fn fully_offscreen_layer_paints_nothing() {
        let l = layer(
            PaneGraphicsFormat::Rgb,
            1,
            1,
            vec![0; 3],
            placement(-8, 0, 8, 4),
        );
        assert_eq!(layer_area(&l.render, INNER), None);
        let l = layer(
            PaneGraphicsFormat::Rgb,
            1,
            1,
            vec![0; 3],
            placement(40, 0, 8, 4),
        );
        assert_eq!(layer_area(&l.render, INNER), None);
    }

    #[test]
    fn empty_pane_rect_paints_nothing() {
        let l = layer(
            PaneGraphicsFormat::Rgb,
            1,
            1,
            vec![0; 3],
            placement(0, 0, 0, 0),
        );
        assert_eq!(layer_area(&l.render, Rect::new(0, 0, 0, 0)), None);
    }

    #[test]
    fn rgb_layer_gains_an_opaque_alpha_channel() {
        let l = layer(
            PaneGraphicsFormat::Rgb,
            2,
            1,
            vec![1, 2, 3, 4, 5, 6],
            placement(0, 0, 0, 0),
        );
        let img = layer_rgba(&l).expect("rgb decodes");
        assert_eq!(img.data, vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn short_raw_layer_is_refused_rather_than_read_out_of_bounds() {
        let l = layer(
            PaneGraphicsFormat::Rgba,
            4,
            4,
            vec![0; 8],
            placement(0, 0, 0, 0),
        );
        assert!(layer_rgba(&l).is_none());
    }

    #[test]
    fn painting_writes_half_block_cells_into_the_buffer() {
        // 1x2 image, top red bottom green, into a 1x1 cell box.
        let data = vec![255, 0, 0, 0, 255, 0];
        let l = layer(PaneGraphicsFormat::Rgb, 1, 2, data, placement(0, 0, 1, 1));
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 4));
        paint_layer(&l, Rect::new(1, 1, 2, 2), &mut buf);
        let cell = buf.cell((1, 1)).expect("cell exists");
        assert_eq!(cell.symbol(), HALF_BLOCK.to_string());
        assert_eq!(cell.fg, ratatui::style::Color::Rgb(255, 0, 0));
        assert_eq!(cell.bg, ratatui::style::Color::Rgb(0, 255, 0));
        // Outside the 1x1 box nothing was touched.
        assert_eq!(buf.cell((2, 1)).expect("cell exists").symbol(), " ");
    }

    #[test]
    fn undecodable_layer_paints_nothing_but_leaves_the_buffer_intact() {
        let l = layer(
            PaneGraphicsFormat::Png,
            2,
            2,
            b"not a png".to_vec(),
            placement(0, 0, 0, 0),
        );
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 4));
        paint_layer(&l, Rect::new(0, 0, 4, 4), &mut buf);
        assert_eq!(buf.cell((0, 0)).expect("cell exists").symbol(), " ");
    }
}
