//! Build identity helpers.

pub const BASE_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn channel() -> &'static str {
    non_empty(option_env!("HERDR_BUILD_CHANNEL")).unwrap_or("stable")
}

/// True when this binary was built from a custom fork rather than an official
/// upstream release. Baked at build time by the fork's `.cargo/config.toml`
/// (`HERDR_FORK_BUILD`); an official build leaves it unset. This is orthogonal
/// to [`channel`]/[`version`] so it does not perturb the reported version
/// string. Fork builds must never self-update: the hosted release assets are
/// upstream stable binaries that would overwrite the fork binary and, on a
/// subsequent upstream boot, strip fork-only registry fields.
pub fn is_fork_build() -> bool {
    non_empty(option_env!("HERDR_FORK_BUILD")).is_some() || fork_provenance().is_some()
}

/// The scrapeable, per-binary provenance marker, baked by `build.rs`.
///
/// Deliberately a single self-describing literal so it can be recovered from a
/// binary that is not running and cannot be asked — `strings -a <bin> | grep
/// '^HERDR_FORK_PROVENANCE='`. The version strip needs the local-changes count
/// for instances that are installed but stopped (`herdrtest` normally is), and
/// there is no process to interrogate in that state. It is not routed through
/// `version()` on purpose: that string feeds protocol and update comparisons,
/// and widening it would change behaviour far outside the status line.
/// The trailing `;` is a delimiter, not punctuation. Rust packs string literals
/// into one contiguous table with no separators, so `strings` reports this
/// marker welded to whatever literal was linked next — observed in the first
/// build of this change as `…limit1HERDR_FORK_PROVENANCE=v0.7.5-114-g87556c79`.
/// Without a terminator a reader cannot tell where the describe string ends and
/// the next literal begins, and `git describe` output is all characters that a
/// neighbouring identifier could also contain. `;` cannot appear in it.
pub const FORK_PROVENANCE: &str =
    concat!("HERDR_FORK_PROVENANCE=", env!("HERDR_FORK_PROVENANCE"), ";");

/// Link-level anchor for the marker.
///
/// `#[used]` is the guarantee that the bytes reach the shipped binary: it keeps
/// the static through both LLVM and the linker, independently of whether any
/// code path that reads [`FORK_PROVENANCE`] survives optimisation. Measured on
/// this toolchain (release + LTO + `codegen-units = 1` + `strip`): `#[used]`
/// survives, and `#[no_mangle]` on the same data is DEAD-STRIPPED by the macOS
/// linker — so `#[no_mangle]` must not be used for this. A reference-typed
/// static is deliberate: the describe string's length is not known at authoring
/// time, so a sized `[u8; N]` cannot be written down.
///
/// `#[allow(dead_code)]`: nothing reads this static by design — its only job is
/// to exist in the binary so `grep -a` can recover the marker from a herdr that
/// is installed but not running.
#[used]
#[allow(dead_code)]
static FORK_PROVENANCE_MARKER: &[u8] = FORK_PROVENANCE.as_bytes();

/// The scrapeable, per-binary VARIANT marker, baked by `build.rs`.
///
/// Its own literal and its own `;` terminator, for the same reason
/// [`FORK_PROVENANCE`] has them: Rust welds string literals together, so a
/// marker without a terminator cannot be delimited by `strings`. It is a
/// SEPARATE literal rather than an extra field on the provenance value because
/// [`fork_commit_count`] parses that value right-to-left and requires `g<sha>`
/// to be its last field — appending anything to it silently breaks the `+N`
/// the version strip renders.
pub const FORK_VARIANT: &str = concat!("HERDR_FORK_VARIANT=", env!("HERDR_FORK_VARIANT"), ";");

/// Link-level anchor for the variant marker; see [`FORK_PROVENANCE_MARKER`] for
/// why `#[used]` and not `#[no_mangle]`.
#[used]
#[allow(dead_code)]
static FORK_VARIANT_MARKER: &[u8] = FORK_VARIANT.as_bytes();

/// Which variant of the fork this binary is, e.g. `noimages`. `None` for a
/// plain build, which is the overwhelmingly common case.
// Reader for the scrapeable `HERDR_FORK_VARIANT` marker baked by
// `build.rs::emit_fork_variant` and `.cargo/config.toml`; kept as the in-process
// accessor for that provenance mechanism even while no runtime surface reads it.
#[allow(dead_code)]
pub fn fork_variant() -> Option<&'static str> {
    // `black_box` for the same reason as in `fork_provenance`: without it the
    // const chain folds away and the literal never reaches the binary.
    std::hint::black_box(FORK_VARIANT)
        .split_once('=')
        .map(|(_, value)| value.trim_end_matches(';'))
        .and_then(|value| non_empty(Some(value)))
}

/// `git describe --tags --long` for the tree this binary was built from, e.g.
/// `v0.7.5-114-g87556c79`. `None` for a build with no git provenance.
pub fn fork_provenance() -> Option<&'static str> {
    // `black_box` is load-bearing, not decoration. Without it the whole chain is
    // constant-foldable — `FORK_PROVENANCE` is a `const`, `split_once` on it is
    // const-evaluable, and `is_fork_build()` collapses to `true` — so LLVM drops
    // the literal and the marker is ABSENT from the shipped binary. That was
    // observed on the first build of this change: the code was correct, the
    // string was gone, and `strings` found nothing. The marker's entire purpose
    // is to be recoverable from a file that is not running, so it has to survive
    // optimisation, which means the compiler must be told it cannot know the
    // value.
    std::hint::black_box(FORK_PROVENANCE)
        .split_once('=')
        .map(|(_, value)| value.trim_end_matches(';'))
        .and_then(|value| non_empty(Some(value)))
        .filter(|value| *value != "unknown")
}

/// Commits since the nearest tag — the `+N` the version strip renders.
// Kept as the canonical parser of the `HERDR_FORK_PROVENANCE` marker: its
// right-to-left `g<sha>`-last contract is what `.cargo/config.toml` and
// `build.rs` are written against, and the tests mirror this parser.
#[allow(dead_code)]
pub fn fork_commit_count() -> Option<u32> {
    // `<tag>-<ahead>-g<sha>[-dirty]`; take the field before the `g<sha>`.
    let desc = fork_provenance()?;
    let desc = desc.strip_suffix("-dirty").unwrap_or(desc);
    let mut parts = desc.rsplitn(3, '-');
    let sha = parts.next()?;
    if !sha.starts_with('g') {
        return None; // `--always` fallback: a bare sha, no tag reachable.
    }
    parts.next()?.parse().ok()
}

pub fn build_id() -> Option<&'static str> {
    non_empty(option_env!("HERDR_BUILD_ID"))
}

pub fn version() -> String {
    match channel() {
        "stable" => BASE_VERSION.to_string(),
        channel => match build_id() {
            Some(build_id) => format!("{BASE_VERSION}-{channel}.{build_id}"),
            None => format!("{BASE_VERSION}-{channel}"),
        },
    }
}

pub fn is_preview() -> bool {
    channel() == "preview"
}

fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn stable_version_defaults_to_cargo_version() {
        assert!(!super::version().is_empty());
    }

    #[test]
    fn provenance_marker_is_self_describing_so_it_can_be_scraped_from_a_stopped_binary() {
        assert!(super::FORK_PROVENANCE.starts_with("HERDR_FORK_PROVENANCE="));
        assert!(super::FORK_PROVENANCE.ends_with(';'));
    }

    #[test]
    fn variant_marker_is_its_own_literal_and_never_rides_on_the_provenance_value() {
        assert!(super::FORK_VARIANT.starts_with("HERDR_FORK_VARIANT="));
        assert!(super::FORK_VARIANT.ends_with(';'));
        // The whole point of the split: whatever the variant is, the provenance
        // string must still end in `g<sha>` so `fork_commit_count` can parse it.
        assert!(!super::FORK_PROVENANCE.contains("HERDR_FORK_VARIANT"));
    }

    #[test]
    fn an_unset_variant_is_none_rather_than_an_empty_name() {
        // This crate is built without HERDR_FORK_VARIANT, so the plain-build
        // path is the one under test here.
        if option_env!("HERDR_FORK_VARIANT").unwrap_or("").is_empty() {
            assert_eq!(super::fork_variant(), None);
        }
    }

    #[test]
    fn commit_count_parses_a_describe_string_and_refuses_a_bare_sha() {
        // The shapes `git describe --tags --long --always --dirty` can produce.
        assert_eq!(count_of("v0.7.5-114-g87556c79"), Some(114));
        assert_eq!(count_of("v0.7.5-114-g87556c79-dirty"), Some(114));
        assert_eq!(count_of("v0.7.5-0-gabc1234"), Some(0));
        // No tag reachable: `--always` degrades to a bare sha. Borrowing a number
        // here is exactly the bug this replaces, so it must stay None.
        assert_eq!(count_of("87556c79"), None);
        assert_eq!(count_of("unknown"), None);
    }

    /// Mirror of `fork_commit_count`'s parser over an arbitrary describe string,
    /// since the real one reads a value baked at compile time.
    fn count_of(desc: &str) -> Option<u32> {
        if desc == "unknown" {
            return None;
        }
        let desc = desc.strip_suffix("-dirty").unwrap_or(desc);
        let mut parts = desc.rsplitn(3, '-');
        let sha = parts.next()?;
        if !sha.starts_with('g') {
            return None;
        }
        parts.next()?.parse().ok()
    }
}
