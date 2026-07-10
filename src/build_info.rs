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
    non_empty(option_env!("HERDR_FORK_BUILD")).is_some()
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
}
