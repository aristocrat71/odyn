//! Wall-clock arithmetic for reminders. SQLite is the date library: rusqlite is
//! already here and knows the platform timezone, so no calendar crate is added.
//! These connections are in-memory calculators and store nothing.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

/// Further out than this is a hallucinated year, not a plan.
const MAX_HORIZON_SECS: i64 = 5 * 365 * 24 * 60 * 60;
const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// Unix epoch seconds, the unit every stored time uses.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default()
}

/// The reference point the model needs to resolve `due_at`. Empty when the
/// clock cannot be read, so the prompt omits it rather than stating a wrong one.
pub fn local_now() -> String {
    let read = || -> rusqlite::Result<(String, i64)> {
        Connection::open_in_memory()?.query_row(
            "SELECT strftime('%Y-%m-%d %H:%M', 'now', 'localtime'),
                    CAST(strftime('%w', 'now', 'localtime') AS INTEGER)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    };
    let Ok((stamp, weekday)) = read() else {
        return String::new();
    };
    match usize::try_from(weekday).ok().and_then(|day| DAYS.get(day)) {
        Some(day) => format!("{stamp} ({day})"),
        None => stamp,
    }
}

/// A stored instant as local wall clock, for confirmations.
pub fn local_stamp(at: i64) -> String {
    let read = || -> rusqlite::Result<String> {
        Connection::open_in_memory()?.query_row(
            "SELECT strftime('%Y-%m-%d %H:%M', ?1, 'unixepoch', 'localtime')",
            [at],
            |row| row.get(0),
        )
    };
    read().unwrap_or_default()
}

/// `in_minutes` wins when both are given: needing no reference clock, it
/// cannot land on the wrong day or year.
pub fn resolve_due(now: i64, in_minutes: Option<i64>, due_at: Option<&str>) -> Result<i64, String> {
    if let Some(minutes) = in_minutes {
        if minutes <= 0 {
            return Err("`in_minutes` must be a positive whole number of minutes".to_string());
        }
        let due = minutes
            .checked_mul(60)
            .and_then(|secs| now.checked_add(secs))
            .ok_or_else(|| "`in_minutes` is too far in the future".to_string())?;
        return horizon(now, due);
    }
    let Some(spec) = due_at else {
        return Err("set_reminder needs either `in_minutes` or `due_at`".to_string());
    };
    let due = parse_local(spec).ok_or_else(|| {
        format!("could not read `{spec}` as a date and time; use YYYY-MM-DD HH:MM")
    })?;
    horizon(now, due)
}

fn horizon(now: i64, due: i64) -> Result<i64, String> {
    if due <= now {
        return Err("that time has already passed".to_string());
    }
    if due - now > MAX_HORIZON_SECS {
        return Err("that is more than five years away".to_string());
    }
    Ok(due)
}

/// Local unless the string carries its own zone. A shape SQLite does not
/// recognise yields NULL, not an error.
fn parse_local(spec: &str) -> Option<i64> {
    let spec = spec.trim();
    // A bare date means midnight, which is never what a reminder meant.
    if !spec.contains(':') {
        return None;
    }
    let sql = if zoned(spec) {
        "SELECT CAST(strftime('%s', ?1) AS INTEGER)"
    } else {
        "SELECT CAST(strftime('%s', ?1, 'utc') AS INTEGER)"
    };
    Connection::open_in_memory()
        .ok()?
        .query_row(sql, [spec], |row| row.get::<_, Option<i64>>(0))
        .ok()
        .flatten()
}

/// Reinterpreting an offset-bearing string as local would shift it twice.
fn zoned(spec: &str) -> bool {
    if spec.ends_with('Z') || spec.ends_with('z') {
        return true;
    }
    let tail: String = spec
        .chars()
        .skip(spec.chars().count().saturating_sub(6))
        .collect();
    (tail.starts_with('+') || tail.starts_with('-')) && tail.contains(':')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-08-10 12:00:00 UTC, an arbitrary fixed "now".
    const NOW: i64 = 1_786_363_200;

    #[test]
    fn relative_minutes_land_on_the_minute() {
        assert_eq!(resolve_due(NOW, Some(20), None), Ok(NOW + 1_200));
    }

    #[test]
    fn relative_minutes_win_over_a_written_date() {
        let due = resolve_due(NOW, Some(5), Some("2030-01-01 09:00"));
        assert_eq!(due, Ok(NOW + 300));
    }

    #[test]
    fn a_zoned_string_is_not_shifted_twice() {
        let utc = parse_local("2026-08-10T13:00:00Z");
        let offset = parse_local("2026-08-10T14:00:00+01:00");
        assert_eq!(utc, Some(NOW + 3_600));
        assert_eq!(offset, utc);
    }

    #[test]
    fn nonsense_and_past_times_come_back_as_readable_errors() {
        assert!(resolve_due(NOW, None, Some("next tuesday-ish")).is_err());
        assert!(resolve_due(NOW, None, Some("2020-01-01 09:00")).is_err());
        assert!(resolve_due(NOW, Some(0), None).is_err());
        assert!(resolve_due(NOW, None, None).is_err());
        assert!(resolve_due(NOW, None, Some("2027-01-01")).is_err());
    }

    #[test]
    fn a_hallucinated_year_is_refused() {
        assert!(resolve_due(NOW, None, Some("9999-01-01 09:00")).is_err());
        assert!(resolve_due(NOW, Some(i64::MAX), None).is_err());
    }

    #[test]
    fn the_clock_reads_as_a_dated_weekday() {
        let now = local_now();
        assert!(now.ends_with(')'), "{now}");
        assert!(DAYS.iter().any(|day| now.contains(day)), "{now}");
    }
}
