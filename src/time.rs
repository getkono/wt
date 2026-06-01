//! Time formatting for display (spec §7 "Display conventions").
//!
//! Commit timestamps are stored as Unix seconds. [`iso8601`] renders the stable
//! `--json` form (`2024-01-15T10:30:00Z`); [`relative`] renders the human form
//! as a single largest unit (`just now`, `5m ago`, `3h ago`, `2d ago`,
//! `4mo ago`, `1y ago`).

use jiff::Timestamp;

const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
const MONTH: i64 = 30 * DAY;
const YEAR: i64 = 365 * DAY;

/// Formats Unix seconds as an ISO-8601 UTC string (`YYYY-MM-DDTHH:MM:SSZ`).
pub fn iso8601(unix_seconds: i64) -> String {
    match Timestamp::from_second(unix_seconds) {
        Ok(ts) => ts.strftime("%Y-%m-%dT%H:%M:%SZ").to_string(),
        Err(_) => "(invalid time)".to_string(),
    }
}

/// Renders the gap from `then_unix` to `now_unix` as a single largest unit.
/// Times in the future (or under a minute old) render as `just now`.
pub fn relative(now_unix: i64, then_unix: i64) -> String {
    let secs = now_unix - then_unix;
    if secs < MINUTE {
        return "just now".to_string();
    }
    let (value, unit) = if secs < HOUR {
        (secs / MINUTE, "m")
    } else if secs < DAY {
        (secs / HOUR, "h")
    } else if secs < MONTH {
        (secs / DAY, "d")
    } else if secs < YEAR {
        (secs / MONTH, "mo")
    } else {
        (secs / YEAR, "y")
    };
    format!("{value}{unit} ago")
}

/// The current time as Unix seconds (the reference for [`relative`]).
pub fn now_unix() -> i64 {
    Timestamp::now().as_second()
}

/// Parses an ISO-8601 timestamp (as produced by [`iso8601`]) back to Unix
/// seconds, for computing relative display from the stored value.
pub fn parse_iso8601(text: &str) -> Option<i64> {
    text.parse::<Timestamp>().ok().map(|t| t.as_second())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_formats_known_instant() {
        // 2024-01-15T10:30:00Z == 1705314600 seconds since the epoch.
        assert_eq!(iso8601(1_705_314_600), "2024-01-15T10:30:00Z");
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn relative_units_pick_largest() {
        assert_eq!(relative(1_000, 1_000), "just now");
        assert_eq!(relative(1_000 + 30, 1_000), "just now");
        assert_eq!(relative(1_000 + 5 * MINUTE, 1_000), "5m ago");
        assert_eq!(relative(1_000 + 3 * HOUR, 1_000), "3h ago");
        assert_eq!(relative(1_000 + 2 * DAY, 1_000), "2d ago");
        assert_eq!(relative(1_000 + 4 * MONTH, 1_000), "4mo ago");
        assert_eq!(relative(1_000 + YEAR, 1_000), "1y ago");
        assert_eq!(relative(1_000 + 3 * YEAR, 1_000), "3y ago");
    }

    #[test]
    fn relative_boundaries() {
        assert_eq!(relative(59, 0), "just now");
        assert_eq!(relative(60, 0), "1m ago");
        assert_eq!(relative(HOUR - 1, 0), "59m ago");
        assert_eq!(relative(HOUR, 0), "1h ago");
        assert_eq!(relative(DAY - 1, 0), "23h ago");
        assert_eq!(relative(DAY, 0), "1d ago");
        assert_eq!(relative(MONTH - 1, 0), "29d ago");
        assert_eq!(relative(MONTH, 0), "1mo ago");
        assert_eq!(relative(YEAR, 0), "1y ago");
    }

    #[test]
    fn future_times_are_just_now() {
        assert_eq!(relative(0, 1_000), "just now");
    }

    #[test]
    fn now_unix_is_after_2020() {
        assert!(now_unix() > 1_600_000_000);
    }

    #[test]
    fn iso8601_round_trips_through_parse() {
        for unix in [0, 1_705_314_600, 1_600_000_000] {
            assert_eq!(parse_iso8601(&iso8601(unix)), Some(unix));
        }
        assert_eq!(parse_iso8601("not a timestamp"), None);
    }
}
