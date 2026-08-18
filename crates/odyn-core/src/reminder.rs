//! Wall-clock arithmetic for reminders. SQLite is the date library: rusqlite is
//! already here and knows the platform timezone, so no calendar crate is added.
//! These connections are in-memory calculators and store nothing.

use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};

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

/// A parsed `every`-phrase. Stored as its canonical text and re-parsed to
/// re-arm, so the row stays readable in the database and the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Repeat {
    Minutes(i64),
    /// `HH:MM`, zero-padded.
    Daily(String),
    /// strftime `%w` weekday and `HH:MM`.
    Weekly(usize, String),
}

impl Repeat {
    pub fn canonical(&self) -> String {
        match self {
            Self::Minutes(minutes) => format!("every {minutes}m"),
            Self::Daily(clock) => format!("every day {clock}"),
            Self::Weekly(day, clock) => format!("every {} {clock}", DAYS[*day].to_lowercase()),
        }
    }
}

const REPEAT_SHAPES: &str =
    "a repeat reads as `every day 09:00`, `every monday 9:30`, or `every 45m`";

pub fn parse_repeat(spec: &str) -> Result<Repeat, String> {
    let spec = spec.trim().to_lowercase();
    let rest = spec.strip_prefix("every ").ok_or(REPEAT_SHAPES)?;
    let words: Vec<&str> = rest.split_whitespace().collect();
    match words.as_slice() {
        [interval] => interval
            .strip_suffix('m')
            .and_then(|count| count.parse().ok())
            .filter(|minutes: &i64| {
                *minutes > 0
                    && minutes
                        .checked_mul(60)
                        .is_some_and(|secs| secs <= MAX_HORIZON_SECS)
            })
            .map(Repeat::Minutes)
            .ok_or_else(|| REPEAT_SHAPES.to_string()),
        [day, time] => {
            let clock = clock(time).ok_or(REPEAT_SHAPES)?;
            if *day == "day" {
                return Ok(Repeat::Daily(clock));
            }
            DAYS.iter()
                .position(|name| name.eq_ignore_ascii_case(day))
                .map(|index| Repeat::Weekly(index, clock))
                .ok_or_else(|| REPEAT_SHAPES.to_string())
        }
        _ => Err(REPEAT_SHAPES.to_string()),
    }
}

/// `9:30` and `09:30` both read; anything off the clock face does not.
fn clock(time: &str) -> Option<String> {
    let (hour, minute) = time.split_once(':')?;
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = minute.parse().ok().filter(|_| minute.len() == 2)?;
    (hour <= 23 && minute <= 59).then(|| format!("{hour:02}:{minute:02}"))
}

/// The next firing strictly after `now`. Always measured from now, so a
/// reminder slept through fires once and re-arms without a backlog.
pub fn next_fire(repeat: &Repeat, now: i64) -> Option<i64> {
    match repeat {
        Repeat::Minutes(minutes) => now.checked_add(minutes.checked_mul(60)?),
        Repeat::Daily(clock) => at_clock(now, clock, None),
        Repeat::Weekly(day, clock) => at_clock(now, clock, Some(*day)),
    }
}

/// The first local `clock` ahead of `now`, on the wanted weekday when one is
/// given. SQLite does the calendar work, as everywhere in this module.
fn at_clock(now: i64, clock: &str, day: Option<usize>) -> Option<i64> {
    let conn = Connection::open_in_memory().ok()?;
    for offset in 0..=7 {
        let read = conn.query_row(
            "SELECT CAST(strftime('%s', date(?1, 'unixepoch', 'localtime', ?2 || ' days')
                                       || ' ' || ?3, 'utc') AS INTEGER),
                    CAST(strftime('%w', ?1, 'unixepoch', 'localtime', ?2 || ' days') AS INTEGER)",
            params![now, offset, clock],
            |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, i64>(1)?)),
        );
        let Ok((Some(at), weekday)) = read else {
            return None;
        };
        if at > now && day.is_none_or(|day| day as i64 == weekday) {
            return Some(at);
        }
    }
    None
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
    fn an_every_phrase_parses_into_its_canonical_shape() {
        assert_eq!(parse_repeat("every 45m"), Ok(Repeat::Minutes(45)));
        assert_eq!(
            parse_repeat("  Every Day 9:00 "),
            Ok(Repeat::Daily("09:00".to_string()))
        );
        assert_eq!(
            parse_repeat("every Monday 9:30"),
            Ok(Repeat::Weekly(1, "09:30".to_string()))
        );
        assert_eq!(
            parse_repeat("every monday 9:30").unwrap().canonical(),
            "every monday 09:30"
        );
        for bad in [
            "every",
            "daily",
            "every day",
            "every fortnight 09:00",
            "every day 25:00",
            "every day 9:3",
            "every 0m",
            "every 99999999999m",
        ] {
            assert!(parse_repeat(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_next_firing_is_always_measured_from_now() {
        let by_minutes = next_fire(&Repeat::Minutes(45), NOW);
        assert_eq!(by_minutes, Some(NOW + 45 * 60));

        let now = now_secs();
        let daily = next_fire(&Repeat::Daily("09:00".to_string()), now).expect("daily");
        assert!(daily > now && daily <= now + 2 * 86_400, "{daily}");
        assert!(
            local_stamp(daily).ends_with("09:00"),
            "{}",
            local_stamp(daily)
        );

        let weekly = next_fire(&Repeat::Weekly(1, "09:30".to_string()), now).expect("weekly");
        assert!(weekly > now && weekly <= now + 8 * 86_400, "{weekly}");
        let weekday: i64 = Connection::open_in_memory()
            .expect("conn")
            .query_row(
                "SELECT CAST(strftime('%w', ?1, 'unixepoch', 'localtime') AS INTEGER)",
                [weekly],
                |row| row.get(0),
            )
            .expect("weekday");
        assert_eq!(weekday, 1);
    }

    #[test]
    fn the_clock_reads_as_a_dated_weekday() {
        let now = local_now();
        assert!(now.ends_with(')'), "{now}");
        assert!(DAYS.iter().any(|day| now.contains(day)), "{now}");
    }
}
