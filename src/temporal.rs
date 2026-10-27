//! openCypher temporal types backed by chrono primitives.
//!
//! Provides `CypherDate`, `CypherLocalTime`, `CypherTime`, `CypherLocalDateTime`,
//! `CypherDateTime`, and `CypherDuration` with ISO 8601 parsing, display, and
//! MessagePack-compatible serde via integer fields.

use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};

use chrono::{
    Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeZone, Timelike,
};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::types::{GraphError, QueryPhase, Result, Value};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract an `i64` from a `Value::I64` in a map.
fn get_i64(map: &BTreeMap<String, Value>, key: &str) -> Option<i64> {
    match map.get(key) {
        Some(Value::I64(n)) => Some(*n),
        _ => None,
    }
}

/// An `InvalidArgumentValue` error for a bad temporal map component.
fn invalid_map(message: impl fmt::Display) -> GraphError {
    GraphError::invalid_argument_value(QueryPhase::Runtime, "temporal map", message)
}

/// An `InvalidArgumentValue` error for a temporal string that fails to parse.
/// The message names the problem, never the offending input (decision D10).
fn parse_error(message: impl fmt::Display) -> GraphError {
    GraphError::invalid_argument_value(QueryPhase::Runtime, "temporal", message)
}

/// The `NumberOutOfRange` error for a temporal value that does not fit.
fn out_of_range(what: impl fmt::Display) -> GraphError {
    GraphError::number_out_of_range(QueryPhase::Runtime, format!("{what} is out of range"))
}

/// Read an integer map component and narrow it to `T`. A value that does not
/// fit is an error: an `as` cast would silently wrap (hour 2^32 + 1 -> 1).
fn get_int<T: TryFrom<i64>>(map: &BTreeMap<String, Value>, key: &str) -> Result<Option<T>> {
    match get_i64(map, key) {
        None => Ok(None),
        Some(n) => T::try_from(n)
            .map(Some)
            .map_err(|_| invalid_map(format!("{key} {n} is out of range"))),
    }
}

/// Keys accepted by the date part of a temporal map.
const DATE_KEYS: &[&str] = &[
    "year",
    "month",
    "day",
    "week",
    "dayOfWeek",
    "ordinalDay",
    "quarter",
    "dayOfQuarter",
];

/// Keys accepted by the time part of a temporal map.
const TIME_KEYS: &[&str] = &[
    "hour",
    "minute",
    "second",
    "millisecond",
    "microsecond",
    "nanosecond",
];

/// Reject map keys outside `groups`, so a typo like `{yaer: 2024}` is an
/// error instead of a silently defaulted component.
fn check_keys(map: &BTreeMap<String, Value>, func: &str, groups: &[&[&str]]) -> Result<()> {
    for key in map.keys() {
        if !groups.iter().any(|g| g.contains(&key.as_str())) {
            return Err(GraphError::invalid_argument_value(
                QueryPhase::Runtime,
                func,
                format!("unknown key '{key}' in temporal map"),
            ));
        }
    }
    Ok(())
}

/// Reject temporal strings containing non-ASCII characters.
///
/// Every valid ISO 8601 temporal string (including IANA zone names) is pure
/// ASCII, and the parsers below slice at fixed byte offsets. Checking once up
/// front guarantees every byte offset is a char boundary, so a multi-byte
/// character can never make a slice panic; it fails here with a parse error.
fn ensure_ascii(s: &str, what: &str) -> Result<()> {
    if s.is_ascii() {
        return Ok(());
    }
    Err(parse_error(format!(
        "invalid {what} string: contains non-ASCII characters"
    )))
}

/// Parse a timezone offset string into a `FixedOffset`.
fn parse_offset(s: &str) -> Result<FixedOffset> {
    ensure_ascii(s, "offset")?;
    if s == "Z" || s == "z" {
        return Ok(FixedOffset::east_opt(0).unwrap());
    }
    let sign: i32 = if s.starts_with('+') {
        1
    } else if s.starts_with('-') {
        -1
    } else {
        return Err(parse_error("invalid offset"));
    };
    let body = &s[1..];
    let (h, m) = if body.len() == 2 {
        // +HH
        (
            body.parse::<i32>()
                .map_err(|e| parse_error(e.to_string()))?,
            0,
        )
    } else if body.len() == 4 {
        // +HHMM
        let hh = body[..2]
            .parse::<i32>()
            .map_err(|e| parse_error(e.to_string()))?;
        let mm = body[2..]
            .parse::<i32>()
            .map_err(|e| parse_error(e.to_string()))?;
        (hh, mm)
    } else if body.len() == 5 && body.as_bytes()[2] == b':' {
        // +HH:MM
        let hh = body[..2]
            .parse::<i32>()
            .map_err(|e| parse_error(e.to_string()))?;
        let mm = body[3..]
            .parse::<i32>()
            .map_err(|e| parse_error(e.to_string()))?;
        (hh, mm)
    } else if body.len() == 8 && body.as_bytes()[2] == b':' && body.as_bytes()[5] == b':' {
        // +HH:MM:SS — historical offsets with seconds precision
        let hh = body[..2]
            .parse::<i32>()
            .map_err(|e| parse_error(e.to_string()))?;
        let mm = body[3..5]
            .parse::<i32>()
            .map_err(|e| parse_error(e.to_string()))?;
        let ss = body[6..8]
            .parse::<i32>()
            .map_err(|e| parse_error(e.to_string()))?;
        let total = sign * (hh * 3600 + mm * 60 + ss);
        return FixedOffset::east_opt(total).ok_or_else(|| parse_error("offset out of range"));
    } else {
        return Err(parse_error("invalid offset"));
    };
    let secs = sign * (h * 3600 + m * 60);
    FixedOffset::east_opt(secs).ok_or_else(|| parse_error("offset out of range"))
}

/// Split a time string from its trailing timezone offset.
/// Returns `(time_part, Some(offset_str))` or `(time_part, None)`.
fn split_time_offset(s: &str) -> (&str, Option<&str>) {
    // Check for Z suffix
    if s.ends_with('Z') || s.ends_with('z') {
        return (&s[..s.len() - 1], Some(&s[s.len() - 1..]));
    }
    // Find last +/- that indicates offset (not at position 0)
    if let Some(pos) = s.rfind('+') {
        if pos > 0 {
            return (&s[..pos], Some(&s[pos..]));
        }
    }
    if let Some(pos) = s.rfind('-') {
        if pos > 0 {
            return (&s[..pos], Some(&s[pos..]));
        }
    }
    (s, None)
}

/// Parse a time string (without offset) into `NaiveTime`.
fn parse_time_str(s: &str) -> Result<NaiveTime> {
    ensure_ascii(s, "time")?;
    if s.contains(':') {
        // Colon-separated: HH:MM, HH:MM:SS, HH:MM:SS.nnn
        let parts: Vec<&str> = s.splitn(3, ':').collect();
        let h: u32 = parts[0]
            .parse()
            .map_err(|e: std::num::ParseIntError| parse_error(e.to_string()))?;
        let m: u32 = parts
            .get(1)
            .unwrap_or(&"0")
            .parse()
            .map_err(|e: std::num::ParseIntError| parse_error(e.to_string()))?;
        if parts.len() < 3 {
            return NaiveTime::from_hms_opt(h, m, 0).ok_or_else(|| parse_error("invalid time"));
        }
        let sec_part = parts[2];
        if let Some(dot_pos) = sec_part.find('.') {
            let sec: u32 = sec_part[..dot_pos]
                .parse()
                .map_err(|e: std::num::ParseIntError| parse_error(e.to_string()))?;
            let frac = &sec_part[dot_pos + 1..];
            let nano = parse_frac_nanos(frac)?;
            NaiveTime::from_hms_nano_opt(h, m, sec, nano).ok_or_else(|| parse_error("invalid time"))
        } else {
            let sec: u32 = sec_part
                .parse()
                .map_err(|e: std::num::ParseIntError| parse_error(e.to_string()))?;
            NaiveTime::from_hms_opt(h, m, sec).ok_or_else(|| parse_error("invalid time"))
        }
    } else {
        // Compact: HH, HHMM, HHMMSS, HHMMSS.nnn
        let (digits, frac) = if let Some(dot_pos) = s.find('.') {
            (&s[..dot_pos], Some(&s[dot_pos + 1..]))
        } else {
            (s, None)
        };
        let (h, m, sec) = match digits.len() {
            2 => (
                digits
                    .parse::<u32>()
                    .map_err(|e| parse_error(e.to_string()))?,
                0u32,
                0u32,
            ),
            4 => {
                let hh = digits[..2]
                    .parse::<u32>()
                    .map_err(|e| parse_error(e.to_string()))?;
                let mm = digits[2..]
                    .parse::<u32>()
                    .map_err(|e| parse_error(e.to_string()))?;
                (hh, mm, 0)
            }
            6 => {
                let hh = digits[..2]
                    .parse::<u32>()
                    .map_err(|e| parse_error(e.to_string()))?;
                let mm = digits[2..4]
                    .parse::<u32>()
                    .map_err(|e| parse_error(e.to_string()))?;
                let ss = digits[4..]
                    .parse::<u32>()
                    .map_err(|e| parse_error(e.to_string()))?;
                (hh, mm, ss)
            }
            _ => return Err(parse_error("invalid compact time")),
        };
        let nano = if let Some(f) = frac {
            parse_frac_nanos(f)?
        } else {
            0
        };
        NaiveTime::from_hms_nano_opt(h, m, sec, nano).ok_or_else(|| parse_error("invalid time"))
    }
}

/// Parse a string as a given numeric type, wrapping the error in `GraphError`.
fn parse_num<T: std::str::FromStr>(s: &str) -> Result<T>
where
    T::Err: fmt::Display,
{
    s.parse::<T>().map_err(|e| parse_error(e.to_string()))
}

/// Parse fractional seconds string (up to 9 digits) into nanoseconds.
fn parse_frac_nanos(frac: &str) -> Result<u32> {
    ensure_ascii(frac, "fractional seconds")?;
    let mut padded = String::from(frac);
    while padded.len() < 9 {
        padded.push('0');
    }
    padded.truncate(9);
    padded
        .parse::<u32>()
        .map_err(|e| parse_error(e.to_string()))
}

/// Parse a date string into `NaiveDate`.
fn parse_date_str(s: &str) -> Result<NaiveDate> {
    ensure_ascii(s, "date")?;
    // Week date: YYYY-Www-D or YYYYWwwD or YYYY-Www or YYYYWww
    if s.contains('W') {
        return parse_week_date(s);
    }

    if s.contains('-') {
        let parts: Vec<&str> = s.split('-').collect();
        match parts.len() {
            3 => {
                let y: i32 = parts[0]
                    .parse()
                    .map_err(|e: std::num::ParseIntError| parse_error(e.to_string()))?;
                let m: u32 = parts[1]
                    .parse()
                    .map_err(|e: std::num::ParseIntError| parse_error(e.to_string()))?;
                let d: u32 = parts[2]
                    .parse()
                    .map_err(|e: std::num::ParseIntError| parse_error(e.to_string()))?;
                NaiveDate::from_ymd_opt(y, m, d).ok_or_else(|| parse_error("invalid date"))
            }
            2 => {
                let y: i32 = parts[0]
                    .parse()
                    .map_err(|e: std::num::ParseIntError| parse_error(e.to_string()))?;
                let part2: u32 = parts[1]
                    .parse()
                    .map_err(|e: std::num::ParseIntError| parse_error(e.to_string()))?;
                if parts[1].len() == 3 {
                    // Ordinal: YYYY-DDD
                    NaiveDate::from_yo_opt(y, part2)
                        .ok_or_else(|| parse_error("invalid ordinal date"))
                } else {
                    // YYYY-MM (day defaults to 1)
                    NaiveDate::from_ymd_opt(y, part2, 1).ok_or_else(|| parse_error("invalid date"))
                }
            }
            _ => Err(parse_error("invalid date")),
        }
    } else {
        // Compact forms: YYYYMMDD, YYYYMM, YYYY, YYYYDDD
        match s.len() {
            8 => {
                let y: i32 = parse_num(&s[..4])?;
                let m: u32 = parse_num(&s[4..6])?;
                let d: u32 = parse_num(&s[6..])?;
                NaiveDate::from_ymd_opt(y, m, d).ok_or_else(|| parse_error("invalid date"))
            }
            7 => {
                // YYYYDDD
                let y: i32 = parse_num(&s[..4])?;
                let d: u32 = parse_num(&s[4..])?;
                NaiveDate::from_yo_opt(y, d).ok_or_else(|| parse_error("invalid ordinal date"))
            }
            6 => {
                // YYYYMM
                let y: i32 = parse_num(&s[..4])?;
                let m: u32 = parse_num(&s[4..])?;
                NaiveDate::from_ymd_opt(y, m, 1).ok_or_else(|| parse_error("invalid date"))
            }
            4 => {
                let y: i32 = parse_num(s)?;
                NaiveDate::from_ymd_opt(y, 1, 1).ok_or_else(|| parse_error("invalid date"))
            }
            _ => Err(parse_error("invalid date")),
        }
    }
}

/// Parse ISO week date forms.
fn parse_week_date(s: &str) -> Result<NaiveDate> {
    ensure_ascii(s, "week date")?;
    // Strip hyphens for uniform handling, but track original for error messages.
    let compact = s.replace('-', "");
    // Expected: YYYYWww or YYYYWwwD
    let w_pos = compact
        .find('W')
        .ok_or_else(|| parse_error("invalid week date"))?;
    let year: i32 = compact[..w_pos]
        .parse()
        .map_err(|e: std::num::ParseIntError| parse_error(e.to_string()))?;
    let after_w = &compact[w_pos + 1..];
    let (week, day) = if after_w.len() >= 3 {
        let wk: u32 = after_w[..2]
            .parse()
            .map_err(|e: std::num::ParseIntError| parse_error(e.to_string()))?;
        let d: u32 = after_w[2..3]
            .parse()
            .map_err(|e: std::num::ParseIntError| parse_error(e.to_string()))?;
        (wk, d)
    } else if after_w.len() == 2 {
        let wk: u32 = after_w
            .parse()
            .map_err(|e: std::num::ParseIntError| parse_error(e.to_string()))?;
        (wk, 1) // default to Monday
    } else {
        return Err(parse_error("invalid week date"));
    };
    let weekday = match day {
        1 => chrono::Weekday::Mon,
        2 => chrono::Weekday::Tue,
        3 => chrono::Weekday::Wed,
        4 => chrono::Weekday::Thu,
        5 => chrono::Weekday::Fri,
        6 => chrono::Weekday::Sat,
        7 => chrono::Weekday::Sun,
        _ => return Err(parse_error("invalid day of week")),
    };
    NaiveDate::from_isoywd_opt(year, week, weekday).ok_or_else(|| parse_error("invalid week date"))
}

/// Format a `NaiveTime` using the TCK local-time rules:
/// Always HH:MM, add :SS if seconds nonzero, add .nnnnnnnnn (trailing zeros trimmed) if sub-second nonzero.
fn fmt_local_time(t: &NaiveTime, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{:02}:{:02}", t.hour(), t.minute())?;
    let sec = t.second();
    let nano = t.nanosecond();
    if sec != 0 || nano != 0 {
        write!(f, ":{:02}", sec)?;
        if nano != 0 {
            let s = format!("{:09}", nano);
            let trimmed = s.trim_end_matches('0');
            write!(f, ".{trimmed}")?;
        }
    }
    Ok(())
}

/// Format a `FixedOffset` for display. UTC (+00:00) renders as `Z`.
/// Format offset as string (public helper for accessor use).
pub fn fmt_offset_public(off: &FixedOffset) -> String {
    let secs = off.local_minus_utc();
    if secs == 0 {
        "Z".to_string()
    } else {
        let sign = if secs < 0 { '-' } else { '+' };
        let abs = secs.unsigned_abs();
        let h = abs / 3600;
        let m = (abs % 3600) / 60;
        let s = abs % 60;
        if s != 0 {
            format!("{sign}{h:02}:{m:02}:{s:02}")
        } else {
            format!("{sign}{h:02}:{m:02}")
        }
    }
}

fn fmt_offset(off: &FixedOffset, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let secs = off.local_minus_utc();
    if secs == 0 {
        write!(f, "Z")
    } else {
        let sign = if secs < 0 { '-' } else { '+' };
        let abs = secs.unsigned_abs();
        let h = abs / 3600;
        let m = (abs % 3600) / 60;
        let s = abs % 60;
        if s != 0 {
            write!(f, "{sign}{h:02}:{m:02}:{s:02}")
        } else {
            write!(f, "{sign}{h:02}:{m:02}")
        }
    }
}

/// Extract the source offset from a base temporal value in the map (if any).
/// Returns `Some(offset)` when the base `time` or `datetime` key carries a timezone.
fn base_source_offset(map: &BTreeMap<String, Value>) -> Option<FixedOffset> {
    match map.get("time").or_else(|| map.get("datetime")) {
        Some(Value::Time(t)) => Some(t.1),
        Some(Value::DateTime(dt)) => Some(dt.1),
        _ => None,
    }
}

/// Re-resolve the source offset at a new NaiveDateTime when the base temporal
/// has a named timezone. This accounts for DST changes when the date differs
/// from the original temporal.
fn base_source_offset_at(
    map: &BTreeMap<String, Value>,
    ndt: &NaiveDateTime,
) -> Option<FixedOffset> {
    match map.get("time").or_else(|| map.get("datetime")) {
        Some(Value::Time(t)) => Some(t.1),
        Some(Value::DateTime(dt)) => {
            if let Some(ref tz_name) = dt.2 {
                // Re-resolve at the new date/time for DST awareness.
                if let Ok((off, _)) = resolve_tz_name_at(tz_name, ndt) {
                    Some(off)
                } else {
                    Some(dt.1)
                }
            } else {
                Some(dt.1)
            }
        }
        _ => None,
    }
}

/// Build a `NaiveTime` from map keys (hour, minute, second, nanosecond, millisecond, microsecond).
/// If a `time` key is present (LocalTime, Time, LocalDateTime, or DateTime), its components
/// are used as defaults for any unspecified fields.
fn time_from_map(map: &BTreeMap<String, Value>) -> Result<NaiveTime> {
    let base_time = match map.get("time").or_else(|| map.get("datetime")) {
        Some(Value::LocalTime(t)) => Some(t.0),
        Some(Value::Time(t)) => Some(t.0),
        Some(Value::LocalDateTime(dt)) => Some(dt.0.time()),
        Some(Value::DateTime(dt)) => Some(dt.0.time()),
        _ => None,
    };

    let invalid = || parse_error("invalid time components".to_string());
    let h = get_int::<u32>(map, "hour")?
        .or_else(|| base_time.map(|t| t.hour()))
        .unwrap_or(0);
    let m = get_int::<u32>(map, "minute")?
        .or_else(|| base_time.map(|t| t.minute()))
        .unwrap_or(0);
    let s = get_int::<u32>(map, "second")?
        .or_else(|| base_time.map(|t| t.second()))
        .unwrap_or(0);
    let nano = get_i64(map, "nanosecond")
        .or_else(|| base_time.map(|t| (t.nanosecond() % 1_000_000_000) as i64))
        .unwrap_or(0);
    // Sub-second parts add up; anything past one second is invalid.
    let nano = get_i64(map, "millisecond")
        .unwrap_or(0)
        .checked_mul(1_000_000)
        .and_then(|ms| ms.checked_add(nano))
        .and_then(|n| {
            get_i64(map, "microsecond")
                .unwrap_or(0)
                .checked_mul(1_000)
                .and_then(|us| us.checked_add(n))
        })
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(invalid)?;
    NaiveTime::from_hms_nano_opt(h, m, s, nano).ok_or_else(invalid)
}

/// Build a `NaiveDate` from map keys (year, month, day, week, dayOfWeek, ordinalDay, quarter, dayOfQuarter).
/// If a `date` key is present (Date, LocalDateTime, or DateTime), its components are used as
/// defaults for any unspecified fields.
fn date_from_map(map: &BTreeMap<String, Value>) -> Result<NaiveDate> {
    // Check for a base `date` or `datetime` temporal value to project from.
    let base_date = match map.get("date").or_else(|| map.get("datetime")) {
        Some(Value::Date(d)) => Some(d.0),
        Some(Value::LocalDateTime(dt)) => Some(dt.0.date()),
        Some(Value::DateTime(dt)) => Some(dt.0.date()),
        _ => None,
    };

    // The year comes from the map or the base date; there is no default.
    let year = |iso_week_year: bool| -> Result<i32> {
        match get_int::<i32>(map, "year")? {
            Some(y) => Ok(y),
            None => base_date
                .map(|d| {
                    if iso_week_year {
                        d.iso_week().year()
                    } else {
                        d.year()
                    }
                })
                .ok_or_else(|| invalid_map("year must be specified")),
        }
    };

    if let Some(week) = get_int::<u32>(map, "week")? {
        // For ISO week dates, the year should be the ISO week year, not the calendar year.
        let year = year(true)?;
        let dow = get_int::<u32>(map, "dayOfWeek")?.unwrap_or(
            base_date
                .map(|d| d.weekday().num_days_from_monday() + 1)
                .unwrap_or(1),
        );
        let weekday = match dow {
            1 => chrono::Weekday::Mon,
            2 => chrono::Weekday::Tue,
            3 => chrono::Weekday::Wed,
            4 => chrono::Weekday::Thu,
            5 => chrono::Weekday::Fri,
            6 => chrono::Weekday::Sat,
            7 => chrono::Weekday::Sun,
            _ => return Err(parse_error(format!("invalid dayOfWeek: {dow}"))),
        };
        NaiveDate::from_isoywd_opt(year, week, weekday)
            .ok_or_else(|| parse_error("invalid week date components".to_string()))
    } else if let Some(ord) = get_int::<u32>(map, "ordinalDay")? {
        let year = year(false)?;
        NaiveDate::from_yo_opt(year, ord)
            .ok_or_else(|| parse_error("invalid ordinal date components".to_string()))
    } else if let Some(quarter) = get_i64(map, "quarter") {
        if !(1..=4).contains(&quarter) {
            return Err(invalid_map(format!("quarter {quarter} is out of range")));
        }
        let year = year(false)?;
        let quarter_start_month = ((quarter - 1) * 3 + 1) as u32;
        if let Some(doq) = get_i64(map, "dayOfQuarter") {
            // Explicit dayOfQuarter: count from first day of the quarter.
            let start = NaiveDate::from_ymd_opt(year, quarter_start_month, 1)
                .ok_or_else(|| parse_error("invalid quarter".to_string()))?;
            doq.checked_sub(1)
                .and_then(chrono::TimeDelta::try_days)
                .and_then(|delta| start.checked_add_signed(delta))
                .ok_or_else(|| parse_error("dayOfQuarter out of range".to_string()))
        } else if let Some(bd) = base_date {
            // No explicit dayOfQuarter but base date present: preserve month-offset
            // within the quarter and day-of-month from the base date.
            let base_month_in_quarter = (bd.month() - 1) % 3; // 0, 1, or 2
            let month = quarter_start_month + base_month_in_quarter;
            let day = get_int::<u32>(map, "day")?.unwrap_or(bd.day());
            NaiveDate::from_ymd_opt(year, month, day)
                .ok_or_else(|| parse_error("invalid quarter date".to_string()))
        } else {
            // No base date, no dayOfQuarter: first day of the quarter.
            NaiveDate::from_ymd_opt(year, quarter_start_month, 1)
                .ok_or_else(|| parse_error("invalid quarter".to_string()))
        }
    } else {
        let year = year(false)?;
        let month = get_int::<u32>(map, "month")?
            .or_else(|| base_date.map(|d| d.month()))
            .unwrap_or(1);
        let day = get_int::<u32>(map, "day")?
            .or_else(|| base_date.map(|d| d.day()))
            .unwrap_or(1);
        NaiveDate::from_ymd_opt(year, month, day)
            .ok_or_else(|| parse_error("invalid date components".to_string()))
    }
}

/// Whether a `timezone` value is an offset literal (`+01:00`, `Z`) rather
/// than an IANA zone name.
fn is_offset_literal(s: &str) -> bool {
    s.starts_with('+') || s.starts_with('-') || s == "Z" || s == "z"
}

/// Extract an offset from the `timezone` map key.
/// Returns `(FixedOffset, Option<tz_name>)`.
fn offset_from_map(map: &BTreeMap<String, Value>) -> Result<(FixedOffset, Option<String>)> {
    match map.get("timezone") {
        Some(Value::String(s)) => {
            if s.starts_with('+') || s.starts_with('-') || s == "Z" || s == "z" {
                Ok((parse_offset(s)?, None))
            } else {
                // IANA timezone name — resolve at "now" (no NaiveDateTime context).
                resolve_tz_name_now(s)
            }
        }
        _ => {
            // No explicit timezone — try to inherit from a `time` or `datetime` base temporal.
            // If the base temporal has no offset (e.g. LocalTime, LocalDateTime), default to UTC.
            match map.get("time").or_else(|| map.get("datetime")) {
                Some(Value::Time(t)) => Ok((t.1, None)),
                Some(Value::DateTime(dt)) => Ok((dt.1, dt.2.clone())),
                Some(Value::LocalTime(_) | Value::LocalDateTime(_)) => {
                    Ok((FixedOffset::east_opt(0).unwrap(), None))
                }
                _ => {
                    // Default to UTC when no timezone source is available.
                    // This covers `time({hour: 12})` and `datetime({date: d})`.
                    Ok((FixedOffset::east_opt(0).unwrap(), None))
                }
            }
        }
    }
}

/// Resolve an IANA timezone name to offset using the current time.
/// Used when no specific datetime context is available (e.g. CypherTime).
fn resolve_tz_name_now(name: &str) -> Result<(FixedOffset, Option<String>)> {
    let tz: chrono_tz::Tz = name.parse().map_err(|_| parse_error("unknown timezone"))?;
    let now = chrono::Utc::now().with_timezone(&tz);
    let off = now.offset().fix();
    Ok((off, Some(name.to_string())))
}

/// Resolve an IANA timezone name to offset at a specific NaiveDateTime.
/// Uses the earliest valid local time (handles DST transitions).
fn resolve_tz_name_at(name: &str, dt: &NaiveDateTime) -> Result<(FixedOffset, Option<String>)> {
    let tz: chrono_tz::Tz = name.parse().map_err(|_| parse_error("unknown timezone"))?;
    let aware = tz
        .from_local_datetime(dt)
        .earliest()
        .ok_or_else(|| parse_error("ambiguous or invalid datetime in timezone"))?;
    let off = aware.offset().fix();
    Ok((off, Some(name.to_string())))
}

// ===========================================================================
// CypherDate
// ===========================================================================

/// openCypher `Date` — a calendar date without time or timezone.
#[derive(Debug, Clone)]
pub struct CypherDate(pub NaiveDate);

impl PartialEq for CypherDate {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for CypherDate {}
impl Hash for CypherDate {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl fmt::Display for CypherDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02}",
            self.0.year(),
            self.0.month(),
            self.0.day()
        )
    }
}

impl Serialize for CypherDate {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("CypherDate", 3)?;
        s.serialize_field("year", &self.0.year())?;
        s.serialize_field("month", &self.0.month())?;
        s.serialize_field("day", &self.0.day())?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for CypherDate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "lowercase")]
        enum Field {
            Year,
            Month,
            Day,
        }

        struct CypherDateVisitor;
        impl<'de> Visitor<'de> for CypherDateVisitor {
            type Value = CypherDate;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a CypherDate struct with year, month, day")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<CypherDate, A::Error> {
                let year: i32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let month: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let day: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(2, &self))?;
                let date = NaiveDate::from_ymd_opt(year, month, day)
                    .ok_or_else(|| de::Error::custom("invalid date"))?;
                Ok(CypherDate(date))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<CypherDate, A::Error> {
                let (mut year, mut month, mut day) = (None, None, None);
                while let Some(key) = map.next_key()? {
                    match key {
                        Field::Year => year = Some(map.next_value()?),
                        Field::Month => month = Some(map.next_value()?),
                        Field::Day => day = Some(map.next_value()?),
                    }
                }
                let year: i32 = year.ok_or_else(|| de::Error::missing_field("year"))?;
                let month: u32 = month.ok_or_else(|| de::Error::missing_field("month"))?;
                let day: u32 = day.ok_or_else(|| de::Error::missing_field("day"))?;
                let date = NaiveDate::from_ymd_opt(year, month, day)
                    .ok_or_else(|| de::Error::custom("invalid date"))?;
                Ok(CypherDate(date))
            }
        }
        deserializer.deserialize_any(CypherDateVisitor)
    }
}

impl CypherDate {
    /// Parse a date from an ISO 8601 string.
    pub fn from_iso_string(s: &str) -> Result<Self> {
        ensure_ascii(s, "date")?;
        parse_date_str(s).map(CypherDate)
    }

    /// Build a date from a property map.
    pub fn from_map(map: &BTreeMap<String, Value>) -> Result<Self> {
        check_keys(map, "date()", &[DATE_KEYS, &["date", "datetime"]])?;
        date_from_map(map).map(CypherDate)
    }
}

// ===========================================================================
// CypherLocalTime
// ===========================================================================

/// openCypher `LocalTime` — a time without timezone.
#[derive(Debug, Clone)]
pub struct CypherLocalTime(pub NaiveTime);

impl PartialEq for CypherLocalTime {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for CypherLocalTime {}
impl Hash for CypherLocalTime {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl fmt::Display for CypherLocalTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_local_time(&self.0, f)
    }
}

impl Serialize for CypherLocalTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("CypherLocalTime", 4)?;
        s.serialize_field("hour", &self.0.hour())?;
        s.serialize_field("minute", &self.0.minute())?;
        s.serialize_field("second", &self.0.second())?;
        s.serialize_field("nanosecond", &self.0.nanosecond())?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for CypherLocalTime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "lowercase")]
        enum Field {
            Hour,
            Minute,
            Second,
            Nanosecond,
        }

        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = CypherLocalTime;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("CypherLocalTime")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<CypherLocalTime, A::Error> {
                let h: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let m: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let s: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(2, &self))?;
                let n: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(3, &self))?;
                let t = NaiveTime::from_hms_nano_opt(h, m, s, n)
                    .ok_or_else(|| de::Error::custom("invalid time"))?;
                Ok(CypherLocalTime(t))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<CypherLocalTime, A::Error> {
                let (mut hour, mut minute, mut second, mut nano) = (None, None, None, None);
                while let Some(key) = map.next_key()? {
                    match key {
                        Field::Hour => hour = Some(map.next_value()?),
                        Field::Minute => minute = Some(map.next_value()?),
                        Field::Second => second = Some(map.next_value()?),
                        Field::Nanosecond => nano = Some(map.next_value()?),
                    }
                }
                let h: u32 = hour.ok_or_else(|| de::Error::missing_field("hour"))?;
                let m: u32 = minute.ok_or_else(|| de::Error::missing_field("minute"))?;
                let s: u32 = second.ok_or_else(|| de::Error::missing_field("second"))?;
                let n: u32 = nano.ok_or_else(|| de::Error::missing_field("nanosecond"))?;
                let t = NaiveTime::from_hms_nano_opt(h, m, s, n)
                    .ok_or_else(|| de::Error::custom("invalid time"))?;
                Ok(CypherLocalTime(t))
            }
        }
        deserializer.deserialize_any(V)
    }
}

impl CypherLocalTime {
    /// Parse a local time from an ISO 8601 string.
    pub fn from_iso_string(s: &str) -> Result<Self> {
        ensure_ascii(s, "local time")?;
        parse_time_str(s).map(CypherLocalTime)
    }

    /// Build a local time from a property map.
    pub fn from_map(map: &BTreeMap<String, Value>) -> Result<Self> {
        check_keys(map, "localtime()", &[TIME_KEYS, &["time", "datetime"]])?;
        time_from_map(map).map(CypherLocalTime)
    }
}

// ===========================================================================
// CypherTime
// ===========================================================================

/// openCypher `Time` — a time with timezone offset.
#[derive(Debug, Clone)]
pub struct CypherTime(pub NaiveTime, pub FixedOffset);

impl PartialEq for CypherTime {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0 && self.1 == other.1
    }
}
impl Eq for CypherTime {}
impl Hash for CypherTime {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
        self.1.local_minus_utc().hash(state);
    }
}

impl fmt::Display for CypherTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_local_time(&self.0, f)?;
        fmt_offset(&self.1, f)
    }
}

impl Serialize for CypherTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("CypherTime", 5)?;
        s.serialize_field("hour", &self.0.hour())?;
        s.serialize_field("minute", &self.0.minute())?;
        s.serialize_field("second", &self.0.second())?;
        s.serialize_field("nanosecond", &self.0.nanosecond())?;
        s.serialize_field("offset_seconds", &self.1.local_minus_utc())?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for CypherTime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "lowercase")]
        enum Field {
            Hour,
            Minute,
            Second,
            Nanosecond,
            #[serde(rename = "offset_seconds")]
            OffsetSeconds,
        }

        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = CypherTime;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("CypherTime")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<CypherTime, A::Error> {
                let h: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let m: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let s: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(2, &self))?;
                let n: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(3, &self))?;
                let o: i32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(4, &self))?;
                let t = NaiveTime::from_hms_nano_opt(h, m, s, n)
                    .ok_or_else(|| de::Error::custom("invalid time"))?;
                let offset =
                    FixedOffset::east_opt(o).ok_or_else(|| de::Error::custom("invalid offset"))?;
                Ok(CypherTime(t, offset))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<CypherTime, A::Error> {
                let (mut hour, mut minute, mut second, mut nano, mut off) =
                    (None, None, None, None, None);
                while let Some(key) = map.next_key()? {
                    match key {
                        Field::Hour => hour = Some(map.next_value()?),
                        Field::Minute => minute = Some(map.next_value()?),
                        Field::Second => second = Some(map.next_value()?),
                        Field::Nanosecond => nano = Some(map.next_value()?),
                        Field::OffsetSeconds => off = Some(map.next_value()?),
                    }
                }
                let h: u32 = hour.ok_or_else(|| de::Error::missing_field("hour"))?;
                let m: u32 = minute.ok_or_else(|| de::Error::missing_field("minute"))?;
                let s: u32 = second.ok_or_else(|| de::Error::missing_field("second"))?;
                let n: u32 = nano.ok_or_else(|| de::Error::missing_field("nanosecond"))?;
                let o: i32 = off.ok_or_else(|| de::Error::missing_field("offset_seconds"))?;
                let t = NaiveTime::from_hms_nano_opt(h, m, s, n)
                    .ok_or_else(|| de::Error::custom("invalid time"))?;
                let offset =
                    FixedOffset::east_opt(o).ok_or_else(|| de::Error::custom("invalid offset"))?;
                Ok(CypherTime(t, offset))
            }
        }
        deserializer.deserialize_any(V)
    }
}

impl CypherTime {
    /// Parse a time with offset from an ISO 8601 string.
    /// When no offset is present, defaults to UTC (+00:00).
    pub fn from_iso_string(s: &str) -> Result<Self> {
        ensure_ascii(s, "time")?;
        let (time_part, off_part) = split_time_offset(s);
        let t = parse_time_str(time_part)?;
        let off = match off_part {
            Some(o) => parse_offset(o)?,
            None => FixedOffset::east_opt(0).unwrap(),
        };
        Ok(CypherTime(t, off))
    }

    /// Build from a property map.
    pub fn from_map(map: &BTreeMap<String, Value>) -> Result<Self> {
        check_keys(
            map,
            "time()",
            &[TIME_KEYS, &["time", "datetime", "timezone"]],
        )?;
        let t = time_from_map(map)?;
        let (off, _tz_name) = offset_from_map(map)?;
        // If the base temporal had a different offset and an explicit timezone was
        // given, convert the local time from source offset to target offset.
        let t = if map.contains_key("timezone") {
            if let Some(src_off) = base_source_offset(map) {
                if src_off != off {
                    let delta = off.local_minus_utc() - src_off.local_minus_utc();
                    let secs = t.num_seconds_from_midnight() as i64 + delta as i64;
                    let secs = secs.rem_euclid(86400) as u32;
                    NaiveTime::from_num_seconds_from_midnight_opt(
                        secs,
                        t.nanosecond() % 1_000_000_000,
                    )
                    .unwrap_or(t)
                } else {
                    t
                }
            } else {
                t
            }
        } else {
            t
        };
        Ok(CypherTime(t, off))
    }
}

// ===========================================================================
// CypherLocalDateTime
// ===========================================================================

/// openCypher `LocalDateTime` — a date-time without timezone.
#[derive(Debug, Clone)]
pub struct CypherLocalDateTime(pub NaiveDateTime);

impl PartialEq for CypherLocalDateTime {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for CypherLocalDateTime {}
impl Hash for CypherLocalDateTime {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl fmt::Display for CypherLocalDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = self.0.date();
        write!(f, "{:04}-{:02}-{:02}T", d.year(), d.month(), d.day())?;
        fmt_local_time(&self.0.time(), f)
    }
}

impl Serialize for CypherLocalDateTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let d = self.0.date();
        let t = self.0.time();
        let mut s = serializer.serialize_struct("CypherLocalDateTime", 7)?;
        s.serialize_field("year", &d.year())?;
        s.serialize_field("month", &d.month())?;
        s.serialize_field("day", &d.day())?;
        s.serialize_field("hour", &t.hour())?;
        s.serialize_field("minute", &t.minute())?;
        s.serialize_field("second", &t.second())?;
        s.serialize_field("nanosecond", &t.nanosecond())?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for CypherLocalDateTime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "lowercase")]
        enum Field {
            Year,
            Month,
            Day,
            Hour,
            Minute,
            Second,
            Nanosecond,
        }

        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = CypherLocalDateTime;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("CypherLocalDateTime")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<CypherLocalDateTime, A::Error> {
                let year: i32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let month: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let day: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(2, &self))?;
                let h: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(3, &self))?;
                let m: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(4, &self))?;
                let s: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(5, &self))?;
                let n: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(6, &self))?;
                let date = NaiveDate::from_ymd_opt(year, month, day)
                    .ok_or_else(|| de::Error::custom("invalid date"))?;
                let time = NaiveTime::from_hms_nano_opt(h, m, s, n)
                    .ok_or_else(|| de::Error::custom("invalid time"))?;
                Ok(CypherLocalDateTime(NaiveDateTime::new(date, time)))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<CypherLocalDateTime, A::Error> {
                let (mut year, mut month, mut day) = (None, None, None);
                let (mut hour, mut minute, mut second, mut nano) = (None, None, None, None);
                while let Some(key) = map.next_key()? {
                    match key {
                        Field::Year => year = Some(map.next_value()?),
                        Field::Month => month = Some(map.next_value()?),
                        Field::Day => day = Some(map.next_value()?),
                        Field::Hour => hour = Some(map.next_value()?),
                        Field::Minute => minute = Some(map.next_value()?),
                        Field::Second => second = Some(map.next_value()?),
                        Field::Nanosecond => nano = Some(map.next_value()?),
                    }
                }
                let y: i32 = year.ok_or_else(|| de::Error::missing_field("year"))?;
                let mo: u32 = month.ok_or_else(|| de::Error::missing_field("month"))?;
                let dy: u32 = day.ok_or_else(|| de::Error::missing_field("day"))?;
                let h: u32 = hour.ok_or_else(|| de::Error::missing_field("hour"))?;
                let mi: u32 = minute.ok_or_else(|| de::Error::missing_field("minute"))?;
                let s: u32 = second.ok_or_else(|| de::Error::missing_field("second"))?;
                let n: u32 = nano.ok_or_else(|| de::Error::missing_field("nanosecond"))?;
                let d = NaiveDate::from_ymd_opt(y, mo, dy)
                    .ok_or_else(|| de::Error::custom("invalid date"))?;
                let t = NaiveTime::from_hms_nano_opt(h, mi, s, n)
                    .ok_or_else(|| de::Error::custom("invalid time"))?;
                Ok(CypherLocalDateTime(NaiveDateTime::new(d, t)))
            }
        }
        deserializer.deserialize_any(V)
    }
}

impl CypherLocalDateTime {
    /// Parse from ISO 8601, e.g. `1984-10-11T12:31:14`.
    pub fn from_iso_string(s: &str) -> Result<Self> {
        ensure_ascii(s, "local datetime")?;
        let t_pos = s
            .find('T')
            .or_else(|| s.find('t'))
            .ok_or_else(|| parse_error("expected 'T' separator"))?;
        let date = parse_date_str(&s[..t_pos])?;
        let time = parse_time_str(&s[t_pos + 1..])?;
        Ok(CypherLocalDateTime(NaiveDateTime::new(date, time)))
    }

    /// Build from a property map.
    pub fn from_map(map: &BTreeMap<String, Value>) -> Result<Self> {
        check_keys(
            map,
            "localdatetime()",
            &[DATE_KEYS, TIME_KEYS, &["date", "time", "datetime"]],
        )?;
        let d = date_from_map(map)?;
        let t = time_from_map(map)?;
        Ok(CypherLocalDateTime(NaiveDateTime::new(d, t)))
    }
}

// ===========================================================================
// CypherDateTime
// ===========================================================================

/// openCypher `DateTime` — a date-time with timezone offset and optional IANA timezone name.
#[derive(Debug, Clone)]
pub struct CypherDateTime(pub NaiveDateTime, pub FixedOffset, pub Option<String>);

impl PartialEq for CypherDateTime {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0 && self.1 == other.1 && self.2 == other.2
    }
}
impl Eq for CypherDateTime {}
impl Hash for CypherDateTime {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
        self.1.local_minus_utc().hash(state);
        self.2.hash(state);
    }
}

impl fmt::Display for CypherDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = self.0.date();
        write!(f, "{:04}-{:02}-{:02}T", d.year(), d.month(), d.day())?;
        fmt_local_time(&self.0.time(), f)?;
        fmt_offset(&self.1, f)?;
        if let Some(ref tz) = self.2 {
            write!(f, "[{tz}]")?;
        }
        Ok(())
    }
}

impl Serialize for CypherDateTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let d = self.0.date();
        let t = self.0.time();
        let field_count = if self.2.is_some() { 9 } else { 8 };
        let mut s = serializer.serialize_struct("CypherDateTime", field_count)?;
        s.serialize_field("year", &d.year())?;
        s.serialize_field("month", &d.month())?;
        s.serialize_field("day", &d.day())?;
        s.serialize_field("hour", &t.hour())?;
        s.serialize_field("minute", &t.minute())?;
        s.serialize_field("second", &t.second())?;
        s.serialize_field("nanosecond", &t.nanosecond())?;
        s.serialize_field("offset_seconds", &self.1.local_minus_utc())?;
        if let Some(ref tz) = self.2 {
            s.serialize_field("tz_name", tz)?;
        }
        s.end()
    }
}

impl<'de> Deserialize<'de> for CypherDateTime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "lowercase")]
        enum Field {
            Year,
            Month,
            Day,
            Hour,
            Minute,
            Second,
            Nanosecond,
            #[serde(rename = "offset_seconds")]
            OffsetSeconds,
            #[serde(rename = "tz_name")]
            TzName,
        }

        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = CypherDateTime;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("CypherDateTime")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<CypherDateTime, A::Error> {
                let year: i32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let month: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let day: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(2, &self))?;
                let h: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(3, &self))?;
                let m: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(4, &self))?;
                let s: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(5, &self))?;
                let n: u32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(6, &self))?;
                let o: i32 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(7, &self))?;
                let tz_name: Option<String> = seq.next_element().ok().flatten();
                let date = NaiveDate::from_ymd_opt(year, month, day)
                    .ok_or_else(|| de::Error::custom("invalid date"))?;
                let time = NaiveTime::from_hms_nano_opt(h, m, s, n)
                    .ok_or_else(|| de::Error::custom("invalid time"))?;
                let offset =
                    FixedOffset::east_opt(o).ok_or_else(|| de::Error::custom("invalid offset"))?;
                let dt = NaiveDateTime::new(date, time);
                Ok(CypherDateTime(dt, offset, tz_name))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<CypherDateTime, A::Error> {
                let (mut year, mut month, mut day) = (None, None, None);
                let (mut hour, mut minute, mut second, mut nano, mut off) =
                    (None, None, None, None, None);
                let mut tz_name: Option<String> = None;
                while let Some(key) = map.next_key()? {
                    match key {
                        Field::Year => year = Some(map.next_value()?),
                        Field::Month => month = Some(map.next_value()?),
                        Field::Day => day = Some(map.next_value()?),
                        Field::Hour => hour = Some(map.next_value()?),
                        Field::Minute => minute = Some(map.next_value()?),
                        Field::Second => second = Some(map.next_value()?),
                        Field::Nanosecond => nano = Some(map.next_value()?),
                        Field::OffsetSeconds => off = Some(map.next_value()?),
                        Field::TzName => tz_name = Some(map.next_value()?),
                    }
                }
                let y: i32 = year.ok_or_else(|| de::Error::missing_field("year"))?;
                let mo: u32 = month.ok_or_else(|| de::Error::missing_field("month"))?;
                let dy: u32 = day.ok_or_else(|| de::Error::missing_field("day"))?;
                let h: u32 = hour.ok_or_else(|| de::Error::missing_field("hour"))?;
                let mi: u32 = minute.ok_or_else(|| de::Error::missing_field("minute"))?;
                let s: u32 = second.ok_or_else(|| de::Error::missing_field("second"))?;
                let n: u32 = nano.ok_or_else(|| de::Error::missing_field("nanosecond"))?;
                let o: i32 = off.ok_or_else(|| de::Error::missing_field("offset_seconds"))?;
                let d = NaiveDate::from_ymd_opt(y, mo, dy)
                    .ok_or_else(|| de::Error::custom("invalid date"))?;
                let t = NaiveTime::from_hms_nano_opt(h, mi, s, n)
                    .ok_or_else(|| de::Error::custom("invalid time"))?;
                let offset =
                    FixedOffset::east_opt(o).ok_or_else(|| de::Error::custom("invalid offset"))?;
                Ok(CypherDateTime(NaiveDateTime::new(d, t), offset, tz_name))
            }
        }
        deserializer.deserialize_any(V)
    }
}

impl CypherDateTime {
    /// Parse from ISO 8601 with timezone, e.g. `1984-10-11T12:31:14Z` or
    /// `2015-07-21T21:40:32.142+02:00[Europe/Stockholm]`.
    pub fn from_iso_string(s: &str) -> Result<Self> {
        ensure_ascii(s, "datetime")?;
        let t_pos = s
            .find('T')
            .or_else(|| s.find('t'))
            .ok_or_else(|| parse_error("expected 'T' separator"))?;
        let date = parse_date_str(&s[..t_pos])?;
        let time_and_off = &s[t_pos + 1..];

        // Check for [TzName] suffix
        let (time_off_part, tz_name) = if let Some(bracket_pos) = time_and_off.find('[') {
            let name = time_and_off[bracket_pos + 1..]
                .trim_end_matches(']')
                .to_string();
            (&time_and_off[..bracket_pos], Some(name))
        } else {
            (time_and_off, None)
        };

        let (time_part, off_part) = split_time_offset(time_off_part);
        let time = parse_time_str(time_part)?;

        let off = if let Some(off_str) = off_part {
            parse_offset(off_str)?
        } else if let Some(ref tz) = tz_name {
            // No explicit offset but tz name present — resolve from tz at this datetime.
            let ndt = NaiveDateTime::new(date, time);
            let (resolved, _) = resolve_tz_name_at(tz, &ndt)?;
            resolved
        } else {
            return Err(parse_error("CypherDateTime requires an offset or timezone"));
        };

        Ok(CypherDateTime(NaiveDateTime::new(date, time), off, tz_name))
    }

    /// Build from a property map.
    pub fn from_map(map: &BTreeMap<String, Value>) -> Result<Self> {
        if map.contains_key("epochSeconds") || map.contains_key("epochMillis") {
            return Self::from_epoch_map(map);
        }
        check_keys(
            map,
            "datetime()",
            &[
                DATE_KEYS,
                TIME_KEYS,
                &["date", "time", "datetime", "timezone"],
            ],
        )?;
        let d = date_from_map(map)?;
        let t = time_from_map(map)?;
        let ndt = NaiveDateTime::new(d, t);
        match map.get("timezone") {
            Some(Value::String(s)) => {
                let target_off = if s.starts_with('+') || s.starts_with('-') || s == "Z" || s == "z"
                {
                    parse_offset(s)?
                } else {
                    // Will be resolved below after possible conversion.
                    FixedOffset::east_opt(0).unwrap()
                };
                let is_named = !(s.starts_with('+') || s.starts_with('-') || s == "Z" || s == "z");
                // Convert local time when the base temporal has a different offset.
                // Use base_source_offset_at to re-resolve named timezones at the new date.
                let ndt = if let Some(src_off) = base_source_offset_at(map, &ndt) {
                    if is_named {
                        // Convert via UTC intermediary, then resolve target named tz.
                        let utc_ndt =
                            ndt - chrono::Duration::seconds(src_off.local_minus_utc() as i64);
                        let tz: chrono_tz::Tz =
                            s.parse().map_err(|_| parse_error("unknown timezone"))?;
                        let aware = tz.from_utc_datetime(&utc_ndt);
                        let off = aware.offset().fix();
                        let converted = aware.naive_local();
                        return Ok(CypherDateTime(converted, off, Some(s.to_string())));
                    } else if src_off != target_off {
                        let delta = target_off.local_minus_utc() - src_off.local_minus_utc();
                        ndt + chrono::Duration::seconds(delta as i64)
                    } else {
                        ndt
                    }
                } else {
                    ndt
                };
                if is_named {
                    let (off, tz_name) = resolve_tz_name_at(s, &ndt)?;
                    Ok(CypherDateTime(ndt, off, tz_name))
                } else {
                    Ok(CypherDateTime(ndt, target_off, None))
                }
            }
            _ => {
                // No explicit timezone — try to inherit from a `time` or `datetime` base temporal.
                match map.get("time").or_else(|| map.get("datetime")) {
                    Some(Value::Time(t)) => Ok(CypherDateTime(ndt, t.1, None)),
                    Some(Value::DateTime(dt)) => {
                        // If the base has a named timezone, re-resolve at the new
                        // NaiveDateTime to account for DST changes.
                        if let Some(ref tz_name) = dt.2 {
                            let (off, tz) = resolve_tz_name_at(tz_name, &ndt)?;
                            Ok(CypherDateTime(ndt, off, tz))
                        } else {
                            Ok(CypherDateTime(ndt, dt.1, None))
                        }
                    }
                    _ => {
                        // Default to UTC when constructing from date/time components
                        // or from a date projection.
                        Ok(CypherDateTime(ndt, FixedOffset::east_opt(0).unwrap(), None))
                    }
                }
            }
        }
    }

    /// Create from epoch seconds and nanoseconds (UTC).
    ///
    /// Errors with `NumberOutOfRange` when the instant is outside chrono's
    /// calendar range (roughly +-262,000 years).
    pub fn from_epoch(seconds: i64, nanos: i64) -> Result<Self> {
        Self::from_epoch_nanos(seconds as i128 * 1_000_000_000 + nanos as i128)
    }

    /// Create from epoch milliseconds (UTC). Errors like [`Self::from_epoch`].
    pub fn from_epoch_millis(millis: i64) -> Result<Self> {
        Self::from_epoch_nanos(millis as i128 * 1_000_000)
    }

    /// Create from nanoseconds since the Unix epoch (UTC).
    fn from_epoch_nanos(total: i128) -> Result<Self> {
        let secs = i64::try_from(total.div_euclid(1_000_000_000));
        let ns = total.rem_euclid(1_000_000_000) as u32;
        let dt = secs
            .ok()
            .and_then(|s| chrono::DateTime::from_timestamp(s, ns))
            .ok_or_else(|| out_of_range(format!("epoch instant {total}ns")))?;
        Ok(CypherDateTime(
            dt.naive_utc(),
            FixedOffset::east_opt(0).unwrap(),
            None,
        ))
    }

    /// `datetime({epochSeconds | epochMillis, [millisecond, microsecond,
    /// nanosecond], [timezone]})`: an instant since the Unix epoch, shown in
    /// `timezone` (UTC by default).
    fn from_epoch_map(map: &BTreeMap<String, Value>) -> Result<Self> {
        check_keys(
            map,
            "datetime()",
            &[
                &["epochSeconds", "epochMillis", "timezone"],
                &["millisecond", "microsecond", "nanosecond"],
            ],
        )
        .map_err(|_| {
            GraphError::invalid_argument_value(
                QueryPhase::Runtime,
                "datetime()",
                "epochSeconds/epochMillis can only be combined with millisecond, \
                 microsecond, nanosecond and timezone",
            )
        })?;
        let int = |key: &str| -> Result<i128> {
            match map.get(key) {
                None | Some(Value::Null) => Ok(0),
                Some(Value::I64(n)) => Ok(*n as i128),
                Some(_) => Err(invalid_map(format!("{key} must be an integer"))),
            }
        };
        let total = int("epochSeconds")? * 1_000_000_000
            + int("epochMillis")? * 1_000_000
            + int("millisecond")? * 1_000_000
            + int("microsecond")? * 1_000
            + int("nanosecond")?;
        let utc = Self::from_epoch_nanos(total)?;
        match map.get("timezone") {
            Some(Value::String(s)) if is_offset_literal(s) => {
                let off = parse_offset(s)?;
                Ok(CypherDateTime(utc.0 + off, off, None))
            }
            Some(Value::String(name)) => {
                let tz: chrono_tz::Tz =
                    name.parse().map_err(|_| parse_error("unknown timezone"))?;
                let aware = tz.from_utc_datetime(&utc.0);
                Ok(CypherDateTime(
                    aware.naive_local(),
                    aware.offset().fix(),
                    Some(name.clone()),
                ))
            }
            _ => Ok(utc),
        }
    }
}

// ===========================================================================
// CypherDuration
// ===========================================================================

/// openCypher `Duration` — calendar and clock components stored independently.
#[derive(Debug, Clone)]
pub struct CypherDuration {
    pub months: i64,
    pub days: i64,
    pub seconds: i64,
    pub nanos: i64,
}

impl PartialEq for CypherDuration {
    fn eq(&self, other: &Self) -> bool {
        self.months == other.months
            && self.days == other.days
            && self.seconds == other.seconds
            && self.nanos == other.nanos
    }
}
impl Eq for CypherDuration {}
impl Hash for CypherDuration {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.months.hash(state);
        self.days.hash(state);
        self.seconds.hash(state);
        self.nanos.hash(state);
    }
}

impl fmt::Display for CypherDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Decompose months into years + months.
        let years = self.months / 12;
        let months = self.months % 12;

        // Normalize seconds and nanos so they have the same sign.
        // E.g. seconds=-86400, nanos=100000000 → seconds=-86399, nanos=-900000000
        // which represents -86399.9s total.
        let (total_seconds, nanos) = if self.seconds < 0 && self.nanos > 0 {
            (self.seconds + 1, self.nanos - 1_000_000_000)
        } else if self.seconds > 0 && self.nanos < 0 {
            (self.seconds - 1, self.nanos + 1_000_000_000)
        } else {
            (self.seconds, self.nanos)
        };

        // Decompose seconds into hours, minutes, seconds.
        // Rust % preserves sign, so e.g. -60 % 3600 = -60, -60 / 60 = -1.
        let hours = total_seconds / 3600;
        let rem = total_seconds % 3600;
        let minutes = rem / 60;
        let secs = rem % 60;

        let has_date_part = years != 0 || months != 0 || self.days != 0;
        let has_time_part = hours != 0 || minutes != 0 || secs != 0 || nanos != 0;

        if !has_date_part && !has_time_part {
            return write!(f, "PT0S");
        }

        write!(f, "P")?;
        if years != 0 {
            write!(f, "{years}Y")?;
        }
        if months != 0 {
            write!(f, "{months}M")?;
        }
        if self.days != 0 {
            write!(f, "{}D", self.days)?;
        }
        if has_time_part {
            write!(f, "T")?;
            if hours != 0 {
                write!(f, "{hours}H")?;
            }
            if minutes != 0 {
                write!(f, "{minutes}M")?;
            }
            if secs != 0 || nanos != 0 {
                if nanos != 0 {
                    // Format fractional seconds.
                    let frac = format!("{:09}", nanos.unsigned_abs());
                    let trimmed = frac.trim_end_matches('0');
                    let neg = secs < 0 || (secs == 0 && nanos < 0);
                    if neg {
                        let abs_secs = secs.unsigned_abs();
                        write!(f, "-{abs_secs}.{trimmed}S")?;
                    } else {
                        write!(f, "{secs}.{trimmed}S")?;
                    }
                } else {
                    write!(f, "{secs}S")?;
                }
            }
        }
        Ok(())
    }
}

impl Serialize for CypherDuration {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("CypherDuration", 4)?;
        s.serialize_field("months", &self.months)?;
        s.serialize_field("days", &self.days)?;
        s.serialize_field("seconds", &self.seconds)?;
        s.serialize_field("nanos", &self.nanos)?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for CypherDuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "lowercase")]
        enum Field {
            Months,
            Days,
            Seconds,
            Nanos,
        }

        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = CypherDuration;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("CypherDuration")
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<CypherDuration, A::Error> {
                let months: i64 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(0, &self))?;
                let days: i64 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(1, &self))?;
                let seconds: i64 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(2, &self))?;
                let nanos: i64 = seq
                    .next_element()?
                    .ok_or_else(|| de::Error::invalid_length(3, &self))?;
                Ok(CypherDuration {
                    months,
                    days,
                    seconds,
                    nanos,
                })
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<CypherDuration, A::Error> {
                let (mut months, mut days, mut seconds, mut nanos) = (None, None, None, None);
                while let Some(key) = map.next_key()? {
                    match key {
                        Field::Months => months = Some(map.next_value()?),
                        Field::Days => days = Some(map.next_value()?),
                        Field::Seconds => seconds = Some(map.next_value()?),
                        Field::Nanos => nanos = Some(map.next_value()?),
                    }
                }
                Ok(CypherDuration {
                    months: months.ok_or_else(|| de::Error::missing_field("months"))?,
                    days: days.ok_or_else(|| de::Error::missing_field("days"))?,
                    seconds: seconds.ok_or_else(|| de::Error::missing_field("seconds"))?,
                    nanos: nanos.ok_or_else(|| de::Error::missing_field("nanos"))?,
                })
            }
        }
        deserializer.deserialize_any(V)
    }
}

impl CypherDuration {
    /// Parse an ISO 8601 duration string, e.g. `P1Y2M3DT4H5M6.789S`.
    ///
    /// Integer components are exact; a component that overflows the i64
    /// months/days/seconds fields is a `NumberOutOfRange` error.
    pub fn from_iso_string(s: &str) -> Result<Self> {
        ensure_ascii(s, "duration")?;
        if !s.starts_with('P') && !s.starts_with('p') {
            return Err(parse_error("duration must start with 'P'"));
        }
        let body = &s[1..];

        let (date_part, time_part) = if let Some(t_pos) = body.find('T').or_else(|| body.find('t'))
        {
            (&body[..t_pos], Some(&body[t_pos + 1..]))
        } else {
            (body, None)
        };

        let mut months: i64 = 0;
        let mut days: i64 = 0;
        let mut seconds: i64 = 0;
        let mut nanos: i64 = 0;

        // Parse date part: nY nM nD (or YYYY-MM-DD date format)
        if !date_part.is_empty() {
            if date_part.contains('-') && !date_part.bytes().any(|b| b.is_ascii_alphabetic()) {
                // Date-format duration: P2012-02-02T14:37:21.545
                let parts: Vec<&str> = date_part.split('-').collect();
                if parts.len() == 3 {
                    add_scaled(&mut months, parse_component(parts[0])?, 12, "months")?;
                    add_scaled(&mut months, parse_component(parts[1])?, 1, "months")?;
                    add_scaled(&mut days, parse_component(parts[2])?, 1, "days")?;
                } else {
                    return Err(parse_error("invalid date-format duration"));
                }
            } else {
                parse_duration_date_part(
                    date_part,
                    &mut months,
                    &mut days,
                    &mut seconds,
                    &mut nanos,
                )?;
            }
        }

        // Parse time part: nH nM nS (or hh:mm:ss colon format)
        if let Some(tp) = time_part {
            if !tp.is_empty() {
                if tp.contains(':') {
                    // Colon-format time: hh:mm:ss[.fff]
                    let parts: Vec<&str> = tp.split(':').collect();
                    if parts.len() >= 2 {
                        add_scaled(&mut seconds, parse_component(parts[0])?, 3600, "seconds")?;
                        add_scaled(&mut seconds, parse_component(parts[1])?, 60, "seconds")?;
                        if parts.len() == 3 {
                            let s_str = parts[2];
                            if let Some(dot_pos) = s_str.find('.') {
                                let int_part = &s_str[..dot_pos];
                                let frac_part = &s_str[dot_pos + 1..];
                                let int_val: i64 = if int_part.is_empty() {
                                    0
                                } else {
                                    parse_component(int_part)?
                                };
                                add_scaled(&mut seconds, int_val, 1, "seconds")?;
                                nanos += parse_frac_nanos(frac_part)? as i64;
                            } else {
                                add_scaled(&mut seconds, parse_component(s_str)?, 1, "seconds")?;
                            }
                        }
                    }
                } else {
                    parse_duration_time_part(tp, &mut seconds, &mut nanos)?;
                }
            }
        }

        Ok(CypherDuration {
            months,
            days,
            seconds,
            nanos,
        })
    }

    /// Build from a property map.
    ///
    /// Integer components are added exactly (seconds and smaller in i128
    /// nanoseconds); only fractional parts go through floating point.
    /// Fractions cascade down: years to months (x12), months to days
    /// (x30.436875, Neo4j's average month), days to nanoseconds. A total that
    /// does not fit the i64 fields is a `NumberOutOfRange` error.
    pub fn from_map(map: &BTreeMap<String, Value>) -> Result<Self> {
        const UNITS: &[&str] = &[
            "years",
            "months",
            "weeks",
            "days",
            "hours",
            "minutes",
            "seconds",
            "milliseconds",
            "microseconds",
            "nanoseconds",
        ];
        check_keys(map, "duration()", &[UNITS])?;
        // (exact integer part, fractional part) of a component. Every
        // integer part fits in i64, so the i128 sums below cannot overflow.
        let part = |key: &str| -> Result<(i128, f64)> {
            match map.get(key) {
                Some(Value::I64(n)) => Ok((*n as i128, 0.0)),
                Some(Value::F64(f)) if f.is_finite() && f.abs() < 9.2e18 => {
                    let whole = f.trunc();
                    Ok((whole as i128, f - whole))
                }
                Some(Value::F64(f)) => Err(out_of_range(format!("duration {key} {f}"))),
                _ => Ok((0, 0.0)),
            }
        };
        let (years, years_f) = part("years")?;
        let (mons, mons_f) = part("months")?;
        let (weeks, weeks_f) = part("weeks")?;
        let (days, days_f) = part("days")?;
        let (hours, hours_f) = part("hours")?;
        let (minutes, minutes_f) = part("minutes")?;
        let (secs, secs_f) = part("seconds")?;
        let (millis, millis_f) = part("milliseconds")?;
        let (micros, micros_f) = part("microseconds")?;
        let (ns, ns_f) = part("nanoseconds")?;

        // Months: exact integer part plus the whole months of the fractions.
        let frac_months = years_f * 12.0 + mons_f;
        let months = years * 12 + mons + frac_months.trunc() as i128;
        let frac_months = frac_months - frac_months.trunc();

        // Days: Neo4j uses average days per month: 365.2425 / 12 = 30.436875
        let frac_days = weeks_f * 7.0 + days_f + frac_months * 30.436875;
        let total_days = weeks * 7 + days + frac_days.trunc() as i128;
        let frac_days = frac_days - frac_days.trunc();

        // Clock part in nanoseconds: exact integers, rounded fractions.
        let exact_nanos = hours * 3_600_000_000_000
            + minutes * 60_000_000_000
            + secs * 1_000_000_000
            + millis * 1_000_000
            + micros * 1_000
            + ns;
        let frac_nanos = (frac_days * 24.0 + hours_f) * 3_600_000_000_000.0
            + minutes_f * 60_000_000_000.0
            + secs_f * 1_000_000_000.0
            + millis_f * 1_000_000.0
            + micros_f * 1_000.0
            + ns_f;
        let total_nanos = exact_nanos + frac_nanos.round() as i128;

        let fit = |v: i128, what: &str| {
            i64::try_from(v).map_err(|_| out_of_range(format!("duration {what}")))
        };
        // Truncating division keeps seconds and nanos on the same sign.
        Ok(CypherDuration {
            months: fit(months, "months")?,
            days: fit(total_days, "days")?,
            seconds: fit(total_nanos / 1_000_000_000, "seconds")?,
            nanos: (total_nanos % 1_000_000_000) as i64,
        })
    }
}

/// `*acc += value * unit`, failing with `NumberOutOfRange` on overflow.
fn add_scaled(acc: &mut i64, value: i64, unit: i64, what: &str) -> Result<()> {
    *acc = value
        .checked_mul(unit)
        .and_then(|v| acc.checked_add(v))
        .ok_or_else(|| out_of_range(format!("duration {what}")))?;
    Ok(())
}

/// Parse an integer duration component; overflow is `NumberOutOfRange`,
/// anything else unparseable is a serialization error.
fn parse_component(s: &str) -> Result<i64> {
    s.parse::<i64>().map_err(|e| match e.kind() {
        std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => {
            out_of_range(format!("duration component {s}"))
        }
        _ => parse_error(e.to_string()),
    })
}

/// Split a decimal component like `-1.5` into its exact integer part and its
/// (signed) fraction.
fn split_decimal(s: &str) -> Result<(i64, f64)> {
    let (int_str, frac_str) = s.split_once('.').unwrap_or((s, ""));
    let negative = int_str.starts_with('-');
    if matches!(int_str, "" | "-") && frac_str.is_empty() {
        return Err(parse_error("invalid duration component"));
    }
    let int_val = if matches!(int_str, "" | "-") {
        0
    } else {
        parse_component(int_str)?
    };
    let frac = if frac_str.is_empty() {
        0.0
    } else {
        format!("0.{frac_str}")
            .parse::<f64>()
            .map_err(|e| parse_error(e.to_string()))?
    };
    Ok((int_val, if negative { -frac } else { frac }))
}

/// Cascade fractional months into days/seconds/nanos.
fn cascade_frac_months(
    frac: f64,
    days: &mut i64,
    seconds: &mut i64,
    nanos: &mut i64,
) -> Result<()> {
    if frac == 0.0 {
        return Ok(());
    }
    let days_f = frac * 30.436875;
    add_scaled(days, days_f.trunc() as i64, 1, "days")?;
    cascade_frac_days(days_f - days_f.trunc(), seconds, nanos)
}

/// Cascade fractional days into seconds/nanos.
fn cascade_frac_days(frac: f64, seconds: &mut i64, nanos: &mut i64) -> Result<()> {
    if frac == 0.0 {
        return Ok(());
    }
    let total_nanos_i = (frac * 86400.0 * 1_000_000_000.0).round() as i64;
    add_scaled(seconds, total_nanos_i / 1_000_000_000, 1, "seconds")?;
    *nanos += total_nanos_i % 1_000_000_000;
    Ok(())
}

/// Parse the date portion of a duration string (before T).
fn parse_duration_date_part(
    s: &str,
    months: &mut i64,
    days: &mut i64,
    seconds: &mut i64,
    nanos: &mut i64,
) -> Result<()> {
    ensure_ascii(s, "duration")?;
    let mut num_start = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    // Handle leading negative sign on first component
    if i < bytes.len() && bytes[i] == b'-' {
        i += 1;
    }
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_digit() || b == b'.' {
            i += 1;
            continue;
        }
        let (int_part, frac) = split_decimal(&s[num_start..i])?;
        match b {
            b'Y' | b'y' => {
                // Integer years → months, fractional years → fractional months
                add_scaled(months, int_part, 12, "months")?;
                let frac_months = frac * 12.0;
                add_scaled(months, frac_months.trunc() as i64, 1, "months")?;
                cascade_frac_months(frac_months - frac_months.trunc(), days, seconds, nanos)?;
            }
            b'M' | b'm' => {
                add_scaled(months, int_part, 1, "months")?;
                cascade_frac_months(frac, days, seconds, nanos)?;
            }
            b'W' | b'w' => {
                add_scaled(days, int_part, 7, "days")?;
                let days_f = frac * 7.0;
                add_scaled(days, days_f.trunc() as i64, 1, "days")?;
                cascade_frac_days(days_f - days_f.trunc(), seconds, nanos)?;
            }
            b'D' | b'd' => {
                add_scaled(days, int_part, 1, "days")?;
                cascade_frac_days(frac, seconds, nanos)?;
            }
            _ => return Err(parse_error("unexpected character in duration date part")),
        }
        i += 1;
        num_start = i;
        // Handle negative sign for next component
        if i < bytes.len() && bytes[i] == b'-' {
            i += 1;
        }
    }
    Ok(())
}

/// Parse the time portion of a duration string (after T).
fn parse_duration_time_part(s: &str, seconds: &mut i64, nanos: &mut i64) -> Result<()> {
    ensure_ascii(s, "duration")?;
    let mut num_start = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    // Handle leading negative sign
    if i < bytes.len() && bytes[i] == b'-' {
        i += 1;
    }
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_digit() || b == b'.' {
            i += 1;
            continue;
        }
        let num_str = &s[num_start..i];
        match b {
            b'H' | b'h' | b'M' | b'm' => {
                let unit: i64 = if matches!(b, b'H' | b'h') { 3600 } else { 60 };
                let (int_part, frac) = split_decimal(num_str)?;
                add_scaled(seconds, int_part, unit, "seconds")?;
                if frac != 0.0 {
                    let frac_nanos = (frac * unit as f64 * 1_000_000_000.0).round() as i64;
                    add_scaled(seconds, frac_nanos / 1_000_000_000, 1, "seconds")?;
                    *nanos += frac_nanos % 1_000_000_000;
                }
            }
            b'S' | b's' => {
                // May be fractional; the fraction is parsed exactly.
                if let Some(dot_pos) = num_str.find('.') {
                    let int_part = &num_str[..dot_pos];
                    let frac_part = &num_str[dot_pos + 1..];
                    let negative = int_part.starts_with('-');
                    let int_val: i64 = if int_part.is_empty() || int_part == "-" {
                        0
                    } else {
                        parse_component(int_part)?
                    };
                    let frac_nanos = parse_frac_nanos(frac_part)? as i64;
                    add_scaled(seconds, int_val, 1, "seconds")?;
                    *nanos += if negative { -frac_nanos } else { frac_nanos };
                } else {
                    add_scaled(seconds, parse_component(num_str)?, 1, "seconds")?;
                }
            }
            _ => return Err(parse_error("unexpected character in duration time part")),
        }
        i += 1;
        num_start = i;
        // Handle negative sign for next component
        if i < bytes.len() && bytes[i] == b'-' {
            i += 1;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// duration.between / inMonths / inDays / inSeconds helpers
// ---------------------------------------------------------------------------

/// Extract a `NaiveDate` from any temporal value that has a date component.
pub fn extract_date(val: &Value) -> Option<NaiveDate> {
    match val {
        Value::Date(d) => Some(d.0),
        Value::LocalDateTime(dt) => Some(dt.0.date()),
        Value::DateTime(dt) => Some(dt.0.date()),
        _ => None,
    }
}

/// Extract a `NaiveTime` from any temporal value that has a time component.
/// For date-only values, returns midnight.
pub fn extract_time(val: &Value) -> Option<NaiveTime> {
    match val {
        Value::Date(_) => Some(NaiveTime::from_hms_opt(0, 0, 0).unwrap()),
        Value::LocalTime(t) => Some(t.0),
        Value::Time(t) => Some(t.0),
        Value::LocalDateTime(dt) => Some(dt.0.time()),
        Value::DateTime(dt) => Some(dt.0.time()),
        _ => None,
    }
}

/// Extract UTC offset seconds from a temporal value. Returns 0 for local types.
pub fn extract_offset_secs(val: &Value) -> i32 {
    match val {
        Value::Time(t) => t.1.local_minus_utc(),
        Value::DateTime(dt) => dt.1.local_minus_utc(),
        _ => 0,
    }
}

/// Whether a value is a time-only type (LocalTime or Time).
fn is_time_only(val: &Value) -> bool {
    matches!(val, Value::LocalTime(_) | Value::Time(_))
}

/// Whether a value has a date component.
fn has_date(val: &Value) -> bool {
    matches!(
        val,
        Value::Date(_) | Value::LocalDateTime(_) | Value::DateTime(_)
    )
}

/// Whether a value is offset-aware (Time with offset or DateTime with offset).
fn is_offset_aware(val: &Value) -> bool {
    matches!(val, Value::Time(_) | Value::DateTime(_))
}

/// Extract the named timezone string from a DateTime value.
fn extract_tz_name(val: &Value) -> Option<&str> {
    if let Value::DateTime(dt) = val {
        dt.2.as_deref()
    } else {
        None
    }
}

/// Resolve the UTC offset for a local date/time interpreted in a named timezone.
/// Uses `earliest()` to pick the pre-transition offset for ambiguous times.
fn resolve_offset_in_tz(date: NaiveDate, time: NaiveTime, tz_name: &str) -> i64 {
    if let Ok(tz) = tz_name.parse::<chrono_tz::Tz>() {
        let ndt = NaiveDateTime::new(date, time);
        if let Some(aware) = tz.from_local_datetime(&ndt).earliest() {
            return aware.offset().fix().local_minus_utc() as i64;
        }
    }
    0
}

/// Compute the effective UTC offset for `val` when paired with `other`.
///
/// When both sides are offset-aware, use actual offsets. When one side has a
/// named timezone and the other is a local type, resolve the local side's
/// offset in the named timezone (DST-aware). For time-only local values, the
/// date from the other side is used for DST resolution.
fn effective_offset_dst(val: &Value, other: &Value) -> i64 {
    // Both offset-aware: standard path.
    if is_offset_aware(val) && is_offset_aware(other) {
        return extract_offset_secs(val) as i64;
    }

    // If val is offset-aware (DateTime with tz name), return its actual offset.
    if is_offset_aware(val) {
        // Check if the other side is a local type and we have a named tz.
        if extract_tz_name(val).is_some() && !is_offset_aware(other) {
            return extract_offset_secs(val) as i64;
        }
        return 0;
    }

    // val is a local type — check if other has a named timezone.
    if let Some(tz_name) = extract_tz_name(other) {
        if !is_offset_aware(val) {
            // Resolve val's local date/time in the other side's timezone.
            let date = extract_date(val)
                .or_else(|| extract_date(other))
                .unwrap_or(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());
            let time = extract_time(val).unwrap_or(NaiveTime::from_hms_opt(0, 0, 0).unwrap());
            return resolve_offset_in_tz(date, time, tz_name);
        }
    }

    0
}

/// Number of days in a given month of a given year.
fn days_in_month(year: i32, month: u32) -> u32 {
    // Use the 1st of the next month minus 1 day trick.
    if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    }
    .unwrap()
    .signed_duration_since(NaiveDate::from_ymd_opt(year, month, 1).unwrap())
    .num_days() as u32
}

/// Add `months` months to a `NaiveDate`, clamping the day to the target month's max.
fn add_months_to_date(d: NaiveDate, months: i64) -> NaiveDate {
    let total_months = d.year() as i64 * 12 + (d.month() as i64 - 1) + months;
    let y = total_months.div_euclid(12) as i32;
    let m = (total_months.rem_euclid(12) + 1) as u32;
    let max_day = days_in_month(y, m);
    let day = (d.day()).min(max_day);
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

/// Convert a `NaiveTime` to total nanoseconds since midnight.
fn time_to_nanos(t: NaiveTime) -> i64 {
    t.num_seconds_from_midnight() as i64 * 1_000_000_000 + t.nanosecond() as i64
}

/// Compute the calendar-month difference between two dates, adjusting if the
/// day-of-month means we haven't reached a full month boundary yet.
fn month_diff(d1: NaiveDate, d2: NaiveDate) -> i64 {
    let mut months =
        (d2.year() as i64 - d1.year() as i64) * 12 + d2.month() as i64 - d1.month() as i64;

    // Check if we overshot: adding `months` months to d1 should not go past d2.
    let adjusted = add_months_to_date(d1, months);
    if months > 0 && adjusted > d2 {
        months -= 1;
    } else if months < 0 && adjusted < d2 {
        months += 1;
    }
    months
}

/// Compute `duration.between(lhs, rhs)`.
///
/// Returns a `CypherDuration` with months (calendar) + remainder as seconds/nanos.
pub fn duration_between(lhs: &Value, rhs: &Value) -> CypherDuration {
    let d1 = extract_date(lhs);
    let d2 = extract_date(rhs);
    let t1 = extract_time(lhs);
    let t2 = extract_time(rhs);

    // When one side is time-only and the other has a date → time-only comparison.
    let both_have_dates = d1.is_some() && d2.is_some() && has_date(lhs) && has_date(rhs);
    let either_time_only = is_time_only(lhs) || is_time_only(rhs);

    if either_time_only && !both_have_dates {
        // Pure time comparison — take the time from whichever has it.
        let lhs_nanos =
            t1.map(time_to_nanos).unwrap_or(0) - effective_offset_dst(lhs, rhs) * 1_000_000_000;
        let rhs_nanos =
            t2.map(time_to_nanos).unwrap_or(0) - effective_offset_dst(rhs, lhs) * 1_000_000_000;
        let diff_nanos = rhs_nanos - lhs_nanos;
        let total_secs = diff_nanos.div_euclid(1_000_000_000);
        let rem_nanos = diff_nanos.rem_euclid(1_000_000_000);
        // Normalize: if total_secs > 0 but we need negative, keep consistent sign.
        // Actually div_euclid/rem_euclid always gives non-negative remainder, which
        // works for positive diffs. For negative diffs, total_secs is negative and
        // rem_nanos is non-negative. If rem_nanos > 0 with negative seconds, we need
        // to present it properly. But CypherDuration stores them separately and the
        // Display handles sign combining.
        return CypherDuration {
            months: 0,
            days: 0,
            seconds: total_secs,
            nanos: rem_nanos,
        };
    }

    if let (Some(date1), Some(date2)) = (d1, d2) {
        // Both have dates — compute calendar months first.
        let months = month_diff(date1, date2);

        // Advance d1 by those months to get the remainder as days+time.
        let d1_advanced = add_months_to_date(date1, months);

        // Remainder days after month advancement.
        let mut day_diff = date2.signed_duration_since(d1_advanced).num_days();

        // Time-of-day difference (in nanos), adjusted for UTC offsets only when
        // both sides are offset-aware.
        let t1_nanos =
            t1.map(time_to_nanos).unwrap_or(0) - effective_offset_dst(lhs, rhs) * 1_000_000_000;
        let t2_nanos =
            t2.map(time_to_nanos).unwrap_or(0) - effective_offset_dst(rhs, lhs) * 1_000_000_000;
        let mut time_diff_nanos = t2_nanos - t1_nanos;

        // Normalize: if day_diff and time_diff have opposite signs, borrow a day.
        let nanos_per_day: i64 = 86_400_000_000_000;
        if day_diff > 0 && time_diff_nanos < 0 {
            day_diff -= 1;
            time_diff_nanos += nanos_per_day;
        } else if day_diff < 0 && time_diff_nanos > 0 {
            day_diff += 1;
            time_diff_nanos -= nanos_per_day;
        }

        let secs = time_diff_nanos.div_euclid(1_000_000_000);
        let ns = time_diff_nanos.rem_euclid(1_000_000_000);

        CypherDuration {
            months,
            days: day_diff,
            seconds: secs,
            nanos: ns,
        }
    } else {
        // Fallback: no dates on either side, this shouldn't normally happen
        // but handle gracefully.
        CypherDuration {
            months: 0,
            days: 0,
            seconds: 0,
            nanos: 0,
        }
    }
}

/// Compute `duration.inMonths(lhs, rhs)` — only the months component.
pub fn duration_in_months(lhs: &Value, rhs: &Value) -> CypherDuration {
    let d1 = extract_date(lhs);
    let d2 = extract_date(rhs);

    if is_time_only(lhs) || is_time_only(rhs) || d1.is_none() || d2.is_none() {
        return CypherDuration {
            months: 0,
            days: 0,
            seconds: 0,
            nanos: 0,
        };
    }

    let date1 = d1.unwrap();
    let date2 = d2.unwrap();

    // For inMonths with datetime args, we need to check if the time pushes us
    // past a month boundary. Compare times at the month-boundary date.
    let mut months = month_diff(date1, date2);

    // If dates+times cross: e.g. datetime('2014-07-21T21:40:36.143+0200') to
    // datetime('2015-07-21T21:40:32.142+0100'): the date diff is exactly 12 months,
    // but the time on RHS is earlier → still 12 months (P1Y) because month_diff
    // already handles day clamping. But we also need time comparison:
    // if same day-of-month after advancing, check time.
    let d1_advanced = add_months_to_date(date1, months);
    if d1_advanced == date2 {
        // Same date after advancing — check time.
        let t1_nanos = extract_time(lhs).map(time_to_nanos).unwrap_or(0)
            - effective_offset_dst(lhs, rhs) * 1_000_000_000;
        let t2_nanos = extract_time(rhs).map(time_to_nanos).unwrap_or(0)
            - effective_offset_dst(rhs, lhs) * 1_000_000_000;
        if months > 0 && t2_nanos < t1_nanos {
            months -= 1;
        } else if months < 0 && t2_nanos > t1_nanos {
            months += 1;
        }
    }

    CypherDuration {
        months,
        days: 0,
        seconds: 0,
        nanos: 0,
    }
}

/// Compute `duration.inDays(lhs, rhs)` — total elapsed days + time remainder.
pub fn duration_in_days(lhs: &Value, rhs: &Value) -> CypherDuration {
    let d1 = extract_date(lhs);
    let d2 = extract_date(rhs);

    // If either side is time-only without a date on the other side with a date,
    // and the other side has no date, or if either side is time-only → no days.
    if is_time_only(lhs) || is_time_only(rhs) {
        // When one side is time-only, inDays returns PT0S (no days to count).
        return CypherDuration {
            months: 0,
            days: 0,
            seconds: 0,
            nanos: 0,
        };
    }

    if let (Some(date1), Some(date2)) = (d1, d2) {
        // Compute total elapsed nanoseconds including time components. i128:
        // a span over ~292 years overflows i64 nanoseconds.
        let day_nanos =
            date2.signed_duration_since(date1).num_days() as i128 * 86_400_000_000_000i128;
        let t1_nanos = extract_time(lhs).map(time_to_nanos).unwrap_or(0)
            - effective_offset_dst(lhs, rhs) * 1_000_000_000;
        let t2_nanos = extract_time(rhs).map(time_to_nanos).unwrap_or(0)
            - effective_offset_dst(rhs, lhs) * 1_000_000_000;
        let total_nanos = day_nanos + (t2_nanos - t1_nanos) as i128;

        // Truncate toward zero to get whole days (always fits: chrono dates
        // span far fewer than i64::MAX days).
        let nanos_per_day: i128 = 86_400_000_000_000;
        let days = (total_nanos / nanos_per_day) as i64;

        CypherDuration {
            months: 0,
            days,
            seconds: 0,
            nanos: 0,
        }
    } else {
        CypherDuration {
            months: 0,
            days: 0,
            seconds: 0,
            nanos: 0,
        }
    }
}

/// Compute `duration.inSeconds(lhs, rhs)` — everything flattened to seconds+nanos.
pub fn duration_in_seconds(lhs: &Value, rhs: &Value) -> CypherDuration {
    let d1 = extract_date(lhs);
    let d2 = extract_date(rhs);

    // If either is time-only and the other has a date (but not time-only), do time-only diff.
    let either_time_only = is_time_only(lhs) || is_time_only(rhs);

    let t1_nanos = extract_time(lhs).map(time_to_nanos).unwrap_or(0)
        - effective_offset_dst(lhs, rhs) * 1_000_000_000;
    let t2_nanos = extract_time(rhs).map(time_to_nanos).unwrap_or(0)
        - effective_offset_dst(rhs, lhs) * 1_000_000_000;

    let time_diff = t2_nanos - t1_nanos;

    if either_time_only && !(has_date(lhs) && has_date(rhs)) {
        // Time-only comparison.
        let secs = time_diff.div_euclid(1_000_000_000);
        let ns = time_diff.rem_euclid(1_000_000_000);
        return CypherDuration {
            months: 0,
            days: 0,
            seconds: secs,
            nanos: ns,
        };
    }

    if let (Some(date1), Some(date2)) = (d1, d2) {
        // i128: a span over ~292 years overflows i64 nanoseconds. The
        // seconds always fit i64 (chrono dates span ~5e14 days at most).
        let total_day_secs = date2.signed_duration_since(date1).num_days() as i128 * 86400;
        let total_nanos = total_day_secs * 1_000_000_000 + time_diff as i128;
        let secs = total_nanos.div_euclid(1_000_000_000) as i64;
        let ns = total_nanos.rem_euclid(1_000_000_000) as i64;
        CypherDuration {
            months: 0,
            days: 0,
            seconds: secs,
            nanos: ns,
        }
    } else {
        CypherDuration {
            months: 0,
            days: 0,
            seconds: 0,
            nanos: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Truncation helpers
// ---------------------------------------------------------------------------

/// Public wrapper around `parse_offset` for use from eval.rs.
pub fn parse_offset_public(s: &str) -> Result<i32> {
    parse_offset(s).map(|off| off.local_minus_utc())
}

/// Truncate a date to the given unit, then apply optional map overrides.
pub fn truncate_date(unit: &str, val: &Value, map: &BTreeMap<String, Value>) -> Result<NaiveDate> {
    let date = extract_date(val)
        .ok_or_else(|| parse_error("truncate_date requires a temporal with a date component"))?;
    let mut d = truncate_date_core(unit, date)?;

    // Apply map overrides.
    if let Some(Value::I64(day)) = map.get("day") {
        d = d
            .with_day(*day as u32)
            .ok_or_else(|| parse_error(format!("invalid day: {day}")))?;
    }
    if let Some(Value::I64(dow)) = map.get("dayOfWeek") {
        // dayOfWeek override: keep the same week, change to the given day.
        // Monday = 1. The truncated date for 'week' is already Monday.
        let current_dow = d.weekday().num_days_from_monday() as i64 + 1;
        let delta = *dow - current_dow;
        d += chrono::Duration::days(delta);
    }
    if let Some(Value::I64(m)) = map.get("month") {
        d = d
            .with_month(*m as u32)
            .ok_or_else(|| parse_error(format!("invalid month: {m}")))?;
    }

    Ok(d)
}

/// Core date truncation logic.
fn truncate_date_core(unit: &str, date: NaiveDate) -> Result<NaiveDate> {
    let y = date.year();
    match unit {
        "millennium" => Ok(NaiveDate::from_ymd_opt((y / 1000) * 1000, 1, 1).unwrap()),
        "century" => Ok(NaiveDate::from_ymd_opt((y / 100) * 100, 1, 1).unwrap()),
        "decade" => Ok(NaiveDate::from_ymd_opt((y / 10) * 10, 1, 1).unwrap()),
        "year" => Ok(NaiveDate::from_ymd_opt(y, 1, 1).unwrap()),
        "weekYear" => {
            // Monday of ISO week 1 of the ISO week-year.
            let iso_year = date.iso_week().year();
            Ok(NaiveDate::from_isoywd_opt(iso_year, 1, chrono::Weekday::Mon).unwrap())
        }
        "quarter" => {
            let q = (date.month() - 1) / 3;
            let first_month = q * 3 + 1;
            Ok(NaiveDate::from_ymd_opt(y, first_month, 1).unwrap())
        }
        "month" => Ok(NaiveDate::from_ymd_opt(y, date.month(), 1).unwrap()),
        "week" => {
            // Monday of the current ISO week.
            let dow = date.weekday().num_days_from_monday() as i64;
            Ok(date - chrono::Duration::days(dow))
        }
        "day" | "hour" | "minute" | "second" | "millisecond" | "microsecond" => Ok(date),
        _ => Err(parse_error("unsupported truncation unit for date")),
    }
}

/// Truncate a time to the given unit, then apply optional map overrides.
pub fn truncate_time(unit: &str, val: &Value, map: &BTreeMap<String, Value>) -> Result<NaiveTime> {
    let time = extract_time(val)
        .ok_or_else(|| parse_error("truncate_time requires a temporal with a time component"))?;
    let mut t = truncate_time_core(unit, time)?;

    // Apply nano override from map.
    // The nanosecond override adds to the truncated nanos (fills in the zeroed-out
    // sub-precision digits). E.g. for 'millisecond' truncation with nanos=645000000,
    // {nanosecond: 2} produces 645000002.
    if let Some(Value::I64(ns)) = map.get("nanosecond") {
        let truncated_nanos = t.nanosecond();
        let new_nanos = truncated_nanos + *ns as u32;
        t = t
            .with_nanosecond(new_nanos)
            .ok_or_else(|| parse_error(format!("invalid nanosecond: {ns}")))?;
    }

    Ok(t)
}

/// Core time truncation logic.
fn truncate_time_core(unit: &str, time: NaiveTime) -> Result<NaiveTime> {
    match unit {
        "millennium" | "century" | "decade" | "year" | "weekYear" | "quarter" | "month"
        | "week" | "day" => Ok(NaiveTime::from_hms_opt(0, 0, 0).unwrap()),
        "hour" => Ok(NaiveTime::from_hms_opt(time.hour(), 0, 0).unwrap()),
        "minute" => Ok(NaiveTime::from_hms_opt(time.hour(), time.minute(), 0).unwrap()),
        "second" => Ok(NaiveTime::from_hms_opt(time.hour(), time.minute(), time.second()).unwrap()),
        "millisecond" => {
            let ms = time.nanosecond() / 1_000_000;
            Ok(NaiveTime::from_hms_nano_opt(
                time.hour(),
                time.minute(),
                time.second(),
                ms * 1_000_000,
            )
            .unwrap())
        }
        "microsecond" => {
            let us = time.nanosecond() / 1_000;
            Ok(
                NaiveTime::from_hms_nano_opt(time.hour(), time.minute(), time.second(), us * 1_000)
                    .unwrap(),
            )
        }
        _ => Err(parse_error("unsupported truncation unit for time")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_date_display() {
        let d = CypherDate(NaiveDate::from_ymd_opt(1984, 10, 11).unwrap());
        assert_eq!(d.to_string(), "1984-10-11");
    }

    #[test]
    fn test_date_parse() {
        let d = CypherDate::from_iso_string("1984-10-11").unwrap();
        assert_eq!(d.0, NaiveDate::from_ymd_opt(1984, 10, 11).unwrap());

        let d2 = CypherDate::from_iso_string("19841011").unwrap();
        assert_eq!(d2.0, d.0);

        let d3 = CypherDate::from_iso_string("1984").unwrap();
        assert_eq!(d3.0, NaiveDate::from_ymd_opt(1984, 1, 1).unwrap());

        let d4 = CypherDate::from_iso_string("1984-10").unwrap();
        assert_eq!(d4.0, NaiveDate::from_ymd_opt(1984, 10, 1).unwrap());
    }

    #[test]
    fn test_date_week_parse() {
        // 2015-W30-2 = Tuesday of week 30 in 2015
        let d = CypherDate::from_iso_string("2015-W30-2").unwrap();
        assert_eq!(
            d.0,
            NaiveDate::from_isoywd_opt(2015, 30, chrono::Weekday::Tue).unwrap()
        );

        let d2 = CypherDate::from_iso_string("2015W302").unwrap();
        assert_eq!(d2.0, d.0);

        // Default day = Monday
        let d3 = CypherDate::from_iso_string("2015-W30").unwrap();
        assert_eq!(
            d3.0,
            NaiveDate::from_isoywd_opt(2015, 30, chrono::Weekday::Mon).unwrap()
        );
    }

    #[test]
    fn test_date_ordinal_parse() {
        let d = CypherDate::from_iso_string("1984-202").unwrap();
        assert_eq!(d.0, NaiveDate::from_yo_opt(1984, 202).unwrap());

        let d2 = CypherDate::from_iso_string("1984202").unwrap();
        assert_eq!(d2.0, d.0);
    }

    #[test]
    fn test_local_time_display() {
        let t = CypherLocalTime(NaiveTime::from_hms_nano_opt(12, 31, 14, 645_876_123).unwrap());
        assert_eq!(t.to_string(), "12:31:14.645876123");

        let t2 = CypherLocalTime(NaiveTime::from_hms_opt(21, 40, 0).unwrap());
        assert_eq!(t2.to_string(), "21:40");

        let t3 = CypherLocalTime(NaiveTime::from_hms_opt(21, 40, 32).unwrap());
        assert_eq!(t3.to_string(), "21:40:32");

        let t4 = CypherLocalTime(NaiveTime::from_hms_nano_opt(21, 40, 32, 142_000_000).unwrap());
        assert_eq!(t4.to_string(), "21:40:32.142");
    }

    #[test]
    fn test_time_display() {
        let utc = FixedOffset::east_opt(0).unwrap();
        let plus1 = FixedOffset::east_opt(3600).unwrap();
        let minus130 = FixedOffset::west_opt(5400).unwrap();

        let t1 = CypherTime(
            NaiveTime::from_hms_nano_opt(21, 40, 32, 142_000_000).unwrap(),
            utc,
        );
        assert_eq!(t1.to_string(), "21:40:32.142Z");

        let t2 = CypherTime(NaiveTime::from_hms_opt(21, 40, 32).unwrap(), plus1);
        assert_eq!(t2.to_string(), "21:40:32+01:00");

        let t3 = CypherTime(NaiveTime::from_hms_opt(21, 40, 0).unwrap(), minus130);
        assert_eq!(t3.to_string(), "21:40-01:30");
    }

    #[test]
    fn test_datetime_display() {
        let utc = FixedOffset::east_opt(0).unwrap();
        let plus1 = FixedOffset::east_opt(3600).unwrap();

        let dt1 = CypherDateTime(
            NaiveDate::from_ymd_opt(1984, 10, 11)
                .unwrap()
                .and_hms_opt(12, 31, 14)
                .unwrap(),
            utc,
            None,
        );
        assert_eq!(dt1.to_string(), "1984-10-11T12:31:14Z");

        let dt2 = CypherDateTime(
            NaiveDate::from_ymd_opt(1984, 10, 11)
                .unwrap()
                .and_hms_opt(12, 31, 14)
                .unwrap(),
            plus1,
            None,
        );
        assert_eq!(dt2.to_string(), "1984-10-11T12:31:14+01:00");
    }

    #[test]
    fn test_duration_display() {
        let d1 = CypherDuration {
            months: 0,
            days: 14,
            seconds: 58320,
            nanos: 0,
        };
        assert_eq!(d1.to_string(), "P14DT16H12M");

        let d2 = CypherDuration {
            months: 14,
            days: 3,
            seconds: 0,
            nanos: 0,
        };
        assert_eq!(d2.to_string(), "P1Y2M3D");

        let d3 = CypherDuration {
            months: 0,
            days: 0,
            seconds: 0,
            nanos: 0,
        };
        assert_eq!(d3.to_string(), "PT0S");

        let d4 = CypherDuration {
            months: 0,
            days: 0,
            seconds: -79200,
            nanos: 0,
        };
        assert_eq!(d4.to_string(), "PT-22H");
    }

    #[test]
    fn test_duration_parse() {
        let d = CypherDuration::from_iso_string("P14DT16H12M").unwrap();
        assert_eq!(d.days, 14);
        assert_eq!(d.seconds, 16 * 3600 + 12 * 60);

        let d2 = CypherDuration::from_iso_string("P1Y2M3D").unwrap();
        assert_eq!(d2.months, 14);
        assert_eq!(d2.days, 3);

        let d3 = CypherDuration::from_iso_string("PT0S").unwrap();
        assert_eq!(d3.months, 0);
        assert_eq!(d3.days, 0);
        assert_eq!(d3.seconds, 0);
    }

    #[test]
    fn test_time_parse() {
        let t = CypherLocalTime::from_iso_string("12:31:14.645876123").unwrap();
        assert_eq!(
            t.0,
            NaiveTime::from_hms_nano_opt(12, 31, 14, 645_876_123).unwrap()
        );

        let t2 = CypherLocalTime::from_iso_string("21:40").unwrap();
        assert_eq!(t2.0, NaiveTime::from_hms_opt(21, 40, 0).unwrap());

        let t3 = CypherLocalTime::from_iso_string("14").unwrap();
        assert_eq!(t3.0, NaiveTime::from_hms_opt(14, 0, 0).unwrap());
    }

    #[test]
    fn test_offset_parse() {
        let t = CypherTime::from_iso_string("21:40:32.142Z").unwrap();
        assert_eq!(t.1, FixedOffset::east_opt(0).unwrap());

        let t2 = CypherTime::from_iso_string("21:40:32+01:00").unwrap();
        assert_eq!(t2.1, FixedOffset::east_opt(3600).unwrap());

        let t3 = CypherTime::from_iso_string("21:40-01:30").unwrap();
        assert_eq!(t3.1, FixedOffset::west_opt(5400).unwrap());
    }

    #[test]
    fn test_serde_roundtrip_date() {
        let d = CypherDate(NaiveDate::from_ymd_opt(2024, 6, 15).unwrap());
        let bytes = rmp_serde::to_vec_named(&d).unwrap();
        let d2: CypherDate = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(d, d2);
    }

    #[test]
    fn test_serde_roundtrip_duration() {
        let d = CypherDuration {
            months: 14,
            days: 3,
            seconds: 7200,
            nanos: 500,
        };
        let bytes = rmp_serde::to_vec_named(&d).unwrap();
        let d2: CypherDuration = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(d, d2);
    }

    #[test]
    fn test_date_from_map() {
        let mut map = BTreeMap::new();
        map.insert("year".to_string(), Value::I64(1984));
        map.insert("month".to_string(), Value::I64(10));
        map.insert("day".to_string(), Value::I64(11));
        let d = CypherDate::from_map(&map).unwrap();
        assert_eq!(d.0, NaiveDate::from_ymd_opt(1984, 10, 11).unwrap());
    }

    #[test]
    fn test_duration_from_map() {
        let mut map = BTreeMap::new();
        map.insert("years".to_string(), Value::I64(1));
        map.insert("months".to_string(), Value::I64(2));
        map.insert("days".to_string(), Value::I64(3));
        map.insert("hours".to_string(), Value::I64(4));
        let d = CypherDuration::from_map(&map).unwrap();
        assert_eq!(d.months, 14);
        assert_eq!(d.days, 3);
        assert_eq!(d.seconds, 14400);
    }
}
