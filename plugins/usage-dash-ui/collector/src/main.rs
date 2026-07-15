//! fork(wave-2): out-of-process usage-dash collector.
//!
//! The herdr host runs this once per dashboard tick with `HERDR_FOCUS_JSON` set
//! to the serialized focused-session [`FocusInput`] and `HERDR_PLUGIN_STATE_DIR`
//! set to the plugin's state dir. It resolves the focus to a snapshot via
//! [`usage_dash_core::refresh`] — the exact logic that used to run compiled into
//! the host — and answers on stdout with the sticky snapshot protocol the host
//! parses. All real logic lives in `usage-dash-core`; this binary is only the
//! env-in / stdout-out shell, so a collection fix is a rebuild of this bin with
//! no host rebuild.

use herdr_ui_plugin_api::FocusInput;
use usage_dash_core::RefreshOutcome;

fn main() {
    let focus_json = match std::env::var("HERDR_FOCUS_JSON") {
        Ok(value) => value,
        Err(err) => {
            eprintln!("usage-dash-collector: HERDR_FOCUS_JSON unavailable: {err}");
            std::process::exit(1);
        }
    };
    let focus: FocusInput = match serde_json::from_str(&focus_json) {
        Ok(focus) => focus,
        Err(err) => {
            eprintln!("usage-dash-collector: HERDR_FOCUS_JSON did not parse: {err}");
            std::process::exit(1);
        }
    };

    let outcome = usage_dash_core::refresh(focus);
    match outcome_to_stdout(&outcome) {
        Ok(line) => println!("{line}"),
        Err(err) => {
            eprintln!("usage-dash-collector: failed to encode outcome: {err}");
            std::process::exit(1);
        }
    }
}

/// Map a [`RefreshOutcome`] to the host stdout protocol line:
///   `{"outcome":"replace","snapshot":{…}}` | `{"outcome":"keep"}` | `{"outcome":"clear"}`
fn outcome_to_stdout(outcome: &RefreshOutcome) -> Result<String, serde_json::Error> {
    let value = match outcome {
        RefreshOutcome::Replace(snapshot) => serde_json::json!({
            "outcome": "replace",
            "snapshot": serde_json::to_value(snapshot.as_ref())?,
        }),
        RefreshOutcome::KeepPrevious => serde_json::json!({ "outcome": "keep" }),
        RefreshOutcome::Clear => serde_json::json!({ "outcome": "clear" }),
    };
    serde_json::to_string(&value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use usage_dash_core::{Cli, UsageSnapshot};

    #[test]
    fn keep_and_clear_map_to_protocol() {
        assert_eq!(
            outcome_to_stdout(&RefreshOutcome::KeepPrevious).unwrap(),
            r#"{"outcome":"keep"}"#
        );
        assert_eq!(
            outcome_to_stdout(&RefreshOutcome::Clear).unwrap(),
            r#"{"outcome":"clear"}"#
        );
    }

    #[test]
    fn replace_carries_serialized_snapshot() {
        let snapshot = UsageSnapshot::empty(Cli::Claude);
        let line = outcome_to_stdout(&RefreshOutcome::Replace(Box::new(snapshot.clone()))).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["outcome"], "replace");
        // The snapshot round-trips back into a UsageSnapshot (what the host does).
        let back: UsageSnapshot = serde_json::from_value(value["snapshot"].clone()).unwrap();
        assert_eq!(
            serde_json::to_value(&back).unwrap(),
            serde_json::to_value(&snapshot).unwrap()
        );
    }
}
