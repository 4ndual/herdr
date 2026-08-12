//! Small clock/time helpers. Kept separate so source adapters can be unit-tested
//! with injected `now`/`today` values while production reads the real clock.
//!
//! Ported verbatim from usage-dash `src/util.rs`.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Local calendar date `YYYY-MM-DD` (std alone has no local-time API).
///
/// This used to shell out to `date +%Y-%m-%d`. The flags were portable, but the
/// spawn was not free: this feeds the daily-budget key that every tick reads, so
/// the dash forked a process on a path that chrono answers in-process.
pub fn today_local() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Age in seconds of a file's last modification, vs now.
pub fn file_age_secs(path: &std::path::Path) -> u64 {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return u64::MAX,
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    (now_unix() - mtime).max(0) as u64
}

/// Parse an RFC3339/ISO-8601 UTC timestamp ("2026-06-17T16:40:00.835972+00:00")
/// to unix seconds. Drops fractional seconds and the offset (the usage API
/// always returns UTC `+00:00`). Returns None on anything unparseable.
///
/// The calendar math used to be delegated to `date -u -j -f …`, which is the
/// BSD/macOS spelling: GNU `date` rejects `-j`, so on Linux every timestamp
/// silently parsed as `None` and the 5h/weekly rows lost their reset times.
/// chrono does the same job on every platform, and without a process spawn.
pub fn iso2epoch_utc(s: &str) -> Option<i64> {
    // keep up to the seconds field: "YYYY-MM-DDTHH:MM:SS"
    let core: String = s
        .chars()
        .take_while(|&c| c != '.' && c != '+' && c != 'Z')
        .collect();
    if core.len() < 19 {
        return None;
    }
    chrono::NaiveDateTime::parse_from_str(&core, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|dt| dt.and_utc().timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Epochs verified independently, not read back from this function. These
    /// pin the calendar math that used to live in `date(1)` — the whole reason
    /// the reset labels silently blanked on Linux.
    #[test]
    fn iso2epoch_utc_converts_known_timestamps() {
        assert_eq!(iso2epoch_utc("2026-01-01T00:00:00Z"), Some(1_767_225_600));
        // Leap day: the case a hand-rolled day count gets wrong first.
        assert_eq!(iso2epoch_utc("2024-02-29T12:00:00Z"), Some(1_709_208_000));
        // Century leap year (2000 IS a leap year; 1900 was not).
        assert_eq!(iso2epoch_utc("2000-02-29T00:00:00Z"), Some(951_782_400));
        // The real shape the usage API returns: fractional seconds + `+00:00`.
        assert_eq!(
            iso2epoch_utc("2026-06-17T16:40:00.835972+00:00"),
            Some(1_781_714_400)
        );
    }

    #[test]
    fn iso2epoch_utc_rejects_what_it_cannot_parse() {
        assert_eq!(iso2epoch_utc(""), None);
        assert_eq!(iso2epoch_utc("2026-06-17"), None); // no time part
        assert_eq!(iso2epoch_utc("not-a-timestamp-at-all"), None);
        // A date that does not exist must not be silently rounded into one.
        assert_eq!(iso2epoch_utc("2026-02-30T00:00:00Z"), None);
    }

    #[test]
    fn today_local_is_a_calendar_date() {
        let t = today_local();
        assert_eq!(t.len(), 10, "YYYY-MM-DD");
        let parts: Vec<&str> = t.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())));
    }
}
