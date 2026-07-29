use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn zig_target(target: &str) -> &str {
    match target {
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
        "x86_64-apple-darwin" => "x86_64-macos",
        "aarch64-apple-darwin" => "aarch64-macos",
        "x86_64-pc-windows-msvc" => "x86_64-windows-msvc",
        "aarch64-pc-windows-msvc" => "aarch64-windows-msvc",
        other => panic!("unsupported target for libghostty-vt build: {other}"),
    }
}

fn env_bool(name: &str) -> Option<bool> {
    match env::var(name) {
        Ok(value) => match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            other => panic!("invalid boolean value for {name}: {other}"),
        },
        Err(env::VarError::NotPresent) => None,
        Err(err) => panic!("failed to read {name}: {err}"),
    }
}

/// Bake this build's own git provenance into the binary.
///
/// WHY: the version strip renders a `+N` local-changes marker that Andres uses
/// to see how far the running thing is from the released tag. It used to be
/// computed once from the working tree at `~/Projects/herdr` and stamped onto
/// EVERY token, so a binary that was never built from that tree still displayed
/// its count — an upstream 0.7.5 with zero fork commits in it rendered `+113`.
/// The count has to be a property of the BINARY, not of whatever tree happens to
/// be checked out when the collector runs, so it is baked here and read back out
/// of the file. A binary without this stamp (upstream, or built before this
/// change) reports nothing and must render as unknown rather than borrowing a
/// number from somewhere else.
fn emit_fork_provenance(manifest_dir: &std::path::Path) {
    println!("cargo:rerun-if-env-changed=HERDR_FORK_PROVENANCE");
    // A git dir change (new commit, checkout) must re-stamp the binary.
    for path in ["/.git/HEAD", "/.git"] {
        println!("cargo:rerun-if-changed={}{}", manifest_dir.display(), path);
    }
    let provenance = env::var("HERDR_FORK_PROVENANCE").ok().or_else(|| {
        Command::new("git")
            .arg("-C")
            .arg(manifest_dir)
            // `--match v*` pins the base to RELEASE tags. Without it, `describe`
            // takes any tag reachable from HEAD, so an unrelated tag sitting on
            // the tip silently rewrites this string: a preservation tag named
            // `fork/pre-decouple-v0.7.5-138` produced
            // `fork/pre-decouple-v0.7.5-138-0-g2b012356` instead of
            // `v0.7.5-138-g2b012356`. That fails quietly -- the build is clean
            // and the string looks plausible -- but `fork_commit_count()` parses
            // it right-to-left and would have read the `0` as the count, so the
            // version strip rendered `+0` instead of `+138` on the exact glyph
            // used to identify a build. Archival tags must not be able to do
            // that; only `v*` release tags may set the base.
            .args([
                "describe", "--tags", "--match", "v*", "--long", "--always", "--dirty",
            ])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|desc| !desc.is_empty())
    });
    // Always emit: `env!` in build_info.rs must resolve on every build, including
    // a source tarball with no git dir. "unknown" is the honest fallback.
    println!(
        "cargo:rustc-env=HERDR_FORK_PROVENANCE={}",
        provenance.as_deref().unwrap_or("unknown")
    );
}

/// Bake which fork VARIANT this binary is, alongside — never inside — the
/// provenance string.
///
/// WHY separate: provenance answers "which commit", variant answers "which
/// build of that commit". Two binaries can share a describe string and differ
/// in what was compiled into them (a feature-stripped build, a bisect build),
/// and appending the variant to the provenance value would corrupt it —
/// `fork_commit_count` parses that string right-to-left and requires `g<sha>`
/// to be the last field. So this gets its own `cargo:rustc-env` line, its own
/// self-describing literal, and its own `#[used]` anchor, scrapeable the same
/// way: `strings -a <bin> | grep '^HERDR_FORK_VARIANT='`.
fn emit_fork_variant() {
    println!("cargo:rerun-if-env-changed=HERDR_FORK_VARIANT");
    // Always emit: `env!` in build_info.rs must resolve on every build. Empty is
    // the honest value for a plain build — unlike provenance there is no
    // fallback worth computing, and "unknown" would falsely imply a variant
    // exists that we failed to name.
    println!(
        "cargo:rustc-env=HERDR_FORK_VARIANT={}",
        env::var("HERDR_FORK_VARIANT").unwrap_or_default().trim()
    );
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/libghostty-vt.vendor.json");
    println!("cargo:rerun-if-changed=vendor/libghostty-vt/build.zig");
    println!("cargo:rerun-if-changed=vendor/libghostty-vt/build.zig.zon");
    println!("cargo:rerun-if-changed=vendor/libghostty-vt/include");
    println!("cargo:rerun-if-changed=vendor/libghostty-vt/pkg");
    println!("cargo:rerun-if-changed=vendor/libghostty-vt/src");
    println!("cargo:rerun-if-changed=vendor/libghostty-vt/VERSION");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_OPTIMIZE");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SIMD");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_ZIG_SYSTEM_DIR");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_CHANNEL");
    println!("cargo:rerun-if-env-changed=HERDR_FORK_BUILD");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_ID");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=ZIG");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    emit_fork_provenance(&manifest_dir);
    emit_fork_variant();
    let vendored_dir = manifest_dir.join("vendor/libghostty-vt");
    let optimize = env::var("LIBGHOSTTY_VT_OPTIMIZE").unwrap_or_else(|_| "ReleaseFast".into());
    let simd = env_bool("LIBGHOSTTY_VT_SIMD").unwrap_or(true);
    let target = env::var("TARGET").expect("TARGET");
    let zig_target = zig_target(&target);
    let version_string = fs::read_to_string(vendored_dir.join("VERSION"))
        .expect("failed to read vendored libghostty-vt VERSION")
        .trim()
        .to_string();

    let zig = env::var("ZIG").unwrap_or_else(|_| "zig".into());
    let mut command = Command::new(zig);
    command
        .arg("build")
        .arg("-Demit-lib-vt")
        .arg(format!("-Doptimize={optimize}"))
        .arg(format!("-Dsimd={simd}"))
        .arg(format!("-Dtarget={zig_target}"))
        .arg(format!("-Dversion-string={version_string}"))
        .arg("-Demit-xcframework=false");
    if let Ok(system_dir) = env::var("LIBGHOSTTY_VT_ZIG_SYSTEM_DIR") {
        command.arg("--system").arg(system_dir);
    }

    let status = command
        .current_dir(&vendored_dir)
        .status()
        .expect("failed to execute zig build for vendored libghostty-vt");
    assert!(
        status.success(),
        "zig build for vendored libghostty-vt failed: {status}"
    );

    let lib_dir = vendored_dir.join("zig-out/lib");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    if target.contains("apple-darwin") {
        let static_lib = lib_dir.join("libghostty-vt.a");
        println!("cargo:rustc-link-arg={}", static_lib.display());
    } else if target.contains("windows-msvc") {
        println!("cargo:rustc-link-lib=static=ghostty-vt-static");
    } else {
        println!("cargo:rustc-link-lib=static=ghostty-vt");
    }
}
