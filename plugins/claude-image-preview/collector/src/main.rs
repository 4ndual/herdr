//! Process side of the Claude image-preview plugin: for the focused agent
//! session, enumerate `~/.claude/image-cache/<session>/<N>.<ext>` and measure
//! each file, writing `image_input.json` into the module's state dir.
//!
//! WHY this is a separate process and not host code: it is the whole point of
//! +127. Deciding that Claude keeps its images in `~/.claude/image-cache`, that
//! they are named after the `N` in `[Image #N]`, and that PNG/JPEG are the
//! formats, is *Claude policy*. Before +127 that policy lived in herdr
//! (`src/host_image.rs`) and could only be removed by rebuilding herdr. It now
//! lives here, where `herdr plugin disable builtin.claude-image-preview` removes
//! it at runtime.
//!
//! WHY it is not in the guest either: a wasm32 guest here has no WASI and no
//! host imports. It cannot list a directory, stat a file, or read a PNG header.
//!
//! So the split is:
//!   * this collector answers "which N exist on disk, where, and how big" —
//!     filesystem facts;
//!   * the guest answers "which N are visible on screen and where" — it gets
//!     that from `pane.read` through the bridge, and joins the two by `N`;
//!   * the host reads the image bytes, and only from a directory this plugin's
//!     manifest declared in `graphics_roots`.
//!
//! Image BYTES never enter this process's output. Only paths and dimensions.

use std::io::Read;
use std::path::{Path, PathBuf};

/// Extensions probed per `N`, in preference order. PNG first because it is the
/// only format the kitty transmit path can forward without a re-encode.
const EXTENSIONS: [&str; 3] = ["png", "jpeg", "jpg"];

/// Upper bound on how many images are described per session. A placeholder
/// stream is unbounded in principle and this file is re-read by the host every
/// frame; capping it here keeps a pathological cache directory from turning
/// into a per-frame parse cost. Matches the manifest's `max_layers_per_pane`.
const MAX_IMAGES: usize = 8;

#[derive(serde::Deserialize, Default)]
struct FocusInput {
    session_id: Option<String>,
}

#[derive(serde::Serialize)]
struct ImageEntry {
    /// The `N` from `[Image #N]` / the cache file stem.
    number: u32,
    /// Absolute, canonicalised path. Absolute because the host requires it, and
    /// canonicalised because `graphics_roots` is checked component-wise against
    /// a canonical path — a relative path or an unresolved symlink could not be
    /// checked meaningfully.
    path: String,
    width: u32,
    height: u32,
}

#[derive(serde::Serialize)]
struct ImageInput {
    /// Which session these entries belong to. The guest compares it against the
    /// focused pane's `agent_session` and paints nothing on a mismatch, so a
    /// focus change mid-flight can never paint one session's images onto
    /// another's pane.
    session_id: String,
    images: Vec<ImageEntry>,
}

fn main() -> std::io::Result<()> {
    let root = match std::env::args()
        .nth(1)
        .or_else(|| std::env::var("CLAUDE_IMAGE_CACHE_ROOT").ok())
    {
        Some(root) => expand_tilde(&root),
        None => default_cache_root(),
    };

    let out = std::env::args()
        .nth(2)
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HERDR_PLUGIN_STATE_DIR")
                .map(|dir| PathBuf::from(dir).join("image_input.json"))
        })
        .unwrap_or_else(|| PathBuf::from("image_input.json"));

    let focus: FocusInput = std::env::var("HERDR_FOCUS_JSON")
        .ok()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();

    // No focused agent session means no cache directory to look in. Write an
    // EMPTY input rather than exiting: the guest has to be told "nothing", or
    // its no-repaint guard keeps the previous session's images on screen.
    //
    // `CLAUDE_IMAGE_SESSION` pins a session explicitly. It exists because herdr
    // cannot always attribute a pane to an agent session — a resumed or
    // wrapper-launched Claude, or a pane driven by something herdr does not
    // detect — and without it the plugin is simply blind in those cases. It is
    // also what makes the pipeline testable against a real cache directory
    // without a live agent. The focus value wins when both are present, so
    // pinning never overrides a session herdr actually knows.
    let session_id = focus
        .session_id
        .filter(|id| !id.is_empty())
        .or_else(|| std::env::var("CLAUDE_IMAGE_SESSION").ok())
        .unwrap_or_default();

    let images = if session_id.is_empty() {
        Vec::new()
    } else {
        collect_images(&root, &session_id)?
    };

    let input = ImageInput { session_id, images };
    write_atomic(&out, &input)
}

/// Enumerate `<root>/<session>/<N>.<ext>` for every `N` present, measuring each.
/// Files whose stem is not a number are ignored: the guest keys on `N` and has
/// nothing to do with anything else that lands in the directory.
fn collect_images(root: &Path, session_id: &str) -> std::io::Result<Vec<ImageEntry>> {
    let dir = root.join(session_id);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // A session with no images is the normal case, not an error.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };

    let mut numbers: Vec<u32> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let has_known_ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| {
                EXTENSIONS
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(ext))
            })
            .unwrap_or(false);
        if !has_known_ext {
            continue;
        }
        if let Some(number) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.parse::<u32>().ok())
        {
            numbers.push(number);
        }
    }
    numbers.sort_unstable();
    numbers.dedup();
    numbers.truncate(MAX_IMAGES);

    let mut out = Vec::with_capacity(numbers.len());
    for number in numbers {
        let Some(path) = probe(&dir, number) else {
            continue;
        };
        // Dimensions come from the file header, never from a decode: this runs
        // on a frame cadence and decoding a large PNG here would show up as this
        // plugin's cost. JPEG is measured by walking SOF markers for the same
        // reason.
        let Some((width, height)) = dimensions(&path)? else {
            continue;
        };
        let Ok(canonical) = path.canonicalize() else {
            continue;
        };
        out.push(ImageEntry {
            number,
            path: canonical.to_string_lossy().into_owned(),
            width,
            height,
        });
    }
    Ok(out)
}

/// Same probe order the host used before +127, kept identical on purpose: this
/// plugin replaces that code path, so a user must not see a different image
/// resolved for the same placeholder.
fn probe(dir: &Path, number: u32) -> Option<PathBuf> {
    for ext in EXTENSIONS {
        let candidate = dir.join(format!("{number}.{ext}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn write_atomic(out: &Path, input: &ImageInput) -> std::io::Result<()> {
    let json = serde_json::to_vec(input)?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write-then-rename so the host never reads a half-written file on the frame
    // that happens to land mid-write.
    let tmp = out.with_extension("json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, out)?;
    println!("{} image(s) -> {}", input.images.len(), out.display());
    Ok(())
}

fn default_cache_root() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".claude").join("image-cache"),
        None => PathBuf::from(".claude/image-cache"),
    }
}

fn expand_tilde(raw: &str) -> PathBuf {
    match raw.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(rest),
            None => PathBuf::from(raw),
        },
        None => PathBuf::from(raw),
    }
}

// ── header-only measurement ────────────────────────────────────────────────

fn dimensions(path: &Path) -> std::io::Result<Option<(u32, u32)>> {
    let mut file = std::fs::File::open(path)?;
    let mut head = [0u8; 32];
    let read = read_full(&mut file, &mut head)?;
    let head = &head[..read];
    if let Some(dims) = png_dimensions(head) {
        return Ok(Some(dims));
    }
    if head.starts_with(&[0xFF, 0xD8]) {
        // JPEG needs a marker walk; re-open rather than seek-juggling the 32
        // bytes already consumed.
        return jpeg_dimensions(path).map(Some).or(Ok(None));
    }
    Ok(None)
}

/// PNG IHDR: 8-byte signature, then a chunk whose length/type occupy 8 bytes and
/// whose first 8 payload bytes are width/height big-endian.
fn png_dimensions(head: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if head.len() < 24 || head[..8] != SIGNATURE || &head[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes([head[16], head[17], head[18], head[19]]);
    let height = u32::from_be_bytes([head[20], head[21], head[22], head[23]]);
    (width > 0 && height > 0).then_some((width, height))
}

/// Walk JPEG segment markers to the first SOFn frame header, which carries the
/// dimensions. Bounded by the file itself; SOF is near the front in practice.
fn jpeg_dimensions(path: &Path) -> std::io::Result<(u32, u32)> {
    let bytes = std::fs::read(path)?;
    let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidData, "not a measurable jpeg");
    let mut i = 2usize; // skip SOI
    while i + 4 <= bytes.len() {
        if bytes[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = bytes[i + 1];
        // Standalone markers carry no length payload.
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        // SOF0..SOF15 except the DHT/JPG/DAC markers interleaved in that range.
        let is_sof =
            (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC;
        if is_sof {
            if i + 9 > bytes.len() {
                return Err(invalid());
            }
            let height = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
            let width = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
            if width == 0 || height == 0 {
                return Err(invalid());
            }
            return Ok((width, height));
        }
        if len < 2 {
            return Err(invalid());
        }
        i += 2 + len;
    }
    Err(invalid())
}

fn read_full(file: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_head(width: u32, height: u32) -> Vec<u8> {
        let mut head = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        head.extend_from_slice(&13u32.to_be_bytes());
        head.extend_from_slice(b"IHDR");
        head.extend_from_slice(&width.to_be_bytes());
        head.extend_from_slice(&height.to_be_bytes());
        head
    }

    #[test]
    fn png_header_is_measured_without_decoding() {
        assert_eq!(png_dimensions(&png_head(1920, 1080)), Some((1920, 1080)));
    }

    /// A truncated or non-PNG head must degrade to "cannot measure", never to a
    /// panic or a bogus size: this runs on a frame cadence over files written by
    /// another process that may still be writing them.
    #[test]
    fn a_non_png_or_truncated_head_measures_as_none() {
        assert_eq!(png_dimensions(b"not a png at all"), None);
        assert_eq!(png_dimensions(&png_head(1, 1)[..12]), None);
        assert_eq!(png_dimensions(&png_head(0, 0)), None);
    }

    #[test]
    fn tilde_expands_against_home() {
        std::env::set_var("HOME", "/home/tester");
        assert_eq!(
            expand_tilde("~/.claude/image-cache"),
            PathBuf::from("/home/tester/.claude/image-cache")
        );
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }
}
