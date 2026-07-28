//! Daily-budget tracking: how many percentage points of the 100% weekly cap
//! were consumed *today*. Target is to stay under `DAILY_BUDGET_PCT` (12%).
//!
//! We snapshot the weekly window's `used_pct` at the first reading of each
//! local day (the "baseline"); today's delta = current - baseline. A weekly
//! reset (current < baseline) re-baselines to current and reports 0.
//!
//! Keyed per backend: "claude" (its own weekly cap) and "codex" (shared by
//! Codex and pi, same OpenAI plan).

use std::fs;
use std::path::PathBuf;

fn budget_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude/dash2/budget")
}

/// Pure fold: given the stored `(date, weekly_pct)` baseline, today's date, and
/// the current weekly_pct, return `(delta_pct, new_baseline)`.
pub fn fold(stored: Option<(String, f32)>, today: &str, current: f32) -> (f32, (String, f32)) {
    match stored {
        Some((date, base)) if date == today && current >= base => (current - base, (date, base)),
        // new day, missing baseline, or a weekly reset → re-baseline to current
        _ => (0.0, (today.to_string(), current)),
    }
}

/// Read the baseline for `key`, fold against the current weekly_pct, persist the
/// (possibly new) baseline, and return today's delta in weekly-cap points.
pub fn daily_delta(key: &str, today: &str, current_weekly_pct: f32) -> f32 {
    let path = budget_dir().join(format!("{key}.json"));
    let stored = fs::read_to_string(&path).ok().and_then(|s| {
        let v: serde_json::Value = serde_json::from_str(&s).ok()?;
        let d = v.get("date")?.as_str()?.to_string();
        let p = v.get("weekly_pct")?.as_f64()? as f32;
        Some((d, p))
    });
    let (delta, (ndate, npct)) = fold(stored, today, current_weekly_pct);
    let _ = fs::create_dir_all(budget_dir());
    let out = serde_json::json!({ "date": ndate, "weekly_pct": npct });
    // atomic-ish write (tmp + rename)
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, out.to_string()).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }
    delta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_day_reports_delta() {
        let (d, _) = fold(Some(("2026-06-17".into(), 90.0)), "2026-06-17", 96.0);
        assert!((d - 6.0).abs() < 1e-4);
    }

    #[test]
    fn new_day_rebaselines_zero() {
        let (d, b) = fold(Some(("2026-06-16".into(), 90.0)), "2026-06-17", 96.0);
        assert_eq!(d, 0.0);
        assert_eq!(b, ("2026-06-17".into(), 96.0));
    }

    #[test]
    fn weekly_reset_rebaselines_zero() {
        // current dropped below baseline => the weekly window reset
        let (d, b) = fold(Some(("2026-06-17".into(), 96.0)), "2026-06-17", 3.0);
        assert_eq!(d, 0.0);
        assert_eq!(b, ("2026-06-17".into(), 3.0));
    }

    #[test]
    fn missing_baseline_zero() {
        let (d, b) = fold(None, "2026-06-17", 40.0);
        assert_eq!(d, 0.0);
        assert_eq!(b.1, 40.0);
    }
}
