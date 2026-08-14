pub(super) fn tab_attention_priority(state: crate::detect::AgentState, seen: bool) -> u8 {
    match (state, seen) {
        (crate::detect::AgentState::Blocked, _) => 4,
        (crate::detect::AgentState::Idle, false) => 3,
        (crate::detect::AgentState::Working, _) => 2,
        (crate::detect::AgentState::Idle, true) => 1,
        (crate::detect::AgentState::Unknown, _) => 0,
    }
}

fn parse_api_key(key: &str) -> Option<crossterm::event::KeyEvent> {
    let normalized = normalize_api_key_alias(key.trim());
    let (code, modifiers) = crate::config::parse_key_combo(normalized)?;
    Some(crossterm::event::KeyEvent::new(code, modifiers))
}

fn normalize_api_key_alias(key: &str) -> &str {
    match key {
        "C-c" | "c-c" => "ctrl+c",
        "+" => "plus",
        _ => key,
    }
}

pub(super) fn encode_api_text(runtime: &crate::terminal::TerminalRuntime, text: &str) -> Vec<u8> {
    let bracketed = runtime
        .input_state()
        .map(|state| state.bracketed_paste)
        .unwrap_or(false);
    if bracketed {
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.as_bytes().to_vec()
    }
}

// fork(0.7.5-merge): the fork's `encode_api_submission` (added for its own
// `agent prompt`) and upstream 0.7.5's identical helper auto-merged into this
// file as two separate definitions with no conflict marker. Bodies were
// byte-identical; the fork's copy was removed and upstream's kept below.
pub(super) fn encode_api_keys(
    runtime: &crate::terminal::TerminalRuntime,
    keys: &[String],
) -> Result<Vec<Vec<u8>>, String> {
    let mut encoded_keys = Vec::with_capacity(keys.len());
    for key in keys {
        let Some(key_event) = parse_api_key(key) else {
            return Err(key.clone());
        };
        encoded_keys.push(runtime.encode_terminal_key(key_event.into()));
    }
    Ok(encoded_keys)
}

pub(super) fn encode_api_submission_parts(
    runtime: &crate::terminal::TerminalRuntime,
    text: &str,
) -> (Vec<u8>, Vec<u8>) {
    let text = encode_api_text(runtime, text);
    let enter = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    );
    (text, runtime.encode_terminal_key(enter.into()))
}

pub(super) fn encode_api_submission(
    runtime: &crate::terminal::TerminalRuntime,
    text: &str,
) -> Vec<u8> {
    let (mut text, enter) = encode_api_submission_parts(runtime, text);
    text.extend_from_slice(&enter);
    text
}

pub(super) fn encode_api_input(
    runtime: &crate::terminal::TerminalRuntime,
    text: &str,
    keys: &[String],
) -> Result<Vec<u8>, String> {
    let mut bytes = if text.is_empty() {
        Vec::new()
    } else {
        encode_api_text(runtime, text)
    };
    for encoded in encode_api_keys(runtime, keys)? {
        bytes.extend_from_slice(&encoded);
    }
    Ok(bytes)
}

pub(super) fn detect_state_from_api(
    state: crate::api::schema::PaneAgentState,
) -> crate::detect::AgentState {
    match state {
        crate::api::schema::PaneAgentState::Idle => crate::detect::AgentState::Idle,
        crate::api::schema::PaneAgentState::Working => crate::detect::AgentState::Working,
        crate::api::schema::PaneAgentState::Blocked => crate::detect::AgentState::Blocked,
        crate::api::schema::PaneAgentState::Unknown => crate::detect::AgentState::Unknown,
    }
}

pub(super) fn pane_agent_status(
    state: crate::detect::AgentState,
    seen: bool,
) -> crate::api::schema::AgentStatus {
    match (state, seen) {
        (crate::detect::AgentState::Idle, false) => crate::api::schema::AgentStatus::Done,
        (crate::detect::AgentState::Idle, true) => crate::api::schema::AgentStatus::Idle,
        (crate::detect::AgentState::Working, _) => crate::api::schema::AgentStatus::Working,
        (crate::detect::AgentState::Blocked, _) => crate::api::schema::AgentStatus::Blocked,
        (crate::detect::AgentState::Unknown, _) => crate::api::schema::AgentStatus::Unknown,
    }
}

pub(super) fn read_terminal_snapshot(
    terminal: &crate::terminal::TerminalRuntime,
    source: crate::api::schema::ReadSource,
    format: crate::api::schema::ReadFormat,
    lines: Option<u32>,
) -> crate::pane::TerminalReadSnapshot {
    use crate::api::schema::{ReadFormat, ReadSource};

    let line_limit = lines.map(|lines| lines.min(1000) as usize);
    let recent_lines = line_limit.unwrap_or(80);
    match (format, source) {
        (ReadFormat::Text, ReadSource::Visible) => {
            limit_snapshot_lines(terminal.visible_text(), line_limit)
        }
        (ReadFormat::Text, ReadSource::Recent) => terminal.recent_text_snapshot(recent_lines),
        (ReadFormat::Text, ReadSource::RecentUnwrapped) => {
            terminal.recent_unwrapped_text_snapshot(recent_lines)
        }
        (ReadFormat::Text, ReadSource::Detection) => {
            limit_snapshot_lines(terminal.detection_text(), line_limit)
        }
        (ReadFormat::Ansi, ReadSource::Visible) => {
            limit_snapshot_lines(terminal.visible_ansi(), line_limit)
        }
        (ReadFormat::Ansi, ReadSource::Recent) => terminal.recent_ansi_snapshot(recent_lines),
        (ReadFormat::Ansi, ReadSource::RecentUnwrapped) => {
            terminal.recent_unwrapped_ansi_snapshot(recent_lines)
        }
        (ReadFormat::Ansi, ReadSource::Detection) => {
            limit_snapshot_lines(terminal.detection_text(), line_limit)
        }
    }
}

pub(crate) fn limit_snapshot_lines(
    text: String,
    limit: Option<usize>,
) -> crate::pane::TerminalReadSnapshot {
    let Some(limit) = limit else {
        return crate::pane::TerminalReadSnapshot {
            text,
            truncated: false,
        };
    };
    let lines: Vec<_> = text.split_inclusive('\n').collect();
    crate::pane::TerminalReadSnapshot {
        text: lines[lines.len().saturating_sub(limit)..].concat(),
        truncated: lines.len() > limit,
    }
}

#[cfg(test)]
mod read_snapshot_tests {
    use super::limit_snapshot_lines;

    #[test]
    fn line_limit_preserves_endings_and_reports_omitted_lines() {
        let snapshot = limit_snapshot_lines("one\ntwø\n三\n".into(), Some(2));
        assert_eq!(snapshot.text, "twø\n三\n");
        assert!(snapshot.truncated);

        let snapshot = limit_snapshot_lines("one\ntwo\nthree".into(), Some(1));
        assert_eq!(snapshot.text, "three");
        assert!(snapshot.truncated);

        let snapshot = limit_snapshot_lines("one\ntwo".into(), Some(0));
        assert_eq!(snapshot.text, "");
        assert!(snapshot.truncated);

        let snapshot = limit_snapshot_lines("".into(), Some(2));
        assert_eq!(snapshot.text, "");
        assert!(!snapshot.truncated);
    }

    #[test]
    fn omitted_line_limit_returns_the_complete_snapshot() {
        let snapshot = limit_snapshot_lines("one\ntwo\n".into(), None);
        assert_eq!(snapshot.text, "one\ntwo\n");
        assert!(!snapshot.truncated);
    }
}

pub(super) fn normalize_reported_agent_label(agent: &str) -> Option<String> {
    let trimmed = agent.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(agent) = crate::detect::parse_agent_label(trimmed) {
        return Some(crate::detect::agent_label(agent).to_string());
    }
    Some(trimmed.to_string())
}

pub(super) const METADATA_TTL_MAX_MS: u64 = 86_400_000;
pub(super) const METADATA_SOURCE_MAX_CHARS: usize = 80;
const METADATA_TTL_MIN_MS: u64 = 1;
const MAX_METADATA_TOKEN_KEYS_PER_REQUEST: usize = 16;
pub(super) const MAX_METADATA_TOKEN_KEYS_PER_RESOURCE: usize = 32;
const MAX_METADATA_TOKEN_KEY_LEN: usize = 32;
const MAX_METADATA_TOKEN_VALUE_LEN: usize = 80;
/// Metadata key the host interprets instead of merely displaying: it tints the
/// sidebar row and the pane border. Reserved, and validated at the handler so an
/// invalid value is a request error with the prior state untouched — validating
/// at render time instead would accept the write and then silently ignore it.
pub(crate) const METADATA_COLOR_KEY: &str = "color";

pub(super) fn normalize_metadata_source(value: String) -> Result<String, &'static str> {
    let value = value.trim();
    if value.is_empty() {
        return Err("metadata source must not be empty");
    }
    if value.chars().count() > METADATA_SOURCE_MAX_CHARS {
        return Err("metadata source must be 80 characters or fewer");
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '.' | '_' | '-'))
    {
        return Err(
            "metadata source may contain only ASCII letters, digits, colon, dot, underscore, and hyphen",
        );
    }
    Ok(value.to_string())
}

pub(super) fn normalize_metadata_ttl(
    ttl_ms: Option<u64>,
) -> Result<Option<std::time::Duration>, &'static str> {
    let Some(ttl_ms) = ttl_ms else {
        return Ok(None);
    };
    if ttl_ms < METADATA_TTL_MIN_MS {
        return Err("metadata ttl_ms must be at least 1");
    }
    if ttl_ms > METADATA_TTL_MAX_MS {
        return Err("metadata ttl_ms must be 86400000 or less");
    }
    Ok(Some(std::time::Duration::from_millis(ttl_ms)))
}

pub(super) fn normalize_metadata_tokens(
    tokens: std::collections::HashMap<String, Option<String>>,
) -> Result<std::collections::HashMap<String, Option<String>>, String> {
    if tokens.is_empty() {
        return Err("missing token to set or clear".into());
    }
    if tokens.len() > MAX_METADATA_TOKEN_KEYS_PER_REQUEST {
        return Err(format!(
            "a metadata report may update at most {MAX_METADATA_TOKEN_KEYS_PER_REQUEST} tokens"
        ));
    }

    tokens
        .into_iter()
        .map(|(key, value)| {
            if key.is_empty()
                || key.len() > MAX_METADATA_TOKEN_KEY_LEN
                || !key
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
            {
                return Err(format!("invalid metadata token key: {key}"));
            }
            let value = value.and_then(|value| {
                let normalized = value
                    .trim()
                    .chars()
                    .filter(|ch| !ch.is_control())
                    .take(MAX_METADATA_TOKEN_VALUE_LEN)
                    .collect::<String>();
                (!normalized.trim().is_empty()).then(|| normalized.trim().to_string())
            });
            // The reserved `color` key is interpreted by the host, not merely
            // displayed, so a bad value is a request error here rather than a
            // silently ignored write. Clearing it (value `None`) is always legal.
            let value = match (key.as_str(), value) {
                (METADATA_COLOR_KEY, Some(value)) => Some(validate_metadata_color(&value)?),
                (_, value) => value,
            };
            Ok((key, value))
        })
        .collect()
}

/// Theme role names accepted for the reserved `color` token.
///
/// Mirrors the `plugin_span_style` vocabulary in `src/ui.rs` and
/// `guest::color_role()` in the plugin API crate. A role is resolved against the
/// LIVE palette at render time, which is the point: `red` means "this theme's
/// red", so a tinted row still looks right after a theme change. A caller that
/// wants a fixed red asks for `#ff0000`.
pub(crate) const METADATA_COLOR_ROLES: &[&str] = &[
    "accent", "muted", "success", "warning", "danger", "green", "yellow", "red", "blue", "teal",
    "cyan", "mauve", "magenta", "subtext", "gray", "peach", "text",
];

/// Validate the reserved `color` token, returning the normalized value.
///
/// Accepts a theme role or an explicit color literal (hex / `rgb()` / reset).
/// Rejects anything else \u2014 including bare ANSI names that are not roles, which
/// would otherwise read as "some fixed color" while behaving like neither the
/// theme's nor the caller's. Rejection happens here, at the handler, so the
/// prior value survives a bad write instead of being replaced by a default.
pub(crate) fn validate_metadata_color(value: &str) -> Result<String, String> {
    let normalized = value.trim().to_lowercase();
    if METADATA_COLOR_ROLES.contains(&normalized.as_str()) {
        return Ok(normalized);
    }
    let looks_literal = normalized.starts_with('#')
        || normalized.starts_with("rgb(")
        || matches!(
            normalized.as_str(),
            "reset" | "default" | "none" | "transparent"
        );
    if !looks_literal {
        return Err(format!(
            "unknown color role `{normalized}`; use one of {} or an explicit #rrggbb",
            METADATA_COLOR_ROLES.join(", ")
        ));
    }
    crate::config::theme::try_parse_color(&normalized).map_err(|err| err.to_string())?;
    Ok(normalized)
}

#[cfg(test)]
mod metadata_token_tests {
    use super::*;

    #[test]
    fn token_normalization_sanitizes_values_and_turns_empty_into_clear() {
        let tokens = normalize_metadata_tokens(std::collections::HashMap::from([
            ("summary".into(), Some("  review\nready  ".into())),
            ("empty".into(), Some(" \n ".into())),
            ("clear".into(), None),
        ]))
        .unwrap();

        assert_eq!(tokens["summary"].as_deref(), Some("reviewready"));
        assert_eq!(tokens["empty"], None);
        assert_eq!(tokens["clear"], None);
    }

    #[test]
    fn token_normalization_rejects_invalid_or_unbounded_keys() {
        for key in [
            "bad.name".to_string(),
            "x".repeat(MAX_METADATA_TOKEN_KEY_LEN + 1),
        ] {
            assert!(normalize_metadata_tokens(std::collections::HashMap::from([(
                key,
                Some("value".into()),
            )]))
            .is_err());
        }
        let too_many = (0..=MAX_METADATA_TOKEN_KEYS_PER_REQUEST)
            .map(|index| (format!("key{index}"), Some("value".into())))
            .collect();
        assert!(normalize_metadata_tokens(too_many).is_err());
    }

    /// The reserved `color` key is INTERPRETED by the host, so a bad value has
    /// to fail the request. Validating at render instead would accept the write
    /// and then quietly ignore it, which is how a user concludes the feature is
    /// broken rather than their value.
    #[test]
    fn color_token_accepts_roles_and_literals_and_rejects_the_rest() {
        for good in [
            "accent",
            " Danger ",
            "#ff0000",
            "#f00",
            "rgb(1, 2, 3)",
            "reset",
        ] {
            let tokens = normalize_metadata_tokens(std::collections::HashMap::from([(
                METADATA_COLOR_KEY.to_string(),
                Some(good.to_string()),
            )]))
            .unwrap_or_else(|err| panic!("{good:?} should be accepted: {err}"));
            assert!(tokens[METADATA_COLOR_KEY].is_some(), "{good:?}");
        }

        for bad in ["lightcyan", "chartreuse", "#gg0000", "rgb(300,0,0)"] {
            let err = normalize_metadata_tokens(std::collections::HashMap::from([(
                METADATA_COLOR_KEY.to_string(),
                Some(bad.to_string()),
            )]))
            .expect_err("{bad:?} must be rejected");
            assert!(!err.is_empty(), "the caller is told why: {bad:?}");
        }
    }

    /// Clearing the color is always legal — otherwise a pane tinted by a plugin
    /// that has since been uninstalled could never be un-tinted.
    #[test]
    fn color_token_can_always_be_cleared() {
        let tokens = normalize_metadata_tokens(std::collections::HashMap::from([(
            METADATA_COLOR_KEY.to_string(),
            None,
        )]))
        .expect("clearing is legal");
        assert_eq!(tokens[METADATA_COLOR_KEY], None);
    }

    /// The strict parser exists because the lenient one answers cyan to
    /// everything, which is right for a config file and wrong for an API.
    #[test]
    fn strict_and_lenient_color_parsers_differ_only_on_bad_input() {
        use ratatui::style::Color;
        assert_eq!(
            crate::config::theme::try_parse_color("#ff0000"),
            Ok(Color::Rgb(255, 0, 0))
        );
        assert!(crate::config::theme::try_parse_color("chartreuse").is_err());
        assert_eq!(
            crate::config::theme::parse_color("chartreuse"),
            Color::Cyan,
            "config keeps its warn-and-default behaviour"
        );
    }
}
