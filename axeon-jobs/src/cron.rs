//! Cron expressions for recurring jobs.

use chrono::{DateTime, Datelike, Duration as ChronoDuration, NaiveDate, NaiveDateTime, Timelike, Utc};
use std::fmt;
use std::str::FromStr;

/// A standard five-field cron expression, evaluated in UTC:
///
/// ```text
/// ┌──────── minute (0-59)
/// │ ┌────── hour (0-23)
/// │ │ ┌──── day of the month (1-31)
/// │ │ │ ┌── month (1-12 or JAN-DEC)
/// │ │ │ │ ┌ day of the week (0-7 or SUN-SAT; 0 and 7 are Sunday)
/// * * * * *
/// ```
///
/// Each field takes `*`, a value, a range `a-b`, a step `*/n`, `a-b/n` or
/// `a/n`, or a comma-separated list of those. As in Vixie cron, when both
/// the day of the month and the day of the week are restricted, a day
/// matching either one counts. `@yearly`, `@monthly`, `@weekly`, `@daily`
/// (or `@midnight`) and `@hourly` work too.
///
/// ```
/// use axeon_jobs::Cron;
/// use chrono::{TimeZone, Utc};
///
/// let nightly: Cron = "0 3 * * *".parse().unwrap();
/// let now = Utc.with_ymd_and_hms(2026, 11, 30, 14, 5, 0).unwrap();
/// assert_eq!(nightly.next_after(now), Some(Utc.with_ymd_and_hms(2026, 12, 1, 3, 0, 0).unwrap()));
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct Cron {
    source: String,
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    weekdays: u64,
    any_day: bool,
    any_weekday: bool,
}

/// An invalid [`Cron`] expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronError(String);

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CronError {}

const MONTHS: [&str; 12] = ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"];
const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

impl Cron {
    /// Parses an expression; see [`Cron`] for the syntax.
    pub fn parse(expression: &str) -> Result<Cron, CronError> {
        let source = expression.trim();
        let expanded = match source.to_ascii_lowercase().as_str() {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            _ => source,
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields[..] else {
            return Err(CronError(format!("{:?} should have 5 fields, not {}", source, fields.len())));
        };
        let mut weekdays = field(weekday, 0, 7, &WEEKDAYS, "day of the week")?;
        if weekdays & (1 << 7) != 0 {
            weekdays = (weekdays | 1) & !(1 << 7); // 7 is Sunday too.
        }
        Ok(Cron {
            source: source.to_string(),
            minutes: field(minute, 0, 59, &[], "minute")?,
            hours: field(hour, 0, 23, &[], "hour")?,
            days: field(day, 1, 31, &[], "day of the month")?,
            months: field(month, 1, 12, &MONTHS, "month")?,
            weekdays,
            any_day: day.starts_with('*'),
            any_weekday: weekday.starts_with('*'),
        })
    }

    /// The expression as written.
    pub fn as_str(&self) -> &str {
        &self.source
    }

    fn day_matches(&self, date: NaiveDate) -> bool {
        let day = self.days & (1 << date.day()) != 0;
        let weekday = self.weekdays & (1 << date.weekday().num_days_from_sunday()) != 0;
        match (self.any_day, self.any_weekday) {
            (false, false) => day || weekday,
            _ => day && weekday,
        }
    }

    /// The first matching minute strictly after `after`, or `None` if the
    /// expression never matches (e.g. `0 0 30 2 *`).
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let start = after.naive_utc().with_second(0)?.with_nanosecond(0)? + ChronoDuration::minutes(1);
        let mut date = start.date();
        let mut first_day = true;
        // Every pattern that matches at all matches within 8 years (Feb 29
        // on a given weekday, say), so this bounds the search.
        for _ in 0..(366 * 8) {
            if self.months & (1 << date.month()) != 0 && self.day_matches(date) {
                let (from_hour, from_minute) = if first_day { (start.hour(), start.minute()) } else { (0, 0) };
                if let Some(time) = self.time_from(from_hour, from_minute) {
                    return Some(NaiveDateTime::new(date, time).and_utc());
                }
            }
            date = date.succ_opt()?;
            first_day = false;
        }
        None
    }

    /// The first matching time of day at or after `hour:minute`.
    fn time_from(&self, hour: u32, minute: u32) -> Option<chrono::NaiveTime> {
        for h in hour..24 {
            if self.hours & (1 << h) == 0 {
                continue;
            }
            let from = if h == hour { minute } else { 0 };
            if let Some(m) = (from..60).find(|m| self.minutes & (1 << m) != 0) {
                return chrono::NaiveTime::from_hms_opt(h, m, 0);
            }
        }
        None
    }
}

impl FromStr for Cron {
    type Err = CronError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Cron::parse(s)
    }
}

impl fmt::Debug for Cron {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Cron({:?})", self.source)
    }
}

impl fmt::Display for Cron {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

/// One field as a bit set of the values it allows.
fn field(text: &str, min: u32, max: u32, names: &[&str], what: &str) -> Result<u64, CronError> {
    let error = || CronError(format!("invalid {} {:?} (allowed: {}-{})", what, text, min, max));
    let value = |s: &str| -> Result<u32, CronError> {
        let upper = s.to_ascii_uppercase();
        let named = names.iter().position(|n| *n == upper).map(|i| i as u32 + if names.len() == 12 { 1 } else { 0 });
        let v = named.or_else(|| s.parse().ok()).ok_or_else(error)?;
        if (min..=max).contains(&v) {
            Ok(v)
        } else {
            Err(error())
        }
    };
    let mut bits = 0u64;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (range, step.parse::<u32>().ok().filter(|s| *s > 0).ok_or_else(error)?),
            None => (part, 1),
        };
        let (start, end) = match range {
            "*" => (min, max),
            _ => match range.split_once('-') {
                Some((a, b)) => (value(a)?, value(b)?),
                // `a/n` runs from a to the maximum.
                None if part.contains('/') => (value(range)?, max),
                None => {
                    let v = value(range)?;
                    (v, v)
                }
            },
        };
        if start > end {
            return Err(error());
        }
        for v in (start..=end).step_by(step as usize) {
            bits |= 1 << v;
        }
    }
    Ok(bits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn next(expression: &str, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        Cron::parse(expression).unwrap().next_after(after)
    }

    #[test]
    fn next_runs() {
        let now = Utc.with_ymd_and_hms(2026, 10, 22, 14, 37, 12).unwrap();
        assert_eq!(next("* * * * *", now), Some(at(2026, 10, 22, 14, 38)));
        assert_eq!(next("*/15 * * * *", now), Some(at(2026, 10, 22, 14, 45)));
        assert_eq!(next("0 3 * * *", now), Some(at(2026, 10, 23, 3, 0)));
        assert_eq!(next("@hourly", now), Some(at(2026, 10, 22, 15, 0)));
        assert_eq!(next("@monthly", now), Some(at(2026, 11, 1, 0, 0)));
        assert_eq!(next("@yearly", now), Some(at(2027, 1, 1, 0, 0)));
        assert_eq!(next("30 9 * * MON-FRI", now), Some(at(2026, 10, 23, 9, 30)));
        assert_eq!(next("0 0 * * SUN", now), Some(at(2026, 10, 25, 0, 0)));
        assert_eq!(next("0 0 * * 7", now), Some(at(2026, 10, 25, 0, 0)));
        assert_eq!(next("0 12 1,15 * *", now), Some(at(2026, 11, 1, 12, 0)));
        assert_eq!(next("0 0 29 FEB *", now), Some(at(2028, 2, 29, 0, 0)));
        assert_eq!(next("5/20 8-10 * * *", now), Some(at(2026, 10, 23, 8, 5)));
        // Day of month OR day of week when both are restricted.
        assert_eq!(next("0 0 13 * FRI", now), Some(at(2026, 10, 23, 0, 0)));
        // Exactly on a match: strictly after.
        assert_eq!(next("37 14 * * *", at(2026, 10, 22, 14, 37)), Some(at(2026, 10, 23, 14, 37)));
        // Year end.
        assert_eq!(next("59 23 31 12 *", at(2026, 12, 31, 23, 59)), Some(at(2027, 12, 31, 23, 59)));
        // Never.
        assert_eq!(next("0 0 30 2 *", now), None);
    }

    #[test]
    fn rejects_bad_expressions() {
        for bad in ["", "* * * *", "* * * * * *", "60 * * * *", "* 24 * * *", "* * 0 * *", "* * * 13 *", "* * * * 8", "*/0 * * * *", "5-1 * * * *", "x * * * *", "1-2-3 * * * *"] {
            assert!(Cron::parse(bad).is_err(), "{:?}", bad);
        }
        assert_eq!(Cron::parse(" 0 3 * * * ").unwrap().as_str(), "0 3 * * *");
    }
}
