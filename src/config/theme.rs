use serde::Deserialize;
use tracing::warn;

/// Theme configuration: pick a built-in or override individual tokens.
///
/// ```toml
/// [theme]
/// name = "tokyo-night"  # built-in: catppuccin, terminal, dracula, nord, etc.
///
/// [theme.custom]        # override individual tokens on top of the base
/// accent = "#f5c2e7"
/// red = "#ff6188"
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// Built-in theme name. Default: "catppuccin".
    pub name: Option<String>,
    /// Follow host terminal light/dark appearance and switch between theme names.
    pub auto_switch: bool,
    /// Theme name used when `auto_switch` selects a dark appearance.
    pub dark_name: Option<String>,
    /// Theme name used when `auto_switch` selects a light appearance.
    pub light_name: Option<String>,
    /// Custom overrides — applied on top of the selected base theme.
    pub custom: Option<CustomThemeColors>,
}

/// Per-token color overrides. All fields optional — only set what you want to change.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct CustomThemeColors {
    pub accent: Option<String>,
    pub panel_bg: Option<String>,
    pub surface0: Option<String>,
    pub surface1: Option<String>,
    pub surface_dim: Option<String>,
    pub overlay0: Option<String>,
    pub overlay1: Option<String>,
    pub text: Option<String>,
    pub subtext0: Option<String>,
    pub mauve: Option<String>,
    pub green: Option<String>,
    pub yellow: Option<String>,
    pub red: Option<String>,
    pub blue: Option<String>,
    pub teal: Option<String>,
    pub peach: Option<String>,
}

/// Why a color string was rejected. Carried to the API caller verbatim, so it
/// has to say what was wrong rather than just that something was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorError(pub String);

impl std::fmt::Display for ColorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Longest color string worth parsing. `rgb(255, 255, 255)` is 18.
const MAX_COLOR_LEN: usize = 32;

/// Parse a color string, REJECTING anything unrecognised.
///
/// [`parse_color`] warns and returns cyan for input it does not understand,
/// which is right for a config file the user can see and edit, and wrong for an
/// API: a caller that sends a typo gets a cyan pane and no way to learn why.
/// Every API path uses this one.
pub fn try_parse_color(s: &str) -> Result<ratatui::style::Color, ColorError> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(ColorError("color must not be empty".into()));
    }
    if trimmed.len() > MAX_COLOR_LEN {
        return Err(ColorError(format!(
            "color must be at most {MAX_COLOR_LEN} characters"
        )));
    }
    let lowered = trimmed.to_lowercase();
    match parse_color_inner(&lowered) {
        Some(color) => Ok(color),
        None => Err(ColorError(format!("unknown color: {trimmed}"))),
    }
}

/// Parse a color string into a ratatui Color.
/// Supports: hex (#rrggbb, #rgb), named colors, rgb(r,g,b), and reset aliases.
///
/// Lenient by design and CONFIG-ONLY: unknown input warns and falls back to
/// cyan. API callers must use [`try_parse_color`], which rejects instead.
pub fn parse_color(s: &str) -> ratatui::style::Color {
    use ratatui::style::Color;
    let s = s.trim().to_lowercase();
    match parse_color_inner(&s) {
        Some(color) => color,
        None => {
            warn!(color = s, "unknown color, defaulting to cyan");
            Color::Cyan
        }
    }
}

/// The shared parse. `None` means "not a color I recognise" — the two public
/// entry points differ only in what they do with that.
fn parse_color_inner(s: &str) -> Option<ratatui::style::Color> {
    use ratatui::style::Color;
    let s = s.trim();

    match s {
        "reset" | "default" | "none" | "transparent" => return Some(Color::Reset),
        _ => {}
    }

    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            if let (Ok(r), Ok(g), Ok(b)) = (
                u8::from_str_radix(&hex[0..2], 16),
                u8::from_str_radix(&hex[2..4], 16),
                u8::from_str_radix(&hex[4..6], 16),
            ) {
                return Some(Color::Rgb(r, g, b));
            }
        } else if hex.len() == 3 {
            let chars: Vec<u8> = hex
                .chars()
                .filter_map(|c| u8::from_str_radix(&c.to_string(), 16).ok())
                .collect();
            if chars.len() == 3 {
                return Some(Color::Rgb(chars[0] * 17, chars[1] * 17, chars[2] * 17));
            }
        }
    }

    if let Some(inner) = s.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let parts: Vec<&str> = inner.split(',').collect();
        if parts.len() == 3 {
            if let (Ok(r), Ok(g), Ok(b)) = (
                parts[0].trim().parse::<u8>(),
                parts[1].trim().parse::<u8>(),
                parts[2].trim().parse::<u8>(),
            ) {
                return Some(Color::Rgb(r, g, b));
            }
        }
    }

    let color = match s {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" | "purple" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        _ => return None,
    };
    Some(color)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn theme_name_parses() {
        let toml = r#"
[theme]
name = "dracula"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.theme.name.as_deref(), Some("dracula"));
    }

    #[test]
    fn parse_color_accepts_reset_aliases() {
        use ratatui::style::Color;

        for value in ["reset", "default", "none", "transparent"] {
            assert_eq!(parse_color(value), Color::Reset, "value: {value}");
        }
    }

    #[test]
    fn theme_auto_switch_fields_parse() {
        let toml = r#"
[theme]
name = "catppuccin"
auto_switch = true
dark_name = "tokyo-night"
light_name = "catppuccin-latte"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.theme.name.as_deref(), Some("catppuccin"));
        assert!(config.theme.auto_switch);
        assert_eq!(config.theme.dark_name.as_deref(), Some("tokyo-night"));
        assert_eq!(config.theme.light_name.as_deref(), Some("catppuccin-latte"));
    }

    #[test]
    fn theme_custom_overrides_parse() {
        let toml = r##"
[theme]
name = "nord"

[theme.custom]
panel_bg = "#1e1e2e"
accent = "#ff79c6"
red = "rgb(255, 85, 85)"
"##;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.theme.name.as_deref(), Some("nord"));
        let custom = config.theme.custom.as_ref().unwrap();
        assert_eq!(custom.panel_bg.as_deref(), Some("#1e1e2e"));
        assert_eq!(custom.accent.as_deref(), Some("#ff79c6"));
        assert_eq!(custom.red.as_deref(), Some("rgb(255, 85, 85)"));
        assert!(custom.green.is_none());
    }

    #[test]
    fn theme_defaults_when_missing() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.theme.name.is_none());
        assert!(!config.theme.auto_switch);
        assert!(config.theme.dark_name.is_none());
        assert!(config.theme.light_name.is_none());
        assert!(config.theme.custom.is_none());
    }
}
