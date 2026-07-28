//! Host-side image loading and placeholder detection shared by the kitty
//! graphics re-emit path and the half-block fallback renderer.
//!
//! Two consumers use this module:
//!   * plugin-panel images — a manifest-declared file painted over a plugin
//!     surface `Rect`;
//!   * Claude Code `[Image #N]` placeholders — herdr owns the pty and knows a
//!     pane's Claude session id, so it resolves
//!     `~/.claude/image-cache/<session>/<N>.png` and paints it itself.
//!
//! Nothing here modifies the Claude binary or its API; the image bytes already
//! exist on disk. All decoding uses the `png` crate already vendored for the
//! ghostty kitty store, so no new dependency is introduced.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use crate::ghostty::KittyImageFormat;

/// A `[Image #N]` placeholder located in a pane's visible grid, with the
/// on-screen cell offset (relative to the pane's inner area) of its first
/// character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImagePlaceholder {
    /// The `N` in `[Image #N]`, 1-indexed exactly as Claude Code numbers it and
    /// as the cache files are named.
    pub number: u32,
    /// Zero-based column of the `[` within the scanned rows.
    pub col: u16,
    /// Zero-based row of the placeholder within the scanned rows.
    pub row: u16,
}

/// Image bytes ready for the kitty transmit path. For PNG the raw file bytes
/// are forwarded verbatim (kitty `f=100`), matching what pi emits; the pixel
/// dimensions come from the IHDR header without a full decode.
#[derive(Debug, Clone)]
pub(crate) struct KittyImageBytes {
    pub data: Vec<u8>,
    pub format: KittyImageFormat,
    pub width_px: u32,
    pub height_px: u32,
    pub fingerprint: u64,
}

/// Decoded RGBA pixels for the half-block fallback renderer.
#[derive(Debug, Clone)]
pub(crate) struct RgbaImage {
    pub width: u32,
    pub height: u32,
    /// Row-major RGBA, 4 bytes per pixel.
    pub data: Vec<u8>,
}

/// Parse `[Image #N]` occurrences out of visible pane rows.
///
/// Each row is the plain text of one visible line; `col` is the byte/character
/// column of the `[`. Claude renders the placeholder as literal ASCII, so a
/// character-index scan is exact for it. Multiple placeholders per screen are
/// supported (a compacted/resumed pane can show several).
pub(crate) fn parse_image_placeholders(rows: &[String]) -> Vec<ImagePlaceholder> {
    let mut out = Vec::new();
    for (row_idx, row) in rows.iter().enumerate() {
        let chars: Vec<char> = row.chars().collect();
        let mut i = 0usize;
        while i < chars.len() {
            if let Some((number, consumed)) = match_placeholder(&chars[i..]) {
                out.push(ImagePlaceholder {
                    number,
                    col: i as u16,
                    row: row_idx as u16,
                });
                i += consumed;
            } else {
                i += 1;
            }
        }
    }
    out
}

/// Match `[Image #<digits>]` at the start of `chars`, returning the number and
/// the count of characters consumed.
fn match_placeholder(chars: &[char]) -> Option<(u32, usize)> {
    const PREFIX: &[char] = &['[', 'I', 'm', 'a', 'g', 'e', ' ', '#'];
    if chars.len() < PREFIX.len() + 2 {
        return None;
    }
    if &chars[..PREFIX.len()] != PREFIX {
        return None;
    }
    let mut idx = PREFIX.len();
    let mut digits = String::new();
    while idx < chars.len() && chars[idx].is_ascii_digit() {
        digits.push(chars[idx]);
        idx += 1;
    }
    if digits.is_empty() {
        return None;
    }
    if idx >= chars.len() || chars[idx] != ']' {
        return None;
    }
    idx += 1;
    let number = digits.parse::<u32>().ok()?;
    Some((number, idx))
}

/// Resolve the on-disk cache file for a Claude `[Image #N]` placeholder.
///
/// The layout is the observed, undocumented convention
/// `~/.claude/image-cache/<session>/<N>.<ext>`. It is *probed*, never assumed:
/// a preferred `.png` is checked first (the kitty transmit path needs PNG),
/// then a small set of known extensions. Returns `None` if nothing is present
/// so the caller can leave the `[Image #N]` text untouched.
pub(crate) fn resolve_image_cache_file(
    cache_root: &Path,
    session_id: &str,
    number: u32,
) -> Option<PathBuf> {
    if session_id.is_empty() {
        return None;
    }
    let dir = cache_root.join(session_id);
    // PNG first: the only format the kitty transmit path can forward directly.
    for ext in ["png", "jpeg", "jpg"] {
        let candidate = dir.join(format!("{number}.{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Scan visible pane `rows` for `[Image #N]` placeholders and resolve each to
/// its on-disk cache file, dropping any that have no file. Shared by the kitty
/// re-emit path and the half-block fallback so both agree on which placeholders
/// render.
pub(crate) fn resolve_pane_placeholders(
    cache_root: &Path,
    session_id: &str,
    rows: &[String],
) -> Vec<(ImagePlaceholder, PathBuf)> {
    parse_image_placeholders(rows)
        .into_iter()
        .filter_map(|placeholder| {
            resolve_image_cache_file(cache_root, session_id, placeholder.number)
                .map(|path| (placeholder, path))
        })
        .collect()
}

/// The default Claude image-cache root, `~/.claude/image-cache`. cx / CCR
/// sessions share the same directory (CCR routes the API, not the config dir),
/// so no per-profile branching is needed.
pub(crate) fn default_image_cache_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".claude").join("image-cache"))
}

/// Read PNG width/height from the IHDR chunk without decoding pixel data.
fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if bytes.len() < 24 || &bytes[..8] != SIG {
        return None;
    }
    // IHDR is the first chunk: length(4) + "IHDR"(4) then width(4) height(4).
    if &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

fn fingerprint_bytes(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.len().hash(&mut hasher);
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Load a PNG file as kitty transmit bytes (raw file bytes, `f=100`). Non-PNG
/// files return `None` — the kitty protocol only forwards PNG or raw RGB(A),
/// and every cache/demo path we drive is PNG.
pub(crate) fn load_kitty_image_bytes(bytes: Vec<u8>) -> Option<KittyImageBytes> {
    let (width_px, height_px) = png_dimensions(&bytes)?;
    let fingerprint = fingerprint_bytes(&bytes);
    Some(KittyImageBytes {
        data: bytes,
        format: KittyImageFormat::Png,
        width_px,
        height_px,
        fingerprint,
    })
}

/// Decode a PNG file to RGBA for the half-block renderer.
pub(crate) fn decode_rgba(bytes: &[u8]) -> Option<RgbaImage> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let frame = &buf[..info.buffer_size()];
    if info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let data = match info.color_type {
        png::ColorType::Rgba => frame.to_vec(),
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity((info.width as usize) * (info.height as usize) * 4);
            for rgb in frame.chunks_exact(3) {
                out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity((info.width as usize) * (info.height as usize) * 4);
            for gray in frame {
                out.extend_from_slice(&[*gray, *gray, *gray, 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity((info.width as usize) * (info.height as usize) * 4);
            for ga in frame.chunks_exact(2) {
                out.extend_from_slice(&[ga[0], ga[0], ga[0], ga[1]]);
            }
            out
        }
        png::ColorType::Indexed => return None,
    };
    Some(RgbaImage {
        width: info.width,
        height: info.height,
        data,
    })
}

/// One terminal cell of a half-block mosaic: the upper half is drawn with a
/// `▀` glyph in `top`'s color over a `bottom`-colored background, so each cell
/// carries two vertically-stacked pixels (doubling vertical resolution).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HalfblockCell {
    pub top: (u8, u8, u8),
    pub bottom: (u8, u8, u8),
}

/// The Unicode upper-half-block glyph used by the fallback renderer.
pub(crate) const HALF_BLOCK: char = '▀';

/// Downsample `img` into a `cols × rows` grid of half-block cells, preserving
/// nearest-neighbour sampling. Each cell samples two sub-rows (top/bottom) so
/// the effective vertical resolution is `rows * 2`. Alpha is composited over
/// black. This is the terminal-agnostic fallback used when the outer terminal
/// cannot render kitty graphics (non-Ghostty, or tmux).
pub(crate) fn halfblock_cells(img: &RgbaImage, cols: u16, rows: u16) -> Vec<HalfblockCell> {
    let mut out = Vec::new();
    if cols == 0 || rows == 0 || img.width == 0 || img.height == 0 {
        return out;
    }
    let sub_rows = rows as u32 * 2;
    let sample = |ix: u32, iy: u32| -> (u8, u8, u8) {
        let x = ix.min(img.width - 1);
        let y = iy.min(img.height - 1);
        let idx = ((y * img.width + x) * 4) as usize;
        let r = img.data[idx];
        let g = img.data[idx + 1];
        let b = img.data[idx + 2];
        let a = img.data[idx + 3] as u32;
        // Composite over black so transparent regions read as background.
        (
            (r as u32 * a / 255) as u8,
            (g as u32 * a / 255) as u8,
            (b as u32 * a / 255) as u8,
        )
    };
    for cy in 0..rows as u32 {
        for cx in 0..cols as u32 {
            let ix = cx * img.width / cols as u32;
            let top_iy = (cy * 2) * img.height / sub_rows;
            let bottom_iy = (cy * 2 + 1) * img.height / sub_rows;
            out.push(HalfblockCell {
                top: sample(ix, top_iy),
                bottom: sample(ix, bottom_iy),
            });
        }
    }
    out
}

/// Fit an image of `img_width` x `img_height` pixels into at most
/// `max_cols` x `max_rows` half-block cells, preserving aspect ratio.
///
/// A half-block cell is one character wide and stacks two vertical pixels, so a
/// `cols` x `rows` grid samples the image at `cols` x `rows*2` pixels. Assuming
/// the two stacked sub-pixels are roughly square on screen (the usual ~1:2
/// character cell), an undistorted image needs `cols : rows*2 == width : height`,
/// i.e. `rows = cols * height / (2 * width)`. We first fit to the full width
/// budget, then fall back to the height budget when that would overflow.
/// Returns `(0, 0)` when any input is zero.
pub(crate) fn fit_halfblock_box(
    img_width: u32,
    img_height: u32,
    max_cols: u16,
    max_rows: u16,
) -> (u16, u16) {
    if img_width == 0 || img_height == 0 || max_cols == 0 || max_rows == 0 {
        return (0, 0);
    }
    let w = img_width as u64;
    let h = img_height as u64;

    // Width-constrained: use the whole width budget, derive rows from aspect.
    let cols = max_cols as u64;
    let rows = ((cols * h + w) / (2 * w)).max(1); // round-ish, never below 1
    if rows <= max_rows as u64 {
        return (cols as u16, rows as u16);
    }

    // Height-constrained: use the whole height budget, derive cols from aspect.
    let rows = max_rows as u64;
    let cols = ((rows * 2 * w + h / 2) / h).clamp(1, max_cols as u64);
    (cols as u16, rows as u16)
}

/// The half-block cell box (`cols`, `rows`) for the image at `path`, fitted to
/// at most `max_cols` x `max_rows` while preserving aspect ratio. `None` when
/// the image can't be loaded. Reuses the mtime-keyed decode cache, so repeated
/// calls across frames don't re-decode.
pub(crate) fn halfblock_image_box(
    path: &Path,
    max_cols: u16,
    max_rows: u16,
) -> Option<(u16, u16)> {
    let img = cached_rgba_image(path)?;
    Some(fit_halfblock_box(img.width, img.height, max_cols, max_rows))
}

/// Paint a half-block mosaic of `path` into `area` of the ratatui buffer. Used
/// as the host image fallback when kitty graphics are unavailable. Silently
/// does nothing if the file is missing/undecodable so the surface just shows
/// its normal content.
pub(crate) fn paint_halfblock_image(
    path: &Path,
    area: ratatui::layout::Rect,
    buf: &mut ratatui::buffer::Buffer,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(img) = cached_rgba_image(path) else {
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
            target.set_fg(ratatui::style::Color::Rgb(cell.top.0, cell.top.1, cell.top.2));
            target.set_bg(ratatui::style::Color::Rgb(
                cell.bottom.0,
                cell.bottom.1,
                cell.bottom.2,
            ));
        }
    }
}

/// Process-wide cache of loaded images keyed by path + mtime, so the render
/// loop does not re-read and re-decode files every frame.
#[derive(Default)]
struct LoaderCache {
    kitty: std::collections::HashMap<PathBuf, (SystemTime, KittyImageBytes)>,
    rgba: std::collections::HashMap<PathBuf, (SystemTime, RgbaImage)>,
}

static LOADER: OnceLock<Mutex<LoaderCache>> = OnceLock::new();

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Load kitty transmit bytes for `path`, memoized by mtime. Returns `None` for
/// a missing/unreadable/non-PNG file (caller degrades gracefully).
pub(crate) fn cached_kitty_image(path: &Path) -> Option<KittyImageBytes> {
    let mtime = file_mtime(path)?;
    let cache = LOADER.get_or_init(|| Mutex::new(LoaderCache::default()));
    if let Ok(mut guard) = cache.lock() {
        if let Some((cached_mtime, image)) = guard.kitty.get(path) {
            if *cached_mtime == mtime {
                return Some(image.clone());
            }
        }
        let bytes = std::fs::read(path).ok()?;
        let image = load_kitty_image_bytes(bytes)?;
        guard
            .kitty
            .insert(path.to_path_buf(), (mtime, image.clone()));
        return Some(image);
    }
    // Lock poisoned: fall back to an uncached read rather than failing render.
    let bytes = std::fs::read(path).ok()?;
    load_kitty_image_bytes(bytes)
}

/// Load decoded RGBA for `path`, memoized by mtime.
pub(crate) fn cached_rgba_image(path: &Path) -> Option<RgbaImage> {
    let mtime = file_mtime(path)?;
    let cache = LOADER.get_or_init(|| Mutex::new(LoaderCache::default()));
    if let Ok(mut guard) = cache.lock() {
        if let Some((cached_mtime, image)) = guard.rgba.get(path) {
            if *cached_mtime == mtime {
                return Some(image.clone());
            }
        }
        let bytes = std::fs::read(path).ok()?;
        let image = decode_rgba(&bytes)?;
        guard.rgba.insert(path.to_path_buf(), (mtime, image.clone()));
        return Some(image);
    }
    let bytes = std::fs::read(path).ok()?;
    decode_rgba(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_halfblock_box_zero_inputs_yield_zero() {
        assert_eq!(fit_halfblock_box(0, 10, 40, 12), (0, 0));
        assert_eq!(fit_halfblock_box(10, 0, 40, 12), (0, 0));
        assert_eq!(fit_halfblock_box(10, 10, 0, 12), (0, 0));
        assert_eq!(fit_halfblock_box(10, 10, 40, 0), (0, 0));
    }

    #[test]
    fn fit_halfblock_box_width_constrained_uses_full_width() {
        // A wide 200x50 image against a generous budget: width wins. rows =
        // cols * h / (2w) = 100 * 50 / 400 = 12.5 -> ~13, which fits in 40.
        let (cols, rows) = fit_halfblock_box(200, 50, 100, 40);
        assert_eq!(cols, 100, "uses the full width budget");
        assert!((12..=13).contains(&rows), "rows tracks aspect, got {rows}");
        assert!(rows <= 40);
    }

    #[test]
    fn fit_halfblock_box_height_constrained_when_tall() {
        // A tall 50x200 image: fitting to full width (80) would need
        // rows = 80 * 200 / 100 = 160 > 12, so it clamps to the height budget
        // and derives a narrower width instead.
        let (cols, rows) = fit_halfblock_box(50, 200, 80, 12);
        assert_eq!(rows, 12, "uses the full height budget");
        // cols = rows*2*w/h = 24*50/200 = 6.
        assert!((5..=7).contains(&cols), "cols tracks aspect, got {cols}");
        assert!(cols <= 80);
    }

    #[test]
    fn fit_halfblock_box_scales_beyond_the_old_40x12_cap() {
        // The prior hard cap was 40x12; a large pane budget must now produce a
        // much bigger box for a landscape image.
        let (cols, rows) = fit_halfblock_box(1600, 900, 120, 40);
        assert!(cols > 40, "width scales past the old cap, got {cols}");
        assert!(rows > 12, "height scales past the old cap, got {rows}");
        assert!(cols <= 120 && rows <= 40, "stays within budget");
    }

    #[test]
    fn fit_halfblock_box_preserves_aspect_ratio() {
        // On-screen the box is cols wide by rows*2 pixel-rows tall; that ratio
        // should track the source aspect within one cell of rounding.
        let (img_w, img_h) = (300u32, 100u32);
        let (cols, rows) = fit_halfblock_box(img_w, img_h, 90, 60);
        let target = img_w as f64 / img_h as f64;
        let actual = cols as f64 / (rows as f64 * 2.0);
        assert!(
            (actual - target).abs() < 0.25,
            "aspect {actual:.3} vs target {target:.3} (cols={cols}, rows={rows})"
        );
    }

    #[test]
    fn parses_single_placeholder_with_coordinates() {
        let rows = vec![
            "some output here".to_string(),
            "  attached [Image #2] to prompt".to_string(),
        ];
        let found = parse_image_placeholders(&rows);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].number, 2);
        assert_eq!(found[0].row, 1);
        assert_eq!(found[0].col, 11);
    }

    #[test]
    fn parses_multiple_placeholders_including_multi_digit() {
        let rows = vec!["[Image #1] and [Image #10]".to_string()];
        let found = parse_image_placeholders(&rows);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].number, 1);
        assert_eq!(found[0].col, 0);
        assert_eq!(found[1].number, 10);
        assert_eq!(found[1].col, 15);
    }

    #[test]
    fn ignores_malformed_placeholders() {
        let rows = vec![
            "[Image #]".to_string(),
            "[Image 3]".to_string(),
            "[image #3]".to_string(),
            "[Image #3".to_string(),
        ];
        assert!(parse_image_placeholders(&rows).is_empty());
    }

    #[test]
    fn resolve_prefers_png_then_probes(/* tmp */) {
        let dir = std::env::temp_dir().join(format!("host_image_test_{}", std::process::id()));
        let session = "sess-abc";
        let session_dir = dir.join(session);
        std::fs::create_dir_all(&session_dir).unwrap();
        // Only a jpeg present for #1.
        std::fs::write(session_dir.join("1.jpeg"), b"x").unwrap();
        // png present for #2 (preferred even if a jpeg also exists).
        std::fs::write(session_dir.join("2.jpeg"), b"x").unwrap();
        std::fs::write(session_dir.join("2.png"), b"x").unwrap();

        let r1 = resolve_image_cache_file(&dir, session, 1).unwrap();
        assert!(r1.ends_with("1.jpeg"));
        let r2 = resolve_image_cache_file(&dir, session, 2).unwrap();
        assert!(r2.ends_with("2.png"));
        assert!(resolve_image_cache_file(&dir, session, 3).is_none());
        assert!(resolve_image_cache_file(&dir, "", 1).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn png_dimensions_reads_ihdr() {
        // Minimal 1x1 PNG.
        let png = build_1x1_png();
        assert_eq!(png_dimensions(&png), Some((1, 1)));
        assert!(png_dimensions(b"not a png").is_none());
    }

    #[test]
    fn load_kitty_image_bytes_forwards_png_verbatim() {
        let png = build_1x1_png();
        let img = load_kitty_image_bytes(png.clone()).expect("png loads");
        assert_eq!(img.format, KittyImageFormat::Png);
        assert_eq!((img.width_px, img.height_px), (1, 1));
        assert_eq!(img.data, png);
        assert!(load_kitty_image_bytes(b"nope".to_vec()).is_none());
    }

    #[test]
    fn decode_rgba_roundtrips_1x1() {
        let png = build_1x1_png();
        let rgba = decode_rgba(&png).expect("decodes");
        assert_eq!((rgba.width, rgba.height), (1, 1));
        assert_eq!(rgba.data.len(), 4);
    }

    #[test]
    fn halfblock_cells_stack_top_and_bottom_pixels() {
        // 1x2 image: top row red, bottom row green.
        let img = RgbaImage {
            width: 1,
            height: 2,
            data: vec![255, 0, 0, 255, 0, 255, 0, 255],
        };
        let cells = halfblock_cells(&img, 1, 1);
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].top, (255, 0, 0));
        assert_eq!(cells[0].bottom, (0, 255, 0));
    }

    #[test]
    fn halfblock_cells_composite_alpha_over_black() {
        // Single pixel, half-transparent white -> mid-grey over black.
        let img = RgbaImage {
            width: 1,
            height: 1,
            data: vec![255, 255, 255, 128],
        };
        let cells = halfblock_cells(&img, 1, 1);
        assert_eq!(cells[0].top, (128, 128, 128));
        assert_eq!(cells[0].bottom, (128, 128, 128));
    }

    #[test]
    fn paint_halfblock_writes_block_glyph_cells() {
        let dir = std::env::temp_dir().join(format!("halfblock_paint_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("img.png");
        std::fs::write(&path, build_1x1_png()).unwrap();

        let area = ratatui::layout::Rect::new(1, 1, 2, 2);
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 4, 4));
        paint_halfblock_image(&path, area, &mut buf);

        let cell = buf.cell((1, 1)).unwrap();
        assert_eq!(cell.symbol(), HALF_BLOCK.to_string());
        // Opaque-red source -> red fg and bg.
        assert_eq!(cell.fg, ratatui::style::Color::Rgb(255, 0, 0));
        assert_eq!(cell.bg, ratatui::style::Color::Rgb(255, 0, 0));
        // Outside the paint area is untouched.
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), " ");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Build a real 1x1 opaque-red PNG using the `png` encoder.
    fn build_1x1_png() -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[255, 0, 0, 255]).unwrap();
        }
        out
    }
}
