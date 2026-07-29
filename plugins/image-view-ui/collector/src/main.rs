//! Process side of the image-view plugin: pick the newest cached image and
//! measure it, writing `image_input.json` into the module's state dir.
//!
//! WHY this exists as a separate process at all: the wasm guest has no WASI and
//! no host imports, so it cannot open a directory, cannot stat a file, and
//! cannot read a PNG header. Somebody has to, and it must not be the host —
//! putting "find the newest image in the agent cache" into herdr is precisely
//! the host-side image feature this whole exercise removes. So it lives here,
//! in the plugin, where it can be disabled by disabling the plugin.
//!
//! What crosses to the guest is a path and two integers. Never bytes.

use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(serde::Serialize)]
struct ImageInput {
    path: String,
    width: u32,
    height: u32,
}

fn main() -> std::io::Result<()> {
    let root = match std::env::args()
        .nth(1)
        .or_else(|| std::env::var("IMAGE_VIEW_ROOT").ok())
    {
        Some(root) => expand_tilde(&root),
        None => {
            eprintln!("usage: image-view-collector <image-root-dir> [output.json]");
            eprintln!("       (or set IMAGE_VIEW_ROOT; output defaults to");
            eprintln!("        $HERDR_PLUGIN_STATE_DIR/image_input.json)");
            std::process::exit(2);
        }
    };

    let out = std::env::args()
        .nth(2)
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HERDR_PLUGIN_STATE_DIR")
                .map(|dir| PathBuf::from(dir).join("image_input.json"))
        })
        .unwrap_or_else(|| PathBuf::from("image_input.json"));

    let Some(newest) = newest_png(&root)? else {
        eprintln!("no png found under {}", root.display());
        std::process::exit(1);
    };

    let Some((width, height)) = png_dimensions(&newest)? else {
        eprintln!("not a png: {}", newest.display());
        std::process::exit(1);
    };

    let input = ImageInput {
        // Absolute, because the host requires it and because a relative path
        // could not be checked against graphics_roots in any meaningful way.
        path: newest.canonicalize()?.to_string_lossy().into_owned(),
        width,
        height,
    };

    let json = serde_json::to_vec_pretty(&input)?;
    // Write-then-rename so the host never reads a half-written file on the frame
    // that happens to land mid-write.
    let tmp = out.with_extension("json.tmp");
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &out)?;
    println!("{} -> {}", input.path, out.display());
    Ok(())
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

/// Newest `.png` directly under `root` or one level below it, by mtime. One
/// level is enough for the agent caches this targets (`<session>/<n>.png`) and
/// bounds the walk so a mistyped root cannot turn into a filesystem crawl.
fn newest_png(root: &Path) -> std::io::Result<Option<PathBuf>> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut consider = |path: PathBuf| -> std::io::Result<()> {
        if path.extension().and_then(|e| e.to_str()) != Some("png") {
            return Ok(());
        }
        let modified = path.metadata()?.modified()?;
        if best.as_ref().is_none_or(|(seen, _)| modified > *seen) {
            best = Some((modified, path));
        }
        Ok(())
    };

    // An absent image root is the NORMAL case, not an error: the agent creates
    // the cache dir lazily, the first time an image is actually pasted. Left
    // propagating, this printed a bare `Error: Os { code: 2 }` — naming neither
    // the path nor the reason — on every single collector run for any user who
    // had not pasted an image yet, which is most of them. Same rule the
    // claude-image-preview collector states for an image-less session.
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            // A subdirectory that vanishes between listing and descending is a
            // race with the agent writing the cache, not a failure of this run.
            let nested_entries = match std::fs::read_dir(&path) {
                Ok(nested_entries) => nested_entries,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            for nested in nested_entries {
                consider(nested?.path())?;
            }
        } else {
            consider(path)?;
        }
    }
    Ok(best.map(|(_, path)| path))
}

/// Read width/height out of a PNG's IHDR without decoding the image.
///
/// No image crate on purpose: the only question is two big-endian u32s at a
/// fixed offset, and a decoder here would pull a large dependency into a plugin
/// whose entire job is to avoid doing image work.
fn png_dimensions(path: &Path) -> std::io::Result<Option<(u32, u32)>> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut header = [0_u8; 24];
    let mut file = std::fs::File::open(path)?;
    if file.read_exact(&mut header).is_err() {
        return Ok(None);
    }
    if header[..8] != SIGNATURE || &header[12..16] != b"IHDR" {
        return Ok(None);
    }
    let width = u32::from_be_bytes([header[16], header[17], header[18], header[19]]);
    let height = u32::from_be_bytes([header[20], header[21], header[22], header[23]]);
    if width == 0 || height == 0 {
        return Ok(None);
    }
    Ok(Some((width, height)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("image-view-collector-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A minimal but real PNG header: signature, length, `IHDR`, then the two
    /// dimensions we read.
    fn write_png(path: &Path, width: u32, height: u32) {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&13_u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn dimensions_come_from_the_ihdr_header() {
        let dir = temp_dir("ihdr");
        let png = dir.join("a.png");
        write_png(&png, 1280, 720);
        assert_eq!(png_dimensions(&png).unwrap(), Some((1280, 720)));
    }

    /// A file that merely ends in `.png` is not a PNG. Reporting dimensions for
    /// it would hand the host a layer whose geometry is fiction.
    #[test]
    fn a_non_png_is_refused_rather_than_guessed() {
        let dir = temp_dir("notpng");
        let fake = dir.join("a.png");
        std::fs::write(&fake, b"this is not a png at all, not even close").unwrap();
        assert_eq!(png_dimensions(&fake).unwrap(), None);

        let truncated = dir.join("b.png");
        std::fs::write(&truncated, b"\x89PNG").unwrap();
        assert_eq!(png_dimensions(&truncated).unwrap(), None);
    }

    #[test]
    fn the_newest_png_wins_including_one_level_down() {
        let dir = temp_dir("newest");
        let nested = dir.join("session");
        std::fs::create_dir_all(&nested).unwrap();

        let old = dir.join("old.png");
        write_png(&old, 1, 1);
        let new = nested.join("new.png");
        write_png(&new, 2, 2);

        // Make the ordering explicit rather than relying on write order.
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        set_mtime(&old, long_ago);

        assert_eq!(newest_png(&dir).unwrap(), Some(new));
    }

    /// The state this plugin is in for most users most of the time: the agent
    /// has not pasted an image yet, so it has never created the cache dir. It
    /// must be indistinguishable from "the dir is there and empty" — both are
    /// "nothing to show", not a failure. This previously escaped as a bare
    /// `Error: Os { code: 2 }` naming neither the path nor the reason.
    #[test]
    fn an_absent_image_root_is_nothing_to_show_not_an_error() {
        let missing = temp_dir("absent").join("never-created");
        assert!(!missing.exists());
        assert_eq!(newest_png(&missing).unwrap(), None);

        // Same answer as the present-but-empty directory, deliberately.
        let empty = temp_dir("absent-empty");
        assert_eq!(newest_png(&empty).unwrap(), newest_png(&missing).unwrap());
    }

    #[test]
    fn non_png_files_are_ignored_entirely() {
        let dir = temp_dir("ignore");
        std::fs::write(dir.join("notes.txt"), b"hello").unwrap();
        assert_eq!(newest_png(&dir).unwrap(), None);
    }

    fn set_mtime(path: &Path, when: std::time::SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }
}
