//! A tiny UTC timestamp formatter (RFC 3339, second precision) built on
//! `SystemTime` so we don't pull in `chrono` just for one line of output.
//!
//! Produces e.g. `2026-07-30T11:42:07Z`. Used to stamp each run so successive
//! runs can be diffed and ordered.

use std::time::{SystemTime, UNIX_EPOCH};

/// Current UTC time as an RFC 3339 string with second precision.
pub fn now_rfc3339() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_epoch_secs(secs)
}

/// Format Unix epoch seconds as `YYYY-MM-DDThh:mm:ssZ` (UTC).
///
/// Uses the civil-from-days algorithm (Howard Hinnant) for the date part —
/// correct across leap years without a calendar library.
fn format_epoch_secs(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let secs_of_day = epoch.rem_euclid(86_400);

    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Parse a `YYYY-MM-DDThh:mm:ssZ` UTC timestamp back to Unix epoch seconds.
/// Only the exact shape this module produces is supported; returns `None` on
/// anything else. Used to compute durations between two run timestamps.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    // Expect exactly: YYYY-MM-DDThh:mm:ssZ (20 chars).
    let b = s.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':'
        || b[16] != b':' || b[19] != b'Z'
    {
        return None;
    }
    let num = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let minute = num(14, 16)?;
    let second = num(17, 19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let days = days_from_civil(year, month as u32, day as u32);
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Inverse of `civil_from_days`: days since 1970-01-01 for a (y, m, d).
/// Ported from Howard Hinnant's public-domain `days_from_civil`.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64; // [0, 399]
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as i64 + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Convert a count of days since 1970-01-01 into (year, month, day).
/// Ported from Howard Hinnant's public-domain `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_known_epochs() {
        assert_eq!(format_epoch_secs(0), "1970-01-01T00:00:00Z");
        // 2001-09-09T01:46:40Z, the classic 1e9 epoch.
        assert_eq!(format_epoch_secs(1_000_000_000), "2001-09-09T01:46:40Z");
        // A leap day: 2020-02-29T12:00:00Z = 1582977600.
        assert_eq!(format_epoch_secs(1_582_977_600), "2020-02-29T12:00:00Z");
    }

    #[test]
    fn now_has_expected_shape() {
        let s = now_rfc3339();
        assert_eq!(s.len(), 20, "YYYY-MM-DDThh:mm:ssZ is 20 chars");
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "T");
    }

    #[test]
    fn parse_is_inverse_of_format() {
        for epoch in [0_i64, 1_000_000_000, 1_582_977_600, 1_800_000_000] {
            let s = format_epoch_secs(epoch);
            assert_eq!(parse_rfc3339(&s), Some(epoch), "round-trip {s}");
        }
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(parse_rfc3339("not-a-time").is_none());
        assert!(parse_rfc3339("2026-07-30 12:00:00").is_none()); // space not T
        assert!(parse_rfc3339("2026-13-01T00:00:00Z").is_none()); // month 13
    }
}
