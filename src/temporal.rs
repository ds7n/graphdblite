//! openCypher temporal types over plain owned fields, with jiff as the calendar engine.
//!
//! Provides `CypherDate`, `CypherLocalTime`, `CypherTime`, `CypherLocalDateTime`,
//! `CypherDateTime`, and `CypherDuration` with ISO 8601 parsing, display, and
//! MessagePack-compatible serde via integer fields.
//!
//! Design: our types own the data, jiff is the calculator. Each value is a
//! plain struct of integers (so it can be stored, displayed and decoded for any
//! year the old chrono-based builds accepted), and calendar math (weekdays, ISO
//! weeks, ordinals, month and day arithmetic, time zone resolution) goes
//! through `to_jiff()` conversions. A value outside jiff's range (years
//! -9999..=9999) can still be decoded and displayed but errors with
//! `NumberOutOfRange` when used in arithmetic or conversion.
//!
//! Storage invariant: the `Serialize` impls emit exactly the positional fields
//! and integer types of the original on-disk format (schema version 2). Do not
//! reorder, add or retype fields without a migration.

use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};

use jiff::civil;
use jiff::tz::{AmbiguousOffset, Offset, TimeZone};
use jiff::{SignedDuration, Span, Timestamp};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::types::{GraphError, QueryPhase, Result, Value};

/// Lowest year a newly constructed temporal value may have.
pub(crate) const MIN_YEAR: i32 = -9999;
/// Highest year a newly constructed temporal value may have.
pub(crate) const MAX_YEAR: i32 = 9999;

/// Largest absolute year the previous (chrono) builds could store. Decoding
/// keeps accepting this whole range so old database files stay readable.
const LEGACY_MAX_YEAR: i32 = 262_142;
const LEGACY_MIN_YEAR: i32 = -262_143;

const NANOS_PER_SEC: i64 = 1_000_000_000;
const SECS_PER_DAY: i64 = 86_400;

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

/// The `NumberOutOfRange` error for a year outside the supported range.
fn year_out_of_range() -> GraphError {
    GraphError::number_out_of_range(
        QueryPhase::Runtime,
        format!("temporal year is out of range (supported years: {MIN_YEAR} to {MAX_YEAR})"),
    )
}

/// Reject a year outside `MIN_YEAR..=MAX_YEAR` (new values only).
fn check_year(year: i32) -> Result<()> {
    if (MIN_YEAR..=MAX_YEAR).contains(&year) {
        Ok(())
    } else {
        Err(year_out_of_range())
    }
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

/// Whether `secs` is a usable UTC offset (strictly inside one day).
fn offset_in_range(secs: i32) -> bool {
    secs.abs() < 86_400
}

/// Validate an offset in seconds, with the parser's error message.
fn checked_offset(secs: i32) -> Result<i32> {
    if offset_in_range(secs) {
        Ok(secs)
    } else {
        Err(parse_error("offset out of range"))
    }
}

/// Parse a timezone offset string into seconds east of UTC.
fn parse_offset(s: &str) -> Result<i32> {
    ensure_ascii(s, "offset")?;
    if s == "Z" || s == "z" {
        return Ok(0);
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
        // +HH:MM:SS: historical offsets with seconds precision
        let hh = body[..2]
            .parse::<i32>()
            .map_err(|e| parse_error(e.to_string()))?;
        let mm = body[3..5]
            .parse::<i32>()
            .map_err(|e| parse_error(e.to_string()))?;
        let ss = body[6..8]
            .parse::<i32>()
            .map_err(|e| parse_error(e.to_string()))?;
        return checked_offset(sign * (hh * 3600 + mm * 60 + ss));
    } else {
        return Err(parse_error("invalid offset"));
    };
    checked_offset(sign * (h * 3600 + m * 60))
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

/// Parse a time string (without offset) into a `CypherLocalTime`.
fn parse_time_str(s: &str) -> Result<CypherLocalTime> {
    ensure_ascii(s, "time")?;
    let invalid = "invalid time";
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
            return CypherLocalTime::new_with(h, m, 0, 0, invalid);
        }
        let sec_part = parts[2];
        if let Some(dot_pos) = sec_part.find('.') {
            let sec: u32 = sec_part[..dot_pos]
                .parse()
                .map_err(|e: std::num::ParseIntError| parse_error(e.to_string()))?;
            let frac = &sec_part[dot_pos + 1..];
            let nano = parse_frac_nanos(frac)?;
            CypherLocalTime::new_with(h, m, sec, nano, invalid)
        } else {
            let sec: u32 = sec_part
                .parse()
                .map_err(|e: std::num::ParseIntError| parse_error(e.to_string()))?;
            CypherLocalTime::new_with(h, m, sec, 0, invalid)
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
        CypherLocalTime::new_with(h, m, sec, nano, invalid)
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

/// Parse a date string into a `CypherDate`.
fn parse_date_str(s: &str) -> Result<CypherDate> {
    ensure_ascii(s, "date")?;
    // Week date: YYYY-Www-D or YYYYWwwD or YYYY-Www or YYYYWww
    if s.contains('W') {
        return parse_week_date(s);
    }

    let invalid = "invalid date";
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
                CypherDate::new_with(y, m, d, invalid)
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
                    CypherDate::from_ordinal(y, part2, "invalid ordinal date")
                } else {
                    // YYYY-MM (day defaults to 1)
                    CypherDate::new_with(y, part2, 1, invalid)
                }
            }
            _ => Err(parse_error(invalid)),
        }
    } else {
        // Compact forms: YYYYMMDD, YYYYMM, YYYY, YYYYDDD
        match s.len() {
            8 => {
                let y: i32 = parse_num(&s[..4])?;
                let m: u32 = parse_num(&s[4..6])?;
                let d: u32 = parse_num(&s[6..])?;
                CypherDate::new_with(y, m, d, invalid)
            }
            7 => {
                // YYYYDDD
                let y: i32 = parse_num(&s[..4])?;
                let d: u32 = parse_num(&s[4..])?;
                CypherDate::from_ordinal(y, d, "invalid ordinal date")
            }
            6 => {
                // YYYYMM
                let y: i32 = parse_num(&s[..4])?;
                let m: u32 = parse_num(&s[4..])?;
                CypherDate::new_with(y, m, 1, invalid)
            }
            4 => {
                let y: i32 = parse_num(s)?;
                CypherDate::new_with(y, 1, 1, invalid)
            }
            _ => Err(parse_error(invalid)),
        }
    }
}

/// Map an ISO weekday number (Monday = 1) to a jiff weekday.
fn weekday_from_number(dow: u32) -> Option<civil::Weekday> {
    i8::try_from(dow)
        .ok()
        .and_then(|d| civil::Weekday::from_monday_one_offset(d).ok())
}

/// Parse ISO week date forms.
fn parse_week_date(s: &str) -> Result<CypherDate> {
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
    let weekday = weekday_from_number(day).ok_or_else(|| parse_error("invalid day of week"))?;
    CypherDate::from_iso_week(year, week, weekday, "invalid week date")
}

/// Format a time using the TCK local-time rules:
/// Always HH:MM, add :SS if seconds nonzero, add .nnnnnnnnn (trailing zeros trimmed) if sub-second nonzero.
fn fmt_local_time(t: &CypherLocalTime, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{:02}:{:02}", t.hour, t.minute)?;
    let sec = t.second;
    let nano = t.nanosecond;
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

/// Format an offset in seconds for display. UTC (+00:00) renders as `Z`.
pub fn fmt_offset_public(secs: i32) -> String {
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

fn fmt_offset(secs: i32, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&fmt_offset_public(secs))
}

/// Extract the source offset from a base temporal value in the map (if any).
/// Returns `Some(offset)` when the base `time` or `datetime` key carries a timezone.
fn base_source_offset(map: &BTreeMap<String, Value>) -> Option<i32> {
    match map.get("time").or_else(|| map.get("datetime")) {
        Some(Value::Time(t)) => Some(t.offset_seconds),
        Some(Value::DateTime(dt)) => Some(dt.offset_seconds),
        _ => None,
    }
}

/// Re-resolve the source offset at a new local date-time when the base temporal
/// has a named timezone. This accounts for DST changes when the date differs
/// from the original temporal.
fn base_source_offset_at(map: &BTreeMap<String, Value>, ndt: &CypherLocalDateTime) -> Option<i32> {
    match map.get("time").or_else(|| map.get("datetime")) {
        Some(Value::Time(t)) => Some(t.offset_seconds),
        Some(Value::DateTime(dt)) => {
            if let Some(ref tz_name) = dt.tz_name {
                // Re-resolve at the new date/time for DST awareness.
                if let Ok((off, _)) = resolve_tz_name_at(tz_name, ndt) {
                    Some(off)
                } else {
                    Some(dt.offset_seconds)
                }
            } else {
                Some(dt.offset_seconds)
            }
        }
        _ => None,
    }
}

/// Build a time from map keys (hour, minute, second, nanosecond, millisecond, microsecond).
/// If a `time` key is present (LocalTime, Time, LocalDateTime, or DateTime), its components
/// are used as defaults for any unspecified fields.
fn time_from_map(map: &BTreeMap<String, Value>) -> Result<CypherLocalTime> {
    let base_time = match map.get("time").or_else(|| map.get("datetime")) {
        Some(Value::LocalTime(t)) => Some(*t),
        Some(Value::Time(t)) => Some(t.time),
        Some(Value::LocalDateTime(dt)) => Some(dt.time),
        Some(Value::DateTime(dt)) => Some(dt.local.time),
        _ => None,
    };

    let invalid = "invalid time components";
    let h = get_int::<u32>(map, "hour")?
        .or_else(|| base_time.map(|t| u32::from(t.hour)))
        .unwrap_or(0);
    let m = get_int::<u32>(map, "minute")?
        .or_else(|| base_time.map(|t| u32::from(t.minute)))
        .unwrap_or(0);
    let s = get_int::<u32>(map, "second")?
        .or_else(|| base_time.map(|t| u32::from(t.second)))
        .unwrap_or(0);
    let nano = get_i64(map, "nanosecond")
        .or_else(|| base_time.map(|t| i64::from(t.nanosecond) % NANOS_PER_SEC))
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
        .ok_or_else(|| parse_error(invalid))?;
    CypherLocalTime::new_with(h, m, s, nano, invalid)
}

/// Build a date from map keys (year, month, day, week, dayOfWeek, ordinalDay, quarter, dayOfQuarter).
/// If a `date` key is present (Date, LocalDateTime, or DateTime), its components are used as
/// defaults for any unspecified fields.
fn date_from_map(map: &BTreeMap<String, Value>) -> Result<CypherDate> {
    // Check for a base `date` or `datetime` temporal value to project from.
    let base_date = match map.get("date").or_else(|| map.get("datetime")) {
        Some(Value::Date(d)) => Some(*d),
        Some(Value::LocalDateTime(dt)) => Some(dt.date),
        Some(Value::DateTime(dt)) => Some(dt.local.date),
        _ => None,
    };

    // The year comes from the map or the base date; there is no default.
    let year = |iso_week_year: bool| -> Result<i32> {
        if let Some(y) = get_int::<i32>(map, "year")? {
            return Ok(y);
        }
        let base = base_date.ok_or_else(|| invalid_map("year must be specified"))?;
        if iso_week_year {
            base.iso_week().map(|(y, _)| y)
        } else {
            Ok(base.year)
        }
    };

    if let Some(week) = get_int::<u32>(map, "week")? {
        // For ISO week dates, the year should be the ISO week year, not the calendar year.
        let year = year(true)?;
        let dow = match get_int::<u32>(map, "dayOfWeek")? {
            Some(dow) => dow,
            None => match base_date {
                Some(d) => d.weekday()?,
                None => 1,
            },
        };
        let weekday = weekday_from_number(dow)
            .ok_or_else(|| parse_error(format!("invalid dayOfWeek: {dow}")))?;
        CypherDate::from_iso_week(year, week, weekday, "invalid week date components")
    } else if let Some(ord) = get_int::<u32>(map, "ordinalDay")? {
        let year = year(false)?;
        CypherDate::from_ordinal(year, ord, "invalid ordinal date components")
    } else if let Some(quarter) = get_i64(map, "quarter") {
        if !(1..=4).contains(&quarter) {
            return Err(invalid_map(format!("quarter {quarter} is out of range")));
        }
        let year = year(false)?;
        let quarter_start_month = ((quarter - 1) * 3 + 1) as u32;
        if let Some(doq) = get_i64(map, "dayOfQuarter") {
            // Explicit dayOfQuarter: count from first day of the quarter.
            let start = CypherDate::new_with(year, quarter_start_month, 1, "invalid quarter")?;
            doq.checked_sub(1)
                .and_then(|delta| start.add_days(delta).ok())
                .ok_or_else(|| parse_error("dayOfQuarter out of range"))
        } else if let Some(bd) = base_date {
            // No explicit dayOfQuarter but base date present: preserve month-offset
            // within the quarter and day-of-month from the base date.
            let base_month_in_quarter = (u32::from(bd.month) - 1) % 3; // 0, 1, or 2
            let month = quarter_start_month + base_month_in_quarter;
            let day = get_int::<u32>(map, "day")?.unwrap_or(u32::from(bd.day));
            CypherDate::new_with(year, month, day, "invalid quarter date")
        } else {
            // No base date, no dayOfQuarter: first day of the quarter.
            CypherDate::new_with(year, quarter_start_month, 1, "invalid quarter")
        }
    } else {
        let year = year(false)?;
        let month = get_int::<u32>(map, "month")?
            .or_else(|| base_date.map(|d| u32::from(d.month)))
            .unwrap_or(1);
        let day = get_int::<u32>(map, "day")?
            .or_else(|| base_date.map(|d| u32::from(d.day)))
            .unwrap_or(1);
        CypherDate::new_with(year, month, day, "invalid date components")
    }
}

/// Whether a `timezone` value is an offset literal (`+01:00`, `Z`) rather
/// than an IANA zone name.
fn is_offset_literal(s: &str) -> bool {
    s.starts_with('+') || s.starts_with('-') || s == "Z" || s == "z"
}

/// Extract an offset from the `timezone` map key.
/// Returns `(offset_seconds, Option<tz_name>)`.
fn offset_from_map(map: &BTreeMap<String, Value>) -> Result<(i32, Option<String>)> {
    match map.get("timezone") {
        Some(Value::String(s)) => {
            if is_offset_literal(s) {
                Ok((parse_offset(s)?, None))
            } else {
                // IANA timezone name: resolve at "now" (no date-time context).
                resolve_tz_name_now(s)
            }
        }
        _ => {
            // No explicit timezone: try to inherit from a `time` or `datetime` base temporal.
            // If the base temporal has no offset (e.g. LocalTime, LocalDateTime), default to UTC.
            match map.get("time").or_else(|| map.get("datetime")) {
                Some(Value::Time(t)) => Ok((t.offset_seconds, None)),
                Some(Value::DateTime(dt)) => Ok((dt.offset_seconds, dt.tz_name.clone())),
                // Default to UTC when no timezone source is available.
                // This covers `time({hour: 12})` and `datetime({date: d})`.
                _ => Ok((0, None)),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Time zones
// ---------------------------------------------------------------------------

/// Look up an IANA zone by its exact name. Lookup is case-sensitive (jiff
/// alone would also accept `europe/stockholm`; the old chrono-tz build did
/// not, and the stored name must stay canonical).
pub(crate) fn lookup_tz(name: &str) -> Option<TimeZone> {
    let tz = TimeZone::get(name).ok()?;
    (tz.iana_name() == Some(name)).then_some(tz)
}

/// The zone for `name`, or the "unknown timezone" parse error.
fn require_tz(name: &str) -> Result<TimeZone> {
    lookup_tz(name).ok_or_else(|| parse_error("unknown timezone"))
}

/// The offsets in force for a local date-time in `tz`: one offset, or two
/// when the local time falls in a DST gap or overlap.
pub(crate) fn tz_local_offsets(
    tz: &TimeZone,
    local: &CypherLocalDateTime,
) -> Result<AmbiguousOffset> {
    Ok(tz.to_ambiguous_timestamp(local.to_jiff()?).offset())
}

/// The offset (seconds) of `local` in `tz`, taking the earlier instant in an
/// overlap (the pre-transition offset). `None` when `local` falls in a gap.
pub(crate) fn earliest_offset(tz: &TimeZone, local: &CypherLocalDateTime) -> Result<Option<i32>> {
    Ok(match tz_local_offsets(tz, local)? {
        AmbiguousOffset::Unambiguous { offset } => Some(offset.seconds()),
        AmbiguousOffset::Fold { before, .. } => Some(before.seconds()),
        AmbiguousOffset::Gap { .. } => None,
    })
}

/// The timestamp of a civil date-time read as UTC.
fn utc_timestamp(utc: &CypherLocalDateTime) -> Result<Timestamp> {
    Offset::UTC
        .to_timestamp(utc.to_jiff()?)
        .map_err(|_| out_of_range("date-time"))
}

/// The offset (seconds) in force in `tz` at the UTC instant `utc`.
pub(crate) fn tz_offset_at_utc(tz: &TimeZone, utc: &CypherLocalDateTime) -> Result<i32> {
    Ok(tz.to_offset(utc_timestamp(utc)?).seconds())
}

/// Convert the UTC instant `utc` to local wall-clock time in `tz`.
/// Returns the local date-time and its offset in seconds.
pub(crate) fn tz_utc_to_local(
    tz: &TimeZone,
    utc: &CypherLocalDateTime,
) -> Result<(CypherLocalDateTime, i32)> {
    let ts = utc_timestamp(utc)?;
    let offset = tz.to_offset(ts);
    Ok((
        CypherLocalDateTime::from_jiff(offset.to_datetime(ts)),
        offset.seconds(),
    ))
}

/// Resolve an IANA timezone name to offset using the current time.
/// Used when no specific datetime context is available (e.g. CypherTime).
fn resolve_tz_name_now(name: &str) -> Result<(i32, Option<String>)> {
    let tz = require_tz(name)?;
    let off = tz.to_offset(Timestamp::now()).seconds();
    Ok((off, Some(name.to_string())))
}

/// Resolve an IANA timezone name to offset at a specific local date-time.
/// Uses the earliest valid local time (handles DST transitions); a time in a
/// DST gap is an error.
fn resolve_tz_name_at(name: &str, dt: &CypherLocalDateTime) -> Result<(i32, Option<String>)> {
    let tz = require_tz(name)?;
    let off = earliest_offset(&tz, dt)?
        .ok_or_else(|| parse_error("ambiguous or invalid datetime in timezone"))?;
    Ok((off, Some(name.to_string())))
}

// ---------------------------------------------------------------------------
// Calendar helpers independent of jiff's range
// ---------------------------------------------------------------------------

/// Whether `year` is a leap year in the proleptic Gregorian calendar.
fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days in a month of the proleptic Gregorian calendar, for any year.
fn days_in_month_any_year(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap_year(year) => 29,
        _ => 28,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Hinnant's civil
/// algorithm). Total over every `i32` year, so ordering keys and epoch
/// seconds never depend on jiff's narrower range and never fail.
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (i64::from(month) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ===========================================================================
// CypherDate
// ===========================================================================

/// openCypher `Date`: a calendar date without time or timezone.
///
/// New values have years in `-9999..=9999`; a legacy stored value may lie
/// outside that range (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CypherDate {
    pub year: i32,
    pub month: u8,
    pub day: u8,
}

impl fmt::Display for CypherDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl Serialize for CypherDate {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("CypherDate", 3)?;
        s.serialize_field("year", &self.year)?;
        s.serialize_field("month", &u32::from(self.month))?;
        s.serialize_field("day", &u32::from(self.day))?;
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
                CypherDate::decoded(year, month, day)
                    .ok_or_else(|| de::Error::custom("invalid date"))
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
                CypherDate::decoded(year, month, day)
                    .ok_or_else(|| de::Error::custom("invalid date"))
            }
        }
        deserializer.deserialize_any(CypherDateVisitor)
    }
}

impl CypherDate {
    /// Build a date, rejecting a year outside `-9999..=9999` (`NumberOutOfRange`)
    /// and an impossible month/day (`InvalidArgumentValue`).
    pub fn new(year: i32, month: u32, day: u32) -> Result<Self> {
        Self::new_with(year, month, day, "invalid date")
    }

    /// [`Self::new`] with a caller-chosen message for a bad month/day.
    fn new_with(year: i32, month: u32, day: u32, invalid: &str) -> Result<Self> {
        check_year(year)?;
        let (Ok(month), Ok(day)) = (i8::try_from(month), i8::try_from(day)) else {
            return Err(parse_error(invalid));
        };
        civil::Date::new(year as i16, month, day)
            .map(Self::from_jiff)
            .map_err(|_| parse_error(invalid))
    }

    /// The date `ordinal` days into `year` (1 = January 1st).
    fn from_ordinal(year: i32, ordinal: u32, invalid: &str) -> Result<Self> {
        check_year(year)?;
        let ordinal = i16::try_from(ordinal).map_err(|_| parse_error(invalid))?;
        civil::Date::new(year as i16, 1, 1)
            .and_then(|jan1| jan1.with().day_of_year(ordinal).build())
            .map(Self::from_jiff)
            .map_err(|_| parse_error(invalid))
    }

    /// The date of ISO week `week`, `weekday` of ISO week-year `year`.
    fn from_iso_week(year: i32, week: u32, weekday: civil::Weekday, invalid: &str) -> Result<Self> {
        check_year(year)?;
        let week = i8::try_from(week).map_err(|_| parse_error(invalid))?;
        civil::ISOWeekDate::new(year as i16, week, weekday)
            .map(|iso| Self::from_jiff(iso.date()))
            .map_err(|_| parse_error(invalid))
    }

    /// Decode stored fields. Accepts exactly what the old chrono-based build
    /// accepted (years -262143..=262142), so legacy files stay readable.
    fn decoded(year: i32, month: u32, day: u32) -> Option<Self> {
        let valid = (LEGACY_MIN_YEAR..=LEGACY_MAX_YEAR).contains(&year)
            && (1..=12).contains(&month)
            && (1..=days_in_month_any_year(year, month)).contains(&day);
        valid.then_some(Self {
            year,
            month: month as u8,
            day: day as u8,
        })
    }

    pub(crate) fn from_jiff(d: civil::Date) -> Self {
        Self {
            year: i32::from(d.year()),
            month: d.month() as u8,
            day: d.day() as u8,
        }
    }

    /// The jiff date, or `NumberOutOfRange` for a legacy value outside
    /// jiff's years -9999..=9999.
    pub(crate) fn to_jiff(self) -> Result<civil::Date> {
        let year = i16::try_from(self.year).map_err(|_| out_of_range("date"))?;
        civil::Date::new(year, self.month as i8, self.day as i8).map_err(|_| out_of_range("date"))
    }

    /// Day of the year, 1-based.
    pub(crate) fn ordinal(self) -> Result<u32> {
        Ok(self.to_jiff()?.day_of_year() as u32)
    }

    /// `(ISO week-year, ISO week)`.
    pub(crate) fn iso_week(self) -> Result<(i32, u32)> {
        let w = self.to_jiff()?.iso_week_date();
        Ok((i32::from(w.year()), w.week() as u32))
    }

    /// ISO day of week, Monday = 1.
    pub(crate) fn weekday(self) -> Result<u32> {
        Ok(self.to_jiff()?.weekday().to_monday_one_offset() as u32)
    }

    /// Calendar quarter, 1 to 4.
    pub(crate) fn quarter(self) -> u32 {
        (u32::from(self.month) - 1) / 3 + 1
    }

    /// Days since 1970-01-01 (never fails, any year).
    pub(crate) fn epoch_days(self) -> i64 {
        days_from_civil(self.year, u32::from(self.month), u32::from(self.day))
    }

    /// Add whole months, clamping the day to the target month's last day.
    pub(crate) fn add_months(self, months: i64) -> Result<Self> {
        self.to_jiff()?;
        let too_far = || out_of_range("date after adding months");
        let total = (i64::from(self.year) * 12 + i64::from(self.month) - 1)
            .checked_add(months)
            .ok_or_else(too_far)?;
        let year = i32::try_from(total.div_euclid(12)).map_err(|_| too_far())?;
        if !(MIN_YEAR..=MAX_YEAR).contains(&year) {
            return Err(too_far());
        }
        let month = total.rem_euclid(12) as i8 + 1;
        let first = civil::Date::new(year as i16, month, 1).map_err(|_| too_far())?;
        Ok(Self {
            year,
            month: month as u8,
            day: self.day.min(first.days_in_month() as u8),
        })
    }

    /// Add whole days (negative to subtract).
    pub(crate) fn add_days(self, days: i64) -> Result<Self> {
        let too_far = || out_of_range("date after adding days");
        let span = Span::new().try_days(days).map_err(|_| too_far())?;
        self.to_jiff()?
            .checked_add(span)
            .map(Self::from_jiff)
            .map_err(|_| too_far())
    }

    /// Whole days from `self` to `other` (negative when `other` is earlier).
    pub(crate) fn days_until(self, other: Self) -> Result<i64> {
        Ok(self.to_jiff()?.duration_until(other.to_jiff()?).as_secs() / SECS_PER_DAY)
    }

    /// Parse a date from an ISO 8601 string.
    pub fn from_iso_string(s: &str) -> Result<Self> {
        ensure_ascii(s, "date")?;
        parse_date_str(s)
    }

    /// Build a date from a property map.
    pub fn from_map(map: &BTreeMap<String, Value>) -> Result<Self> {
        check_keys(map, "date()", &[DATE_KEYS, &["date", "datetime"]])?;
        date_from_map(map)
    }
}

// ===========================================================================
// CypherLocalTime
// ===========================================================================

/// openCypher `LocalTime`: a time without timezone.
///
/// New values have `nanosecond < 1_000_000_000`; a legacy stored leap-second
/// value (`second == 59` with a larger nanosecond) decodes and displays as
/// stored but errors in arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CypherLocalTime {
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub nanosecond: u32,
}

impl fmt::Display for CypherLocalTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_local_time(self, f)
    }
}

impl Serialize for CypherLocalTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("CypherLocalTime", 4)?;
        s.serialize_field("hour", &u32::from(self.hour))?;
        s.serialize_field("minute", &u32::from(self.minute))?;
        s.serialize_field("second", &u32::from(self.second))?;
        s.serialize_field("nanosecond", &self.nanosecond)?;
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
                CypherLocalTime::decoded(h, m, s, n)
                    .ok_or_else(|| de::Error::custom("invalid time"))
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
                CypherLocalTime::decoded(h, m, s, n)
                    .ok_or_else(|| de::Error::custom("invalid time"))
            }
        }
        deserializer.deserialize_any(V)
    }
}

impl CypherLocalTime {
    /// Midnight, 00:00:00.
    pub(crate) const MIDNIGHT: Self = Self {
        hour: 0,
        minute: 0,
        second: 0,
        nanosecond: 0,
    };

    /// Build a time. Hour 0..24, minute and second 0..60, nanosecond
    /// 0..=999_999_999 (leap seconds are rejected); otherwise
    /// `InvalidArgumentValue`.
    pub fn new(hour: u32, minute: u32, second: u32, nanosecond: u32) -> Result<Self> {
        Self::new_with(hour, minute, second, nanosecond, "invalid time")
    }

    /// [`Self::new`] with a caller-chosen error message.
    fn new_with(
        hour: u32,
        minute: u32,
        second: u32,
        nanosecond: u32,
        invalid: &str,
    ) -> Result<Self> {
        if hour >= 24 || minute >= 60 || second >= 60 || nanosecond >= 1_000_000_000 {
            return Err(parse_error(invalid));
        }
        Ok(Self {
            hour: hour as u8,
            minute: minute as u8,
            second: second as u8,
            nanosecond,
        })
    }

    /// Decode stored fields. Accepts exactly what the old chrono-based build
    /// accepted, including a leap-second nanosecond (`>= 1e9` with second 59).
    fn decoded(hour: u32, minute: u32, second: u32, nanosecond: u32) -> Option<Self> {
        let valid = hour < 24
            && minute < 60
            && second < 60
            && (nanosecond < 1_000_000_000 || (nanosecond < 2_000_000_000 && second == 59));
        valid.then_some(Self {
            hour: hour as u8,
            minute: minute as u8,
            second: second as u8,
            nanosecond,
        })
    }

    pub(crate) fn from_jiff(t: civil::Time) -> Self {
        Self {
            hour: t.hour() as u8,
            minute: t.minute() as u8,
            second: t.second() as u8,
            nanosecond: t.subsec_nanosecond() as u32,
        }
    }

    /// The jiff time, or `NumberOutOfRange` for a legacy leap-second value.
    pub(crate) fn to_jiff(self) -> Result<civil::Time> {
        let nanos = i32::try_from(self.nanosecond)
            .ok()
            .filter(|n| *n < 1_000_000_000)
            .ok_or_else(|| out_of_range("time"))?;
        civil::Time::new(self.hour as i8, self.minute as i8, self.second as i8, nanos)
            .map_err(|_| out_of_range("time"))
    }

    /// Whole seconds since midnight.
    pub(crate) fn secs_of_day(self) -> u32 {
        u32::from(self.hour) * 3600 + u32::from(self.minute) * 60 + u32::from(self.second)
    }

    /// Nanoseconds since midnight.
    pub(crate) fn nanos_of_day(self) -> i64 {
        i64::from(self.secs_of_day()) * NANOS_PER_SEC + i64::from(self.nanosecond)
    }

    /// The time `secs` seconds (`< 86400`) after midnight plus `nanosecond`.
    fn from_secs_of_day(secs: u32, nanosecond: u32) -> Self {
        Self {
            hour: (secs / 3600) as u8,
            minute: (secs % 3600 / 60) as u8,
            second: (secs % 60) as u8,
            nanosecond,
        }
    }

    /// Parse a local time from an ISO 8601 string.
    pub fn from_iso_string(s: &str) -> Result<Self> {
        ensure_ascii(s, "local time")?;
        parse_time_str(s)
    }

    /// Build a local time from a property map.
    pub fn from_map(map: &BTreeMap<String, Value>) -> Result<Self> {
        check_keys(map, "localtime()", &[TIME_KEYS, &["time", "datetime"]])?;
        time_from_map(map)
    }
}

// ===========================================================================
// CypherTime
// ===========================================================================

/// openCypher `Time`: a time with timezone offset (seconds east of UTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CypherTime {
    pub time: CypherLocalTime,
    pub offset_seconds: i32,
}

impl fmt::Display for CypherTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_local_time(&self.time, f)?;
        fmt_offset(self.offset_seconds, f)
    }
}

impl Serialize for CypherTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("CypherTime", 5)?;
        s.serialize_field("hour", &u32::from(self.time.hour))?;
        s.serialize_field("minute", &u32::from(self.time.minute))?;
        s.serialize_field("second", &u32::from(self.time.second))?;
        s.serialize_field("nanosecond", &self.time.nanosecond)?;
        s.serialize_field("offset_seconds", &self.offset_seconds)?;
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
                decode_time(h, m, s, n, o)
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
                decode_time(h, m, s, n, o)
            }
        }

        fn decode_time<E: de::Error>(
            h: u32,
            m: u32,
            s: u32,
            n: u32,
            o: i32,
        ) -> std::result::Result<CypherTime, E> {
            let time =
                CypherLocalTime::decoded(h, m, s, n).ok_or_else(|| E::custom("invalid time"))?;
            if !offset_in_range(o) {
                return Err(E::custom("invalid offset"));
            }
            Ok(CypherTime {
                time,
                offset_seconds: o,
            })
        }
        deserializer.deserialize_any(V)
    }
}

impl CypherTime {
    /// A `(time of day in UTC, nanosecond)` key. Times of day wrap at
    /// midnight, so this orders by wall-clock UTC time, as comparison always has.
    pub(crate) fn utc_sort_key(self) -> (i64, u32) {
        let secs = i64::from(self.time.secs_of_day()) - i64::from(self.offset_seconds);
        (secs.rem_euclid(SECS_PER_DAY), self.time.nanosecond)
    }

    /// Parse a time with offset from an ISO 8601 string.
    /// When no offset is present, defaults to UTC (+00:00).
    pub fn from_iso_string(s: &str) -> Result<Self> {
        ensure_ascii(s, "time")?;
        let (time_part, off_part) = split_time_offset(s);
        let time = parse_time_str(time_part)?;
        let offset_seconds = match off_part {
            Some(o) => parse_offset(o)?,
            None => 0,
        };
        Ok(Self {
            time,
            offset_seconds,
        })
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
        let time = match base_source_offset(map) {
            Some(src_off) if map.contains_key("timezone") && src_off != off => {
                let secs = i64::from(t.secs_of_day()) + i64::from(off) - i64::from(src_off);
                CypherLocalTime::from_secs_of_day(
                    secs.rem_euclid(SECS_PER_DAY) as u32,
                    t.nanosecond % 1_000_000_000,
                )
            }
            _ => t,
        };
        Ok(Self {
            time,
            offset_seconds: off,
        })
    }
}

// ===========================================================================
// CypherLocalDateTime
// ===========================================================================

/// openCypher `LocalDateTime`: a date-time without timezone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CypherLocalDateTime {
    pub date: CypherDate,
    pub time: CypherLocalTime,
}

impl fmt::Display for CypherLocalDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = self.date;
        write!(f, "{:04}-{:02}-{:02}T", d.year, d.month, d.day)?;
        fmt_local_time(&self.time, f)
    }
}

impl Serialize for CypherLocalDateTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let (d, t) = (self.date, self.time);
        let mut s = serializer.serialize_struct("CypherLocalDateTime", 7)?;
        s.serialize_field("year", &d.year)?;
        s.serialize_field("month", &u32::from(d.month))?;
        s.serialize_field("day", &u32::from(d.day))?;
        s.serialize_field("hour", &u32::from(t.hour))?;
        s.serialize_field("minute", &u32::from(t.minute))?;
        s.serialize_field("second", &u32::from(t.second))?;
        s.serialize_field("nanosecond", &t.nanosecond)?;
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
                decode_local(year, month, day, h, m, s, n)
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
                decode_local(y, mo, dy, h, mi, s, n)
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Decode stored local date-time fields (legacy-tolerant, see
/// [`CypherDate::decoded`] and [`CypherLocalTime::decoded`]).
fn decode_local<E: de::Error>(
    year: i32,
    month: u32,
    day: u32,
    h: u32,
    m: u32,
    s: u32,
    n: u32,
) -> std::result::Result<CypherLocalDateTime, E> {
    let date = CypherDate::decoded(year, month, day).ok_or_else(|| E::custom("invalid date"))?;
    let time = CypherLocalTime::decoded(h, m, s, n).ok_or_else(|| E::custom("invalid time"))?;
    Ok(CypherLocalDateTime { date, time })
}

impl CypherLocalDateTime {
    pub(crate) fn from_jiff(dt: civil::DateTime) -> Self {
        Self {
            date: CypherDate::from_jiff(dt.date()),
            time: CypherLocalTime::from_jiff(dt.time()),
        }
    }

    /// The jiff date-time, or `NumberOutOfRange` for a legacy value jiff
    /// cannot represent.
    pub(crate) fn to_jiff(self) -> Result<civil::DateTime> {
        Ok(self.date.to_jiff()?.to_datetime(self.time.to_jiff()?))
    }

    /// Shift by an exact amount of seconds plus nanoseconds (both may be
    /// negative). Out of range is `NumberOutOfRange`.
    pub(crate) fn add_exact(self, seconds: i64, nanos: i64) -> Result<Self> {
        let too_far = || out_of_range("date-time after adding a duration");
        let delta = SignedDuration::from_secs(seconds)
            .checked_add(SignedDuration::from_nanos(nanos))
            .ok_or_else(too_far)?;
        self.to_jiff()?
            .checked_add(delta)
            .map(Self::from_jiff)
            .map_err(|_| too_far())
    }

    /// Parse from ISO 8601, e.g. `1984-10-11T12:31:14`.
    pub fn from_iso_string(s: &str) -> Result<Self> {
        ensure_ascii(s, "local datetime")?;
        let t_pos = s
            .find('T')
            .or_else(|| s.find('t'))
            .ok_or_else(|| parse_error("expected 'T' separator"))?;
        let date = parse_date_str(&s[..t_pos])?;
        let time = parse_time_str(&s[t_pos + 1..])?;
        Ok(Self { date, time })
    }

    /// Build from a property map.
    pub fn from_map(map: &BTreeMap<String, Value>) -> Result<Self> {
        check_keys(
            map,
            "localdatetime()",
            &[DATE_KEYS, TIME_KEYS, &["date", "time", "datetime"]],
        )?;
        let date = date_from_map(map)?;
        let time = time_from_map(map)?;
        Ok(Self { date, time })
    }
}

// ===========================================================================
// CypherDateTime
// ===========================================================================

/// openCypher `DateTime`: a date-time with timezone offset and optional IANA
/// timezone name. Equality compares the local date-time, the offset and the
/// zone name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CypherDateTime {
    pub local: CypherLocalDateTime,
    pub offset_seconds: i32,
    pub tz_name: Option<String>,
}

impl fmt::Display for CypherDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.local)?;
        fmt_offset(self.offset_seconds, f)?;
        if let Some(ref tz) = self.tz_name {
            write!(f, "[{tz}]")?;
        }
        Ok(())
    }
}

impl Serialize for CypherDateTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let (d, t) = (self.local.date, self.local.time);
        let field_count = if self.tz_name.is_some() { 9 } else { 8 };
        let mut s = serializer.serialize_struct("CypherDateTime", field_count)?;
        s.serialize_field("year", &d.year)?;
        s.serialize_field("month", &u32::from(d.month))?;
        s.serialize_field("day", &u32::from(d.day))?;
        s.serialize_field("hour", &u32::from(t.hour))?;
        s.serialize_field("minute", &u32::from(t.minute))?;
        s.serialize_field("second", &u32::from(t.second))?;
        s.serialize_field("nanosecond", &t.nanosecond)?;
        s.serialize_field("offset_seconds", &self.offset_seconds)?;
        if let Some(ref tz) = self.tz_name {
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
                decode_zoned(year, month, day, h, m, s, n, o, tz_name)
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
                decode_zoned(y, mo, dy, h, mi, s, n, o, tz_name)
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Decode stored zoned date-time fields (legacy-tolerant).
#[allow(clippy::too_many_arguments)]
fn decode_zoned<E: de::Error>(
    year: i32,
    month: u32,
    day: u32,
    h: u32,
    m: u32,
    s: u32,
    n: u32,
    offset_seconds: i32,
    tz_name: Option<String>,
) -> std::result::Result<CypherDateTime, E> {
    let local = decode_local(year, month, day, h, m, s, n)?;
    if !offset_in_range(offset_seconds) {
        return Err(E::custom("invalid offset"));
    }
    Ok(CypherDateTime {
        local,
        offset_seconds,
        tz_name,
    })
}

impl CypherDateTime {
    /// Seconds since the Unix epoch of this instant (never fails, any year).
    pub(crate) fn epoch_seconds(&self) -> i64 {
        self.local.date.epoch_days() * SECS_PER_DAY + i64::from(self.local.time.secs_of_day())
            - i64::from(self.offset_seconds)
    }

    /// Milliseconds since the Unix epoch of this instant.
    pub(crate) fn epoch_millis(&self) -> i64 {
        self.epoch_seconds() * 1000 + i64::from(self.local.time.nanosecond / 1_000_000)
    }

    /// `(epoch seconds, nanosecond)`: orders by instant, as comparison always has.
    pub(crate) fn utc_sort_key(&self) -> (i64, u32) {
        (self.epoch_seconds(), self.local.time.nanosecond)
    }

    /// A UTC date-time at the given instant.
    pub(crate) fn from_utc_timestamp(ts: Timestamp) -> Self {
        Self {
            local: CypherLocalDateTime::from_jiff(Offset::UTC.to_datetime(ts)),
            offset_seconds: 0,
            tz_name: None,
        }
    }

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
        let local = CypherLocalDateTime { date, time };

        let offset_seconds = if let Some(off_str) = off_part {
            parse_offset(off_str)?
        } else if let Some(ref tz) = tz_name {
            // No explicit offset but tz name present: resolve from tz at this datetime.
            resolve_tz_name_at(tz, &local)?.0
        } else {
            return Err(parse_error("CypherDateTime requires an offset or timezone"));
        };

        Ok(Self {
            local,
            offset_seconds,
            tz_name,
        })
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
        let date = date_from_map(map)?;
        let time = time_from_map(map)?;
        let ndt = CypherLocalDateTime { date, time };
        match map.get("timezone") {
            Some(Value::String(s)) => {
                let is_named = !is_offset_literal(s);
                let target_off = if is_named { 0 } else { parse_offset(s)? };
                // Convert local time when the base temporal has a different offset.
                // Use base_source_offset_at to re-resolve named timezones at the new date.
                let ndt = if let Some(src_off) = base_source_offset_at(map, &ndt) {
                    if is_named {
                        // Convert via UTC intermediary, then resolve target named tz.
                        let utc = ndt.add_exact(-i64::from(src_off), 0)?;
                        let tz = require_tz(s)?;
                        let (converted, off) = tz_utc_to_local(&tz, &utc)?;
                        return Ok(Self {
                            local: converted,
                            offset_seconds: off,
                            tz_name: Some(s.to_string()),
                        });
                    } else if src_off != target_off {
                        ndt.add_exact(i64::from(target_off) - i64::from(src_off), 0)?
                    } else {
                        ndt
                    }
                } else {
                    ndt
                };
                if is_named {
                    let (off, tz_name) = resolve_tz_name_at(s, &ndt)?;
                    Ok(Self {
                        local: ndt,
                        offset_seconds: off,
                        tz_name,
                    })
                } else {
                    Ok(Self {
                        local: ndt,
                        offset_seconds: target_off,
                        tz_name: None,
                    })
                }
            }
            _ => {
                // No explicit timezone: try to inherit from a `time` or `datetime` base temporal.
                match map.get("time").or_else(|| map.get("datetime")) {
                    Some(Value::Time(t)) => Ok(Self {
                        local: ndt,
                        offset_seconds: t.offset_seconds,
                        tz_name: None,
                    }),
                    Some(Value::DateTime(dt)) => {
                        // If the base has a named timezone, re-resolve at the new
                        // local date-time to account for DST changes.
                        if let Some(ref tz_name) = dt.tz_name {
                            let (off, tz) = resolve_tz_name_at(tz_name, &ndt)?;
                            Ok(Self {
                                local: ndt,
                                offset_seconds: off,
                                tz_name: tz,
                            })
                        } else {
                            Ok(Self {
                                local: ndt,
                                offset_seconds: dt.offset_seconds,
                                tz_name: None,
                            })
                        }
                    }
                    // Default to UTC when constructing from date/time components
                    // or from a date projection.
                    _ => Ok(Self {
                        local: ndt,
                        offset_seconds: 0,
                        tz_name: None,
                    }),
                }
            }
        }
    }

    /// Create from epoch seconds and nanoseconds (UTC).
    ///
    /// Errors with `NumberOutOfRange` when the instant is outside the
    /// supported years (-9999..=9999).
    pub fn from_epoch(seconds: i64, nanos: i64) -> Result<Self> {
        Self::from_epoch_nanos(i128::from(seconds) * 1_000_000_000 + i128::from(nanos))
    }

    /// Create from epoch milliseconds (UTC). Errors like [`Self::from_epoch`].
    pub fn from_epoch_millis(millis: i64) -> Result<Self> {
        Self::from_epoch_nanos(i128::from(millis) * 1_000_000)
    }

    /// Create from nanoseconds since the Unix epoch (UTC).
    fn from_epoch_nanos(total: i128) -> Result<Self> {
        let too_far = || out_of_range("epoch instant");
        let delta = SignedDuration::try_from_nanos_i128(total).ok_or_else(too_far)?;
        let utc = civil::DateTime::constant(1970, 1, 1, 0, 0, 0, 0)
            .checked_add(delta)
            .map_err(|_| too_far())?;
        Ok(Self {
            local: CypherLocalDateTime::from_jiff(utc),
            offset_seconds: 0,
            tz_name: None,
        })
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
                Some(Value::I64(n)) => Ok(i128::from(*n)),
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
                Ok(Self {
                    local: utc.local.add_exact(i64::from(off), 0)?,
                    offset_seconds: off,
                    tz_name: None,
                })
            }
            Some(Value::String(name)) => {
                let tz = require_tz(name)?;
                let (local, off) = tz_utc_to_local(&tz, &utc.local)?;
                Ok(Self {
                    local,
                    offset_seconds: off,
                    tz_name: Some(name.clone()),
                })
            }
            _ => Ok(utc),
        }
    }
}

// ===========================================================================
// CypherDuration
// ===========================================================================

/// openCypher `Duration`: calendar and clock components stored independently.
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

/// Extract a date from any temporal value that has a date component.
pub fn extract_date(val: &Value) -> Option<CypherDate> {
    match val {
        Value::Date(d) => Some(*d),
        Value::LocalDateTime(dt) => Some(dt.date),
        Value::DateTime(dt) => Some(dt.local.date),
        _ => None,
    }
}

/// Extract a time from any temporal value that has a time component.
/// For date-only values, returns midnight.
pub fn extract_time(val: &Value) -> Option<CypherLocalTime> {
    match val {
        Value::Date(_) => Some(CypherLocalTime::MIDNIGHT),
        Value::LocalTime(t) => Some(*t),
        Value::Time(t) => Some(t.time),
        Value::LocalDateTime(dt) => Some(dt.time),
        Value::DateTime(dt) => Some(dt.local.time),
        _ => None,
    }
}

/// Extract UTC offset seconds from a temporal value. Returns 0 for local types.
pub fn extract_offset_secs(val: &Value) -> i32 {
    match val {
        Value::Time(t) => t.offset_seconds,
        Value::DateTime(dt) => dt.offset_seconds,
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
        dt.tz_name.as_deref()
    } else {
        None
    }
}

/// Resolve the UTC offset for a local date/time interpreted in a named timezone.
/// Uses the earlier instant to pick the pre-transition offset for ambiguous
/// times. An unknown zone or a time in a DST gap resolves to offset 0.
fn resolve_offset_in_tz(date: CypherDate, time: CypherLocalTime, tz_name: &str) -> Result<i64> {
    let Some(tz) = lookup_tz(tz_name) else {
        return Ok(0);
    };
    let local = CypherLocalDateTime { date, time };
    Ok(earliest_offset(&tz, &local)?.map_or(0, i64::from))
}

/// Compute the effective UTC offset for `val` when paired with `other`.
///
/// When both sides are offset-aware, use actual offsets. When one side has a
/// named timezone and the other is a local type, resolve the local side's
/// offset in the named timezone (DST-aware). For time-only local values, the
/// date from the other side is used for DST resolution.
fn effective_offset_dst(val: &Value, other: &Value) -> Result<i64> {
    // Both offset-aware: standard path.
    if is_offset_aware(val) && is_offset_aware(other) {
        return Ok(i64::from(extract_offset_secs(val)));
    }

    // If val is offset-aware (DateTime with tz name), return its actual offset.
    if is_offset_aware(val) {
        // Check if the other side is a local type and we have a named tz.
        if extract_tz_name(val).is_some() && !is_offset_aware(other) {
            return Ok(i64::from(extract_offset_secs(val)));
        }
        return Ok(0);
    }

    // val is a local type: check if other has a named timezone.
    if let Some(tz_name) = extract_tz_name(other) {
        if !is_offset_aware(val) {
            // Resolve val's local date/time in the other side's timezone.
            let date = extract_date(val)
                .or_else(|| extract_date(other))
                .unwrap_or(CypherDate {
                    year: 1970,
                    month: 1,
                    day: 1,
                });
            let time = extract_time(val).unwrap_or(CypherLocalTime::MIDNIGHT);
            return resolve_offset_in_tz(date, time, tz_name);
        }
    }

    Ok(0)
}

/// Compute the calendar-month difference between two dates, adjusting if the
/// day-of-month means we haven't reached a full month boundary yet.
fn month_diff(d1: CypherDate, d2: CypherDate) -> Result<i64> {
    let mut months =
        (i64::from(d2.year) - i64::from(d1.year)) * 12 + i64::from(d2.month) - i64::from(d1.month);

    // Check if we overshot: adding `months` months to d1 should not go past d2.
    let adjusted = d1.add_months(months)?;
    if months > 0 && adjusted > d2 {
        months -= 1;
    } else if months < 0 && adjusted < d2 {
        months += 1;
    }
    Ok(months)
}

/// The nanoseconds-of-day of `val` in UTC terms, as used by every duration
/// helper: local time of day minus the effective offset against `other`.
fn utc_nanos_of_day(val: &Value, other: &Value) -> Result<i64> {
    let local = extract_time(val).map_or(0, CypherLocalTime::nanos_of_day);
    Ok(local - effective_offset_dst(val, other)? * NANOS_PER_SEC)
}

/// Compute `duration.between(lhs, rhs)`.
///
/// Returns a `CypherDuration` with months (calendar) + remainder as seconds/nanos.
/// A legacy value outside the supported years is `NumberOutOfRange`.
pub fn duration_between(lhs: &Value, rhs: &Value) -> Result<CypherDuration> {
    let d1 = extract_date(lhs);
    let d2 = extract_date(rhs);

    // When one side is time-only and the other has a date -> time-only comparison.
    let both_have_dates = d1.is_some() && d2.is_some() && has_date(lhs) && has_date(rhs);
    let either_time_only = is_time_only(lhs) || is_time_only(rhs);

    if either_time_only && !both_have_dates {
        // Pure time comparison: take the time from whichever has it.
        let lhs_nanos = utc_nanos_of_day(lhs, rhs)?;
        let rhs_nanos = utc_nanos_of_day(rhs, lhs)?;
        let diff_nanos = rhs_nanos - lhs_nanos;
        let total_secs = diff_nanos.div_euclid(NANOS_PER_SEC);
        let rem_nanos = diff_nanos.rem_euclid(NANOS_PER_SEC);
        // div_euclid/rem_euclid always gives a non-negative remainder. For a
        // negative diff, total_secs is negative and rem_nanos is non-negative;
        // CypherDuration stores them separately and Display combines the sign.
        return Ok(CypherDuration {
            months: 0,
            days: 0,
            seconds: total_secs,
            nanos: rem_nanos,
        });
    }

    let (Some(date1), Some(date2)) = (d1, d2) else {
        // No dates on either side: this shouldn't normally happen, but
        // handle it gracefully.
        return Ok(CypherDuration {
            months: 0,
            days: 0,
            seconds: 0,
            nanos: 0,
        });
    };

    // Both have dates: compute calendar months first.
    let months = month_diff(date1, date2)?;

    // Advance d1 by those months to get the remainder as days+time.
    let d1_advanced = date1.add_months(months)?;

    // Remainder days after month advancement.
    let mut day_diff = d1_advanced.days_until(date2)?;

    // Time-of-day difference (in nanos), adjusted for UTC offsets only when
    // both sides are offset-aware.
    let t1_nanos = utc_nanos_of_day(lhs, rhs)?;
    let t2_nanos = utc_nanos_of_day(rhs, lhs)?;
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

    Ok(CypherDuration {
        months,
        days: day_diff,
        seconds: time_diff_nanos.div_euclid(NANOS_PER_SEC),
        nanos: time_diff_nanos.rem_euclid(NANOS_PER_SEC),
    })
}

/// A zero duration.
fn zero_duration() -> CypherDuration {
    CypherDuration {
        months: 0,
        days: 0,
        seconds: 0,
        nanos: 0,
    }
}

/// Compute `duration.inMonths(lhs, rhs)`: only the months component.
pub fn duration_in_months(lhs: &Value, rhs: &Value) -> Result<CypherDuration> {
    let (Some(date1), Some(date2)) = (extract_date(lhs), extract_date(rhs)) else {
        return Ok(zero_duration());
    };
    if is_time_only(lhs) || is_time_only(rhs) {
        return Ok(zero_duration());
    }

    // For inMonths with datetime args, we need to check if the time pushes us
    // past a month boundary. Compare times at the month-boundary date.
    let mut months = month_diff(date1, date2)?;

    // If dates+times cross: e.g. datetime('2014-07-21T21:40:36.143+0200') to
    // datetime('2015-07-21T21:40:32.142+0100'): the date diff is exactly 12 months,
    // but the time on RHS is earlier, so still 12 months (P1Y) because month_diff
    // already handles day clamping. But we also need time comparison:
    // if same day-of-month after advancing, check time.
    let d1_advanced = date1.add_months(months)?;
    if d1_advanced == date2 {
        // Same date after advancing: check time.
        let t1_nanos = utc_nanos_of_day(lhs, rhs)?;
        let t2_nanos = utc_nanos_of_day(rhs, lhs)?;
        if months > 0 && t2_nanos < t1_nanos {
            months -= 1;
        } else if months < 0 && t2_nanos > t1_nanos {
            months += 1;
        }
    }

    Ok(CypherDuration {
        months,
        days: 0,
        seconds: 0,
        nanos: 0,
    })
}

/// Compute `duration.inDays(lhs, rhs)`: total elapsed days + time remainder.
pub fn duration_in_days(lhs: &Value, rhs: &Value) -> Result<CypherDuration> {
    // When either side is time-only, inDays returns PT0S (no days to count).
    if is_time_only(lhs) || is_time_only(rhs) {
        return Ok(zero_duration());
    }

    let (Some(date1), Some(date2)) = (extract_date(lhs), extract_date(rhs)) else {
        return Ok(zero_duration());
    };
    // Compute total elapsed nanoseconds including time components. i128:
    // a span over ~292 years overflows i64 nanoseconds.
    let day_nanos = i128::from(date1.days_until(date2)?) * 86_400_000_000_000i128;
    let t1_nanos = utc_nanos_of_day(lhs, rhs)?;
    let t2_nanos = utc_nanos_of_day(rhs, lhs)?;
    let total_nanos = day_nanos + i128::from(t2_nanos - t1_nanos);

    // Truncate toward zero to get whole days (always fits: the supported
    // years span only ~7.3 million days).
    let nanos_per_day: i128 = 86_400_000_000_000;
    let days = (total_nanos / nanos_per_day) as i64;

    Ok(CypherDuration {
        months: 0,
        days,
        seconds: 0,
        nanos: 0,
    })
}

/// Compute `duration.inSeconds(lhs, rhs)`: everything flattened to seconds+nanos.
pub fn duration_in_seconds(lhs: &Value, rhs: &Value) -> Result<CypherDuration> {
    // If either is time-only and the other has a date (but not time-only), do time-only diff.
    let either_time_only = is_time_only(lhs) || is_time_only(rhs);

    let t1_nanos = utc_nanos_of_day(lhs, rhs)?;
    let t2_nanos = utc_nanos_of_day(rhs, lhs)?;
    let time_diff = t2_nanos - t1_nanos;

    if either_time_only && !(has_date(lhs) && has_date(rhs)) {
        // Time-only comparison.
        return Ok(CypherDuration {
            months: 0,
            days: 0,
            seconds: time_diff.div_euclid(NANOS_PER_SEC),
            nanos: time_diff.rem_euclid(NANOS_PER_SEC),
        });
    }

    let (Some(date1), Some(date2)) = (extract_date(lhs), extract_date(rhs)) else {
        return Ok(zero_duration());
    };
    // i128: a span over ~292 years overflows i64 nanoseconds. The seconds
    // always fit i64 (the supported years span ~6e11 seconds).
    let total_day_secs = i128::from(date1.days_until(date2)?) * 86400;
    let total_nanos = total_day_secs * 1_000_000_000 + i128::from(time_diff);
    Ok(CypherDuration {
        months: 0,
        days: 0,
        seconds: total_nanos.div_euclid(1_000_000_000) as i64,
        nanos: total_nanos.rem_euclid(1_000_000_000) as i64,
    })
}

// ---------------------------------------------------------------------------
// Truncation helpers
// ---------------------------------------------------------------------------

/// Public wrapper around `parse_offset` for use from eval.rs.
pub fn parse_offset_public(s: &str) -> Result<i32> {
    parse_offset(s)
}

/// Truncate a date to the given unit, then apply optional map overrides.
pub fn truncate_date(unit: &str, val: &Value, map: &BTreeMap<String, Value>) -> Result<CypherDate> {
    let date = extract_date(val)
        .ok_or_else(|| parse_error("truncate_date requires a temporal with a date component"))?;
    let mut d = truncate_date_core(unit, date)?;

    // Apply map overrides.
    if let Some(Value::I64(day)) = map.get("day") {
        d = u32::try_from(*day)
            .map_err(|_| parse_error(format!("invalid day: {day}")))
            .and_then(|day_of_month| {
                CypherDate::new_with(
                    d.year,
                    u32::from(d.month),
                    day_of_month,
                    &format!("invalid day: {day}"),
                )
            })?;
    }
    if let Some(Value::I64(dow)) = map.get("dayOfWeek") {
        // dayOfWeek override: keep the same week, change to the given day.
        // Monday = 1. The truncated date for 'week' is already Monday.
        let current_dow = i64::from(d.weekday()?);
        let delta = dow.saturating_sub(current_dow);
        d = d.add_days(delta)?;
    }
    if let Some(Value::I64(m)) = map.get("month") {
        d = u32::try_from(*m)
            .map_err(|_| parse_error(format!("invalid month: {m}")))
            .and_then(|month| {
                CypherDate::new_with(
                    d.year,
                    month,
                    u32::from(d.day),
                    &format!("invalid month: {m}"),
                )
            })?;
    }

    Ok(d)
}

/// Core date truncation logic.
fn truncate_date_core(unit: &str, date: CypherDate) -> Result<CypherDate> {
    let y = date.year;
    match unit {
        "millennium" => CypherDate::new((y / 1000) * 1000, 1, 1),
        "century" => CypherDate::new((y / 100) * 100, 1, 1),
        "decade" => CypherDate::new((y / 10) * 10, 1, 1),
        "year" => CypherDate::new(y, 1, 1),
        "weekYear" => {
            // Monday of ISO week 1 of the ISO week-year.
            let (iso_year, _) = date.iso_week()?;
            CypherDate::from_iso_week(iso_year, 1, civil::Weekday::Monday, "invalid week date")
        }
        "quarter" => CypherDate::new(y, (date.quarter() - 1) * 3 + 1, 1),
        "month" => CypherDate::new(y, u32::from(date.month), 1),
        "week" => {
            // Monday of the current ISO week.
            let dow = i64::from(date.weekday()?) - 1;
            date.add_days(-dow)
        }
        "day" | "hour" | "minute" | "second" | "millisecond" | "microsecond" => Ok(date),
        _ => Err(parse_error("unsupported truncation unit for date")),
    }
}

/// Truncate a time to the given unit, then apply optional map overrides.
pub fn truncate_time(
    unit: &str,
    val: &Value,
    map: &BTreeMap<String, Value>,
) -> Result<CypherLocalTime> {
    let time = extract_time(val)
        .ok_or_else(|| parse_error("truncate_time requires a temporal with a time component"))?;
    let mut t = truncate_time_core(unit, time)?;

    // Apply nano override from map.
    // The nanosecond override adds to the truncated nanos (fills in the zeroed-out
    // sub-precision digits). E.g. for 'millisecond' truncation with nanos=645000000,
    // {nanosecond: 2} produces 645000002.
    if let Some(Value::I64(ns)) = map.get("nanosecond") {
        let invalid = format!("invalid nanosecond: {ns}");
        let new_nanos = u32::try_from(*ns)
            .ok()
            .and_then(|ns| t.nanosecond.checked_add(ns))
            .ok_or_else(|| parse_error(&invalid))?;
        t = CypherLocalTime::new_with(
            u32::from(t.hour),
            u32::from(t.minute),
            u32::from(t.second),
            new_nanos,
            &invalid,
        )?;
    }

    Ok(t)
}

/// Core time truncation logic.
fn truncate_time_core(unit: &str, time: CypherLocalTime) -> Result<CypherLocalTime> {
    let at = |hour: u8, minute: u8, second: u8, nanosecond: u32| CypherLocalTime {
        hour,
        minute,
        second,
        nanosecond,
    };
    match unit {
        "millennium" | "century" | "decade" | "year" | "weekYear" | "quarter" | "month"
        | "week" | "day" => Ok(CypherLocalTime::MIDNIGHT),
        "hour" => Ok(at(time.hour, 0, 0, 0)),
        "minute" => Ok(at(time.hour, time.minute, 0, 0)),
        "second" => Ok(at(time.hour, time.minute, time.second, 0)),
        "millisecond" => Ok(at(
            time.hour,
            time.minute,
            time.second,
            time.nanosecond / 1_000_000 * 1_000_000,
        )),
        "microsecond" => Ok(at(
            time.hour,
            time.minute,
            time.second,
            time.nanosecond / 1_000 * 1_000,
        )),
        _ => Err(parse_error("unsupported truncation unit for time")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ErrorCode;

    fn date(y: i32, m: u32, d: u32) -> CypherDate {
        CypherDate::new(y, m, d).unwrap()
    }

    fn time(h: u32, m: u32, s: u32, n: u32) -> CypherLocalTime {
        CypherLocalTime::new(h, m, s, n).unwrap()
    }

    fn query_code(e: &GraphError) -> ErrorCode {
        match e {
            GraphError::Query(q) => q.code(),
            other => panic!("expected a query error, got {other:?}"),
        }
    }

    #[test]
    fn test_date_display() {
        assert_eq!(date(1984, 10, 11).to_string(), "1984-10-11");
        assert_eq!(date(-44, 3, 15).to_string(), "-044-03-15");
    }

    #[test]
    fn test_date_parse() {
        let d = CypherDate::from_iso_string("1984-10-11").unwrap();
        assert_eq!(d, date(1984, 10, 11));

        let d2 = CypherDate::from_iso_string("19841011").unwrap();
        assert_eq!(d2, d);

        let d3 = CypherDate::from_iso_string("1984").unwrap();
        assert_eq!(d3, date(1984, 1, 1));

        let d4 = CypherDate::from_iso_string("1984-10").unwrap();
        assert_eq!(d4, date(1984, 10, 1));
    }

    #[test]
    fn test_date_week_parse() {
        // 2015-W30-2 = Tuesday of week 30 in 2015
        let d = CypherDate::from_iso_string("2015-W30-2").unwrap();
        assert_eq!(d, date(2015, 7, 21));

        let d2 = CypherDate::from_iso_string("2015W302").unwrap();
        assert_eq!(d2, d);

        // Default day = Monday
        let d3 = CypherDate::from_iso_string("2015-W30").unwrap();
        assert_eq!(d3, date(2015, 7, 20));

        // Week 53 exists only in long ISO years.
        assert_eq!(
            CypherDate::from_iso_string("2020-W53-7").unwrap(),
            date(2021, 1, 3)
        );
        let e = CypherDate::from_iso_string("2021-W53-1").unwrap_err();
        assert_eq!(query_code(&e), ErrorCode::InvalidArgumentValue);
    }

    #[test]
    fn test_date_ordinal_parse() {
        let d = CypherDate::from_iso_string("1984-202").unwrap();
        assert_eq!(d, date(1984, 7, 20));

        let d2 = CypherDate::from_iso_string("1984202").unwrap();
        assert_eq!(d2, d);

        // 366 only in a leap year; 0 never.
        assert_eq!(
            CypherDate::from_iso_string("2024-366").unwrap(),
            date(2024, 12, 31)
        );
        assert!(CypherDate::from_iso_string("2023-366").is_err());
        assert!(CypherDate::from_iso_string("2023-000").is_err());
    }

    #[test]
    fn test_local_time_display() {
        assert_eq!(
            time(12, 31, 14, 645_876_123).to_string(),
            "12:31:14.645876123"
        );
        assert_eq!(time(21, 40, 0, 0).to_string(), "21:40");
        assert_eq!(time(21, 40, 32, 0).to_string(), "21:40:32");
        assert_eq!(time(21, 40, 32, 142_000_000).to_string(), "21:40:32.142");
    }

    #[test]
    fn test_time_display() {
        let t = |h, m, s, n, off| CypherTime {
            time: time(h, m, s, n),
            offset_seconds: off,
        };
        assert_eq!(t(21, 40, 32, 142_000_000, 0).to_string(), "21:40:32.142Z");
        assert_eq!(t(21, 40, 32, 0, 3600).to_string(), "21:40:32+01:00");
        assert_eq!(t(21, 40, 0, 0, -5400).to_string(), "21:40-01:30");
    }

    #[test]
    fn test_datetime_display() {
        let dt = |off| CypherDateTime {
            local: CypherLocalDateTime {
                date: date(1984, 10, 11),
                time: time(12, 31, 14, 0),
            },
            offset_seconds: off,
            tz_name: None,
        };
        assert_eq!(dt(0).to_string(), "1984-10-11T12:31:14Z");
        assert_eq!(dt(3600).to_string(), "1984-10-11T12:31:14+01:00");
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
        assert_eq!(t, time(12, 31, 14, 645_876_123));

        let t2 = CypherLocalTime::from_iso_string("21:40").unwrap();
        assert_eq!(t2, time(21, 40, 0, 0));

        let t3 = CypherLocalTime::from_iso_string("14").unwrap();
        assert_eq!(t3, time(14, 0, 0, 0));
    }

    #[test]
    fn test_offset_parse() {
        let t = CypherTime::from_iso_string("21:40:32.142Z").unwrap();
        assert_eq!(t.offset_seconds, 0);

        let t2 = CypherTime::from_iso_string("21:40:32+01:00").unwrap();
        assert_eq!(t2.offset_seconds, 3600);

        let t3 = CypherTime::from_iso_string("21:40-01:30").unwrap();
        assert_eq!(t3.offset_seconds, -5400);

        // Offsets must stay strictly inside one day.
        assert!(CypherTime::from_iso_string("21:40+24:00").is_err());
        assert_eq!(
            CypherTime::from_iso_string("21:40+23:59:59")
                .unwrap()
                .offset_seconds,
            86_399
        );
    }

    #[test]
    fn test_serde_roundtrip_date() {
        let d = date(2024, 6, 15);
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
        assert_eq!(d, date(1984, 10, 11));
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

    // ---- Construction ranges (BVA) -------------------------------------

    #[test]
    fn date_year_boundaries() {
        assert_eq!(CypherDate::new(-9999, 1, 1).unwrap().year, -9999);
        assert_eq!(CypherDate::new(9999, 12, 31).unwrap().year, 9999);
        for bad in [-10_000, 10_000] {
            let e = CypherDate::new(bad, 1, 1).unwrap_err();
            assert_eq!(query_code(&e), ErrorCode::NumberOutOfRange, "year {bad}");
            assert!(
                e.to_string().contains("temporal year is out of range"),
                "year {bad}: {e}"
            );
            // The message never echoes the runtime value (D10).
            assert!(!e.to_string().contains(&bad.to_string()), "{e}");
        }
    }

    #[test]
    fn date_rejects_impossible_month_and_day() {
        for (m, d) in [(0, 1), (13, 1), (1, 0), (1, 32), (2, 30), (4, 31)] {
            let e = CypherDate::new(2023, m, d).unwrap_err();
            assert_eq!(query_code(&e), ErrorCode::InvalidArgumentValue, "{m}-{d}");
        }
        assert!(CypherDate::new(2023, 2, 29).is_err());
        assert!(CypherDate::new(2024, 2, 29).is_ok());
        assert!(CypherDate::new(1900, 2, 29).is_err());
        assert!(CypherDate::new(2000, 2, 29).is_ok());
    }

    #[test]
    fn year_range_applies_to_map_and_string_construction() {
        let mut map = BTreeMap::new();
        map.insert("year".to_string(), Value::I64(10_000));
        let e = CypherDate::from_map(&map).unwrap_err();
        assert_eq!(query_code(&e), ErrorCode::NumberOutOfRange);
        let e = CypherDate::from_iso_string("+10000-01-01").unwrap_err();
        assert_eq!(query_code(&e), ErrorCode::NumberOutOfRange);
        let e = CypherLocalDateTime::from_iso_string("+10000-01-01T00:00").unwrap_err();
        assert_eq!(query_code(&e), ErrorCode::NumberOutOfRange);
        assert!(CypherDate::from_iso_string("9999-12-31").is_ok());
    }

    #[test]
    fn nanosecond_boundaries_for_new_values() {
        assert_eq!(
            CypherLocalTime::new(0, 0, 0, 999_999_999)
                .unwrap()
                .nanosecond,
            999_999_999
        );
        let e = CypherLocalTime::new(23, 59, 59, 1_000_000_000).unwrap_err();
        assert_eq!(query_code(&e), ErrorCode::InvalidArgumentValue);
        // Leap seconds (second 60) are rejected too.
        assert!(CypherLocalTime::new(23, 59, 60, 0).is_err());
        assert!(CypherLocalTime::new(24, 0, 0, 0).is_err());
        assert!(CypherLocalTime::new(0, 60, 0, 0).is_err());

        let mut map = BTreeMap::new();
        map.insert("hour".to_string(), Value::I64(23));
        map.insert("minute".to_string(), Value::I64(59));
        map.insert("second".to_string(), Value::I64(59));
        map.insert("nanosecond".to_string(), Value::I64(1_000_000_000));
        let e = CypherLocalTime::from_map(&map).unwrap_err();
        assert_eq!(query_code(&e), ErrorCode::InvalidArgumentValue);
        map.insert("nanosecond".to_string(), Value::I64(999_999_999));
        assert_eq!(
            CypherLocalTime::from_map(&map).unwrap(),
            time(23, 59, 59, 999_999_999)
        );
    }

    // ---- Legacy values (wire compatibility) ----------------------------

    /// Positional bytes as written by the original chrono-based build.
    fn legacy_bytes<T: Serialize>(v: &T) -> Vec<u8> {
        rmp_serde::to_vec(v).unwrap()
    }

    #[test]
    fn legacy_year_20000_date_decodes_displays_and_errors_in_arithmetic() {
        // [20000, 6, 1] as a msgpack array, as stored by v0.2.
        let bytes = [0x93, 0xcd, 0x4e, 0x20, 0x06, 0x01];
        let d: CypherDate = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(d.year, 20_000);
        assert_eq!(d.to_string(), "20000-06-01");
        // It re-encodes to the same bytes.
        assert_eq!(legacy_bytes(&d), bytes);

        for err in [
            d.add_days(1).unwrap_err(),
            d.add_months(1).unwrap_err(),
            d.ordinal().unwrap_err(),
            d.iso_week().unwrap_err(),
            d.weekday().unwrap_err(),
            d.days_until(date(2024, 1, 1)).unwrap_err(),
        ] {
            assert_eq!(query_code(&err), ErrorCode::NumberOutOfRange);
            assert!(err.to_string().contains("out of range"), "{err}");
        }
        // Plain accessors and ordering never fail.
        assert!(d > date(9999, 12, 31));
        assert_eq!(d.quarter(), 2);
        assert_eq!(date(2024, 1, 1).epoch_days(), 19_723);
        assert!(d.epoch_days() > date(9999, 12, 31).epoch_days());
    }

    #[test]
    fn legacy_leap_second_nanos_decode_and_display() {
        // [23, 59, 59, 1_500_000_000]
        let bytes = [0x94, 0x17, 0x3b, 0x3b, 0xce, 0x59, 0x68, 0x2f, 0x00];
        let t: CypherLocalTime = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(t.nanosecond, 1_500_000_000);
        assert_eq!(t.to_string(), "23:59:59.15");
        assert_eq!(legacy_bytes(&t), bytes);
        let e = t.to_jiff().unwrap_err();
        assert_eq!(query_code(&e), ErrorCode::NumberOutOfRange);
        // Leap nanos are only legal on second 59.
        let bad = rmp_serde::to_vec(&(23u32, 59u32, 58u32, 1_500_000_000u32)).unwrap();
        assert!(rmp_serde::from_slice::<CypherLocalTime>(&bad).is_err());
    }

    #[test]
    fn decode_rejects_what_chrono_rejected() {
        let enc = |v: (i32, u32, u32)| rmp_serde::to_vec(&v).unwrap();
        // chrono's NaiveDate range is -262143..=262142.
        assert!(rmp_serde::from_slice::<CypherDate>(&enc((262_142, 12, 31))).is_ok());
        assert!(rmp_serde::from_slice::<CypherDate>(&enc((-262_143, 1, 1))).is_ok());
        assert!(rmp_serde::from_slice::<CypherDate>(&enc((262_143, 1, 1))).is_err());
        assert!(rmp_serde::from_slice::<CypherDate>(&enc((-262_144, 1, 1))).is_err());
        assert!(rmp_serde::from_slice::<CypherDate>(&enc((2023, 2, 29))).is_err());
        assert!(rmp_serde::from_slice::<CypherDate>(&enc((2023, 13, 1))).is_err());
        assert!(rmp_serde::from_slice::<CypherDate>(&enc((2023, 0, 1))).is_err());
    }

    #[test]
    fn wire_format_is_positional_and_unchanged() {
        let dt = CypherDateTime {
            local: CypherLocalDateTime {
                date: date(2015, 7, 21),
                time: time(21, 40, 32, 142_000_000),
            },
            offset_seconds: 7200,
            tz_name: Some("Europe/Stockholm".to_string()),
        };
        let bytes = rmp_serde::to_vec(&dt).unwrap();
        let (y, mo, d, h, mi, s, n, off, tz): (i32, u32, u32, u32, u32, u32, u32, i32, String) =
            rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(
            (y, mo, d, h, mi, s, n, off, tz.as_str()),
            (
                2015,
                7,
                21,
                21,
                40,
                32,
                142_000_000,
                7200,
                "Europe/Stockholm"
            )
        );
        let back: CypherDateTime = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(back, dt);
        // Without a zone name there is no trailing element.
        let plain = CypherDateTime {
            tz_name: None,
            ..dt.clone()
        };
        let bytes = rmp_serde::to_vec(&plain).unwrap();
        assert_eq!(bytes[0], 0x98);
        // The named (map) form decodes as well.
        let named = rmp_serde::to_vec_named(&dt).unwrap();
        assert_eq!(rmp_serde::from_slice::<CypherDateTime>(&named).unwrap(), dt);
        // Offsets at or beyond one day are not decodable.
        let bad =
            rmp_serde::to_vec(&(2015i32, 7u32, 21u32, 0u32, 0u32, 0u32, 0u32, 86_400i32)).unwrap();
        assert!(rmp_serde::from_slice::<CypherDateTime>(&bad).is_err());
    }

    // ---- Arithmetic through jiff ---------------------------------------

    #[test]
    fn month_arithmetic_clamps_to_month_end() {
        assert_eq!(date(2023, 1, 31).add_months(1).unwrap(), date(2023, 2, 28));
        assert_eq!(date(2024, 1, 31).add_months(1).unwrap(), date(2024, 2, 29));
        assert_eq!(date(2024, 3, 31).add_months(-1).unwrap(), date(2024, 2, 29));
        assert_eq!(date(2024, 2, 29).add_months(12).unwrap(), date(2025, 2, 28));
        assert_eq!(date(2024, 12, 15).add_months(1).unwrap(), date(2025, 1, 15));
        assert_eq!(
            date(2024, 1, 15).add_months(-1).unwrap(),
            date(2023, 12, 15)
        );
    }

    #[test]
    fn date_arithmetic_stops_at_the_supported_range() {
        assert_eq!(date(9999, 12, 30).add_days(1).unwrap(), date(9999, 12, 31));
        assert!(date(9999, 12, 31).add_days(1).is_err());
        assert_eq!(date(-9999, 1, 2).add_days(-1).unwrap(), date(-9999, 1, 1));
        assert!(date(-9999, 1, 1).add_days(-1).is_err());
        assert!(date(9999, 12, 1).add_months(1).is_err());
        assert!(date(2024, 1, 1).add_months(i64::MAX).is_err());
        assert!(date(2024, 1, 1).add_days(i64::MAX).is_err());
    }

    #[test]
    fn date_accessors() {
        // 2021-01-01 belongs to ISO week 53 of 2020 and is a Friday.
        let d = date(2021, 1, 1);
        assert_eq!(d.iso_week().unwrap(), (2020, 53));
        assert_eq!(d.weekday().unwrap(), 5);
        assert_eq!(d.ordinal().unwrap(), 1);
        assert_eq!(date(2024, 12, 31).ordinal().unwrap(), 366);
        assert_eq!(date(2024, 12, 31).iso_week().unwrap(), (2025, 1));
        assert_eq!(date(2024, 6, 2).weekday().unwrap(), 7);
        assert_eq!(date(2024, 8, 15).quarter(), 3);
    }

    #[test]
    fn tz_lookup_is_exact_and_case_sensitive() {
        assert!(lookup_tz("Europe/Stockholm").is_some());
        assert!(lookup_tz("UTC").is_some());
        assert!(lookup_tz("europe/stockholm").is_none());
        assert!(lookup_tz("Not/AZone").is_none());
        assert!(lookup_tz("").is_none());
    }

    #[test]
    fn named_zone_gap_and_overlap_resolution() {
        let local = |m, d, h, mi| CypherLocalDateTime {
            date: date(2024, m, d),
            time: time(h, mi, 0, 0),
        };
        // Spring forward 2024-03-31 02:00 -> 03:00 in Stockholm: 02:30 is a gap.
        let gap = CypherDateTime::from_iso_string("2024-03-31T02:30[Europe/Stockholm]");
        let gap_err = gap.unwrap_err();
        assert_eq!(query_code(&gap_err), ErrorCode::InvalidArgumentValue);
        assert!(
            gap_err
                .to_string()
                .contains("ambiguous or invalid datetime in timezone"),
            "{gap_err}"
        );
        // Fall back 2024-10-27 03:00 -> 02:00: 02:30 happens twice, the
        // earlier (+02:00) wins.
        let fold = CypherDateTime::from_iso_string("2024-10-27T02:30[Europe/Stockholm]").unwrap();
        assert_eq!(fold.offset_seconds, 7200);
        let tz = lookup_tz("Europe/Stockholm").unwrap();
        assert_eq!(earliest_offset(&tz, &local(3, 31, 2, 30)).unwrap(), None);
        assert_eq!(
            earliest_offset(&tz, &local(10, 27, 2, 30)).unwrap(),
            Some(7200)
        );
        assert_eq!(
            earliest_offset(&tz, &local(7, 1, 12, 0)).unwrap(),
            Some(7200)
        );
        assert_eq!(
            earliest_offset(&tz, &local(1, 1, 12, 0)).unwrap(),
            Some(3600)
        );
    }

    #[test]
    fn epoch_conversions_cover_the_full_range() {
        let max = CypherDateTime::from_epoch(253_402_300_799, 0).unwrap();
        assert_eq!(max.to_string(), "9999-12-31T23:59:59Z");
        assert!(CypherDateTime::from_epoch(253_402_300_800, 0).is_err());
        let min = CypherDateTime::from_epoch(-377_705_116_800, 0).unwrap();
        assert_eq!(min.to_string(), "-9999-01-01T00:00Z");
        assert!(CypherDateTime::from_epoch(-377_705_116_801, 0).is_err());
        // Round trip through epoch seconds, including a negative offset.
        let dt = CypherDateTime::from_iso_string("1969-12-31T23:59:59.5-05:45").unwrap();
        // 23:59:59.5 at -05:45 is 05:44:59.5 UTC the next day.
        assert_eq!(dt.epoch_seconds(), 5 * 3600 + 44 * 60 + 59);
        assert_eq!(dt.epoch_millis(), (5 * 3600 + 44 * 60 + 59) * 1000 + 500);
    }

    #[test]
    fn sort_keys_order_by_instant() {
        let a = CypherDateTime::from_iso_string("2024-01-01T12:00+02:00").unwrap();
        let b = CypherDateTime::from_iso_string("2024-01-01T11:00Z").unwrap();
        let c = CypherDateTime::from_iso_string("2024-01-01T11:00:00.000000001Z").unwrap();
        assert!(a.utc_sort_key() < b.utc_sort_key());
        assert!(b.utc_sort_key() < c.utc_sort_key());
        // Times of day wrap at midnight (existing comparison semantics).
        let t1 = CypherTime::from_iso_string("00:30+01:00").unwrap();
        let t2 = CypherTime::from_iso_string("23:45Z").unwrap();
        assert!(t2.utc_sort_key() > t1.utc_sort_key());
    }
}
