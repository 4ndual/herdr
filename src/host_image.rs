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
//! exist on disk.
//!
//! fork(+126): compiled only under `--features host-images`, default off.
//! fork(+127): decoding moved to `crate::image_decode` (always compiled). What
//! remains here is exactly the Claude-specific POLICY -- where the cache lives
//! and how `[Image #N]` is spelled -- which is the part +127 hands to a plugin.

use std::path::{Path, PathBuf};

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

// fork(+127): the codecs moved to `crate::image_decode`, which is compiled
// unconditionally so plugin-owned graphics layers can render without the
// `host-images` feature. Re-exported here so every existing `host_image::`
// call site (`kitty_graphics.rs`, `app/mod.rs`) keeps compiling unchanged --
// this file is the one an upstream merge is most likely to touch, so its
// surface deliberately did not change.
#[allow(unused_imports)]
pub(crate) use crate::image_decode::{
    cached_kitty_image, cached_rgba_image, decode_rgba, fit_halfblock_box, halfblock_cells,
    halfblock_image_box, load_kitty_image_bytes, paint_halfblock_image, HalfblockCell,
    KittyImageBytes, RgbaImage, HALF_BLOCK,
};

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

/// The default Claude image-cache root, `~/.claude/image-cache`.
pub(crate) fn default_image_cache_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".claude").join("image-cache"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
