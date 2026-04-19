//! openCypher temporal types backed by chrono primitives.
//!
//! Provides `CypherDate`, `CypherLocalTime`, `CypherTime`, `CypherLocalDateTime`,
//! `CypherDateTime`, and `CypherDuration` with ISO 8601 parsing, display, and
//! MessagePack-compatible serde via integer fields.

use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};

use chrono::{Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::types::{GraphError, Result, Value};

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

/// Parse a timezone offset string into a `FixedOffset`.
fn parse_offset(s: &str) -> Result<FixedOffset> {
    if s == "Z" || s == "z" {
        return Ok(FixedOffset::east_opt(0).unwrap());
    }
    let sign: i32 = if s.starts_with('+') {
        1
    } else if s.starts_with('-') {
        -1
    } else {
        return Err(GraphError::Serialization(format!("invalid offset: {s}")));
    };
    let body = &s[1..];
    let (h, m) = if body.len() == 2 {
        // +HH
        (
            body.parse::<i32>()
                .map_err(|e| GraphError::Serialization(e.to_string()))?,
            0,
        )
    } else if body.len() == 4 {
        // +HHMM
        let hh = body[..2]
            .parse::<i32>()
            .map_err(|e| GraphError::Serialization(e.to_string()))?;
        let mm = body[2..]
            .parse::<i32>()
            .map_err(|e| GraphError::Serialization(e.to_string()))?;
        (hh, mm)
    } else if body.len() == 5 && body.as_bytes()[2] == b':' {
        // +HH:MM
        let hh = body[..2]
            .parse::<i32>()
            .map_err(|e| GraphError::Serialization(e.to_string()))?;
        let mm = body[3..]
            .parse::<i32>()
            .map_err(|e| GraphError::Serialization(e.to_string()))?;
        (hh, mm)
    } else {
        return Err(GraphError::Serialization(format!("invalid offset: {s}")));
    };
    let secs = sign * (h * 3600 + m * 60);
    FixedOffset::east_opt(secs)
        .ok_or_else(|| GraphError::Serialization(format!("offset out of range: {s}")))
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
    if s.contains(':') {
        // Colon-separated: HH:MM, HH:MM:SS, HH:MM:SS.nnn
        let parts: Vec<&str> = s.splitn(3, ':').collect();
        let h: u32 = parts[0]
            .parse()
            .map_err(|e: std::num::ParseIntError| GraphError::Serialization(e.to_string()))?;
        let m: u32 = parts
            .get(1)
            .unwrap_or(&"0")
            .parse()
            .map_err(|e: std::num::ParseIntError| GraphError::Serialization(e.to_string()))?;
        if parts.len() < 3 {
            return NaiveTime::from_hms_opt(h, m, 0)
                .ok_or_else(|| GraphError::Serialization(format!("invalid time: {s}")));
        }
        let sec_part = parts[2];
        if let Some(dot_pos) = sec_part.find('.') {
            let sec: u32 = sec_part[..dot_pos]
                .parse()
                .map_err(|e: std::num::ParseIntError| GraphError::Serialization(e.to_string()))?;
            let frac = &sec_part[dot_pos + 1..];
            let nano = parse_frac_nanos(frac)?;
            NaiveTime::from_hms_nano_opt(h, m, sec, nano)
                .ok_or_else(|| GraphError::Serialization(format!("invalid time: {s}")))
        } else {
            let sec: u32 = sec_part
                .parse()
                .map_err(|e: std::num::ParseIntError| GraphError::Serialization(e.to_string()))?;
            NaiveTime::from_hms_opt(h, m, sec)
                .ok_or_else(|| GraphError::Serialization(format!("invalid time: {s}")))
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
                    .map_err(|e| GraphError::Serialization(e.to_string()))?,
                0u32,
                0u32,
            ),
            4 => {
                let hh = digits[..2]
                    .parse::<u32>()
                    .map_err(|e| GraphError::Serialization(e.to_string()))?;
                let mm = digits[2..]
                    .parse::<u32>()
                    .map_err(|e| GraphError::Serialization(e.to_string()))?;
                (hh, mm, 0)
            }
            6 => {
                let hh = digits[..2]
                    .parse::<u32>()
                    .map_err(|e| GraphError::Serialization(e.to_string()))?;
                let mm = digits[2..4]
                    .parse::<u32>()
                    .map_err(|e| GraphError::Serialization(e.to_string()))?;
                let ss = digits[4..]
                    .parse::<u32>()
                    .map_err(|e| GraphError::Serialization(e.to_string()))?;
                (hh, mm, ss)
            }
            _ => {
                return Err(GraphError::Serialization(format!(
                    "invalid compact time: {s}"
                )))
            }
        };
        let nano = if let Some(f) = frac {
            parse_frac_nanos(f)?
        } else {
            0
        };
        NaiveTime::from_hms_nano_opt(h, m, sec, nano)
            .ok_or_else(|| GraphError::Serialization(format!("invalid time: {s}")))
    }
}

/// Parse a string as a given numeric type, wrapping the error in `GraphError`.
fn parse_num<T: std::str::FromStr>(s: &str) -> Result<T>
where
    T::Err: fmt::Display,
{
    s.parse::<T>()
        .map_err(|e| GraphError::Serialization(e.to_string()))
}

/// Parse fractional seconds string (up to 9 digits) into nanoseconds.
fn parse_frac_nanos(frac: &str) -> Result<u32> {
    let mut padded = String::from(frac);
    while padded.len() < 9 {
        padded.push('0');
    }
    padded.truncate(9);
    padded
        .parse::<u32>()
        .map_err(|e| GraphError::Serialization(e.to_string()))
}

/// Parse a date string into `NaiveDate`.
fn parse_date_str(s: &str) -> Result<NaiveDate> {
    // Week date: YYYY-Www-D or YYYYWwwD or YYYY-Www or YYYYWww
    if s.contains('W') {
        return parse_week_date(s);
    }

    if s.contains('-') {
        let parts: Vec<&str> = s.split('-').collect();
        match parts.len() {
            3 => {
                let y: i32 = parts[0].parse().map_err(|e: std::num::ParseIntError| {
                    GraphError::Serialization(e.to_string())
                })?;
                let m: u32 = parts[1].parse().map_err(|e: std::num::ParseIntError| {
                    GraphError::Serialization(e.to_string())
                })?;
                let d: u32 = parts[2].parse().map_err(|e: std::num::ParseIntError| {
                    GraphError::Serialization(e.to_string())
                })?;
                NaiveDate::from_ymd_opt(y, m, d)
                    .ok_or_else(|| GraphError::Serialization(format!("invalid date: {s}")))
            }
            2 => {
                let y: i32 = parts[0].parse().map_err(|e: std::num::ParseIntError| {
                    GraphError::Serialization(e.to_string())
                })?;
                let part2: u32 = parts[1].parse().map_err(|e: std::num::ParseIntError| {
                    GraphError::Serialization(e.to_string())
                })?;
                if parts[1].len() == 3 {
                    // Ordinal: YYYY-DDD
                    NaiveDate::from_yo_opt(y, part2).ok_or_else(|| {
                        GraphError::Serialization(format!("invalid ordinal date: {s}"))
                    })
                } else {
                    // YYYY-MM (day defaults to 1)
                    NaiveDate::from_ymd_opt(y, part2, 1)
                        .ok_or_else(|| GraphError::Serialization(format!("invalid date: {s}")))
                }
            }
            _ => Err(GraphError::Serialization(format!("invalid date: {s}"))),
        }
    } else {
        // Compact forms: YYYYMMDD, YYYYMM, YYYY, YYYYDDD
        match s.len() {
            8 => {
                let y: i32 = parse_num(&s[..4])?;
                let m: u32 = parse_num(&s[4..6])?;
                let d: u32 = parse_num(&s[6..])?;
                NaiveDate::from_ymd_opt(y, m, d)
                    .ok_or_else(|| GraphError::Serialization(format!("invalid date: {s}")))
            }
            7 => {
                // YYYYDDD
                let y: i32 = parse_num(&s[..4])?;
                let d: u32 = parse_num(&s[4..])?;
                NaiveDate::from_yo_opt(y, d)
                    .ok_or_else(|| GraphError::Serialization(format!("invalid ordinal date: {s}")))
            }
            6 => {
                // YYYYMM
                let y: i32 = parse_num(&s[..4])?;
                let m: u32 = parse_num(&s[4..])?;
                NaiveDate::from_ymd_opt(y, m, 1)
                    .ok_or_else(|| GraphError::Serialization(format!("invalid date: {s}")))
            }
            4 => {
                let y: i32 = parse_num(s)?;
                NaiveDate::from_ymd_opt(y, 1, 1)
                    .ok_or_else(|| GraphError::Serialization(format!("invalid date: {s}")))
            }
            _ => Err(GraphError::Serialization(format!("invalid date: {s}"))),
        }
    }
}

/// Parse ISO week date forms.
fn parse_week_date(s: &str) -> Result<NaiveDate> {
    // Strip hyphens for uniform handling, but track original for error messages.
    let compact = s.replace('-', "");
    // Expected: YYYYWww or YYYYWwwD
    let w_pos = compact
        .find('W')
        .ok_or_else(|| GraphError::Serialization(format!("invalid week date: {s}")))?;
    let year: i32 = compact[..w_pos]
        .parse()
        .map_err(|e: std::num::ParseIntError| GraphError::Serialization(e.to_string()))?;
    let after_w = &compact[w_pos + 1..];
    let (week, day) = if after_w.len() >= 3 {
        let wk: u32 = after_w[..2]
            .parse()
            .map_err(|e: std::num::ParseIntError| GraphError::Serialization(e.to_string()))?;
        let d: u32 = after_w[2..3]
            .parse()
            .map_err(|e: std::num::ParseIntError| GraphError::Serialization(e.to_string()))?;
        (wk, d)
    } else if after_w.len() == 2 {
        let wk: u32 = after_w
            .parse()
            .map_err(|e: std::num::ParseIntError| GraphError::Serialization(e.to_string()))?;
        (wk, 1) // default to Monday
    } else {
        return Err(GraphError::Serialization(format!("invalid week date: {s}")));
    };
    let weekday = match day {
        1 => chrono::Weekday::Mon,
        2 => chrono::Weekday::Tue,
        3 => chrono::Weekday::Wed,
        4 => chrono::Weekday::Thu,
        5 => chrono::Weekday::Fri,
        6 => chrono::Weekday::Sat,
        7 => chrono::Weekday::Sun,
        _ => {
            return Err(GraphError::Serialization(format!(
                "invalid day of week {day} in: {s}"
            )))
        }
    };
    NaiveDate::from_isoywd_opt(year, week, weekday)
        .ok_or_else(|| GraphError::Serialization(format!("invalid week date: {s}")))
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
        format!("{sign}{h:02}:{m:02}")
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
        write!(f, "{sign}{h:02}:{m:02}")
    }
}

/// Build a `NaiveTime` from map keys (hour, minute, second, nanosecond, millisecond, microsecond).
fn time_from_map(map: &BTreeMap<String, Value>) -> Result<NaiveTime> {
    let h = get_i64(map, "hour").unwrap_or(0) as u32;
    let m = get_i64(map, "minute").unwrap_or(0) as u32;
    let s = get_i64(map, "second").unwrap_or(0) as u32;
    let mut nano = get_i64(map, "nanosecond").unwrap_or(0) as u32;
    nano += get_i64(map, "millisecond").unwrap_or(0) as u32 * 1_000_000;
    nano += get_i64(map, "microsecond").unwrap_or(0) as u32 * 1_000;
    NaiveTime::from_hms_nano_opt(h, m, s, nano)
        .ok_or_else(|| GraphError::Serialization("invalid time components".to_string()))
}

/// Build a `NaiveDate` from map keys (year, month, day, week, dayOfWeek, ordinalDay).
fn date_from_map(map: &BTreeMap<String, Value>) -> Result<NaiveDate> {
    let year = get_i64(map, "year").unwrap_or(0) as i32;
    if let Some(week) = get_i64(map, "week") {
        let dow = get_i64(map, "dayOfWeek").unwrap_or(1) as u32;
        let weekday = match dow {
            1 => chrono::Weekday::Mon,
            2 => chrono::Weekday::Tue,
            3 => chrono::Weekday::Wed,
            4 => chrono::Weekday::Thu,
            5 => chrono::Weekday::Fri,
            6 => chrono::Weekday::Sat,
            7 => chrono::Weekday::Sun,
            _ => {
                return Err(GraphError::Serialization(format!(
                    "invalid dayOfWeek: {dow}"
                )))
            }
        };
        NaiveDate::from_isoywd_opt(year, week as u32, weekday)
            .ok_or_else(|| GraphError::Serialization("invalid week date components".to_string()))
    } else if let Some(ord) = get_i64(map, "ordinalDay") {
        NaiveDate::from_yo_opt(year, ord as u32)
            .ok_or_else(|| GraphError::Serialization("invalid ordinal date components".to_string()))
    } else {
        let month = get_i64(map, "month").unwrap_or(1) as u32;
        let day = get_i64(map, "day").unwrap_or(1) as u32;
        NaiveDate::from_ymd_opt(year, month, day)
            .ok_or_else(|| GraphError::Serialization("invalid date components".to_string()))
    }
}

/// Extract an offset from the `timezone` map key.
fn offset_from_map(map: &BTreeMap<String, Value>) -> Result<FixedOffset> {
    match map.get("timezone") {
        Some(Value::String(s)) => parse_offset(s),
        _ => Err(GraphError::Serialization(
            "missing or invalid 'timezone' key".to_string(),
        )),
    }
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
        deserializer.deserialize_struct("CypherDate", &["year", "month", "day"], CypherDateVisitor)
    }
}

impl CypherDate {
    /// Parse a date from an ISO 8601 string.
    pub fn from_iso_string(s: &str) -> Result<Self> {
        parse_date_str(s).map(CypherDate)
    }

    /// Build a date from a property map.
    pub fn from_map(map: &BTreeMap<String, Value>) -> Result<Self> {
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
        deserializer.deserialize_struct(
            "CypherLocalTime",
            &["hour", "minute", "second", "nanosecond"],
            V,
        )
    }
}

impl CypherLocalTime {
    /// Parse a local time from an ISO 8601 string.
    pub fn from_iso_string(s: &str) -> Result<Self> {
        parse_time_str(s).map(CypherLocalTime)
    }

    /// Build a local time from a property map.
    pub fn from_map(map: &BTreeMap<String, Value>) -> Result<Self> {
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
        deserializer.deserialize_struct(
            "CypherTime",
            &["hour", "minute", "second", "nanosecond", "offset_seconds"],
            V,
        )
    }
}

impl CypherTime {
    /// Parse a time with offset from an ISO 8601 string.
    pub fn from_iso_string(s: &str) -> Result<Self> {
        let (time_part, off_part) = split_time_offset(s);
        let off_str = off_part.ok_or_else(|| {
            GraphError::Serialization(format!("CypherTime requires an offset: {s}"))
        })?;
        let t = parse_time_str(time_part)?;
        let off = parse_offset(off_str)?;
        Ok(CypherTime(t, off))
    }

    /// Build from a property map.
    pub fn from_map(map: &BTreeMap<String, Value>) -> Result<Self> {
        let t = time_from_map(map)?;
        let off = offset_from_map(map)?;
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
        deserializer.deserialize_struct(
            "CypherLocalDateTime",
            &[
                "year",
                "month",
                "day",
                "hour",
                "minute",
                "second",
                "nanosecond",
            ],
            V,
        )
    }
}

impl CypherLocalDateTime {
    /// Parse from ISO 8601, e.g. `1984-10-11T12:31:14`.
    pub fn from_iso_string(s: &str) -> Result<Self> {
        let t_pos = s
            .find('T')
            .or_else(|| s.find('t'))
            .ok_or_else(|| GraphError::Serialization(format!("expected 'T' separator in: {s}")))?;
        let date = parse_date_str(&s[..t_pos])?;
        let time = parse_time_str(&s[t_pos + 1..])?;
        Ok(CypherLocalDateTime(NaiveDateTime::new(date, time)))
    }

    /// Build from a property map.
    pub fn from_map(map: &BTreeMap<String, Value>) -> Result<Self> {
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
        fmt_offset(&self.1, f)
    }
}

impl Serialize for CypherDateTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let d = self.0.date();
        let t = self.0.time();
        let mut s = serializer.serialize_struct("CypherDateTime", 8)?;
        s.serialize_field("year", &d.year())?;
        s.serialize_field("month", &d.month())?;
        s.serialize_field("day", &d.day())?;
        s.serialize_field("hour", &t.hour())?;
        s.serialize_field("minute", &t.minute())?;
        s.serialize_field("second", &t.second())?;
        s.serialize_field("nanosecond", &t.nanosecond())?;
        s.serialize_field("offset_seconds", &self.1.local_minus_utc())?;
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
        }

        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = CypherDateTime;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("CypherDateTime")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<CypherDateTime, A::Error> {
                let (mut year, mut month, mut day) = (None, None, None);
                let (mut hour, mut minute, mut second, mut nano, mut off) =
                    (None, None, None, None, None);
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
                Ok(CypherDateTime(NaiveDateTime::new(d, t), offset, None))
            }
        }
        deserializer.deserialize_struct(
            "CypherDateTime",
            &[
                "year",
                "month",
                "day",
                "hour",
                "minute",
                "second",
                "nanosecond",
                "offset_seconds",
            ],
            V,
        )
    }
}

impl CypherDateTime {
    /// Parse from ISO 8601 with timezone, e.g. `1984-10-11T12:31:14Z`.
    pub fn from_iso_string(s: &str) -> Result<Self> {
        let t_pos = s
            .find('T')
            .or_else(|| s.find('t'))
            .ok_or_else(|| GraphError::Serialization(format!("expected 'T' separator in: {s}")))?;
        let date = parse_date_str(&s[..t_pos])?;
        let time_and_off = &s[t_pos + 1..];
        let (time_part, off_part) = split_time_offset(time_and_off);
        let off_str = off_part.ok_or_else(|| {
            GraphError::Serialization(format!("CypherDateTime requires an offset: {s}"))
        })?;
        let time = parse_time_str(time_part)?;
        let off = parse_offset(off_str)?;
        Ok(CypherDateTime(NaiveDateTime::new(date, time), off, None))
    }

    /// Build from a property map.
    pub fn from_map(map: &BTreeMap<String, Value>) -> Result<Self> {
        let d = date_from_map(map)?;
        let t = time_from_map(map)?;
        let off = offset_from_map(map)?;
        Ok(CypherDateTime(NaiveDateTime::new(d, t), off, None))
    }

    /// Create from epoch seconds and nanoseconds (UTC).
    pub fn from_epoch(seconds: i64, nanos: i64) -> Self {
        let total_nanos = seconds * 1_000_000_000 + nanos;
        let secs = total_nanos.div_euclid(1_000_000_000);
        let ns = total_nanos.rem_euclid(1_000_000_000) as u32;
        let dt = chrono::DateTime::from_timestamp(secs, ns)
            .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).unwrap());
        CypherDateTime(dt.naive_utc(), FixedOffset::east_opt(0).unwrap(), None)
    }

    /// Create from epoch milliseconds (UTC).
    pub fn from_epoch_millis(millis: i64) -> Self {
        let secs = millis.div_euclid(1000);
        let ms = millis.rem_euclid(1000) as u32;
        let dt = chrono::DateTime::from_timestamp(secs, ms * 1_000_000)
            .unwrap_or_else(|| chrono::DateTime::from_timestamp(0, 0).unwrap());
        CypherDateTime(dt.naive_utc(), FixedOffset::east_opt(0).unwrap(), None)
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

        // Decompose seconds into hours + minutes + remaining seconds.
        let total_secs = self.seconds;
        let hours = total_secs / 3600;
        let rem = total_secs % 3600;
        let minutes = rem / 60;
        let secs = rem % 60;
        let nanos = self.nanos;

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
                    if nanos < 0 || secs < 0 {
                        // Combine sign: if seconds is negative or nanos is negative.
                        let abs_secs = secs.unsigned_abs();
                        if secs < 0 {
                            write!(f, "-{abs_secs}.{trimmed}S")?;
                        } else {
                            write!(f, "{secs}.{trimmed}S")?;
                        }
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
        deserializer.deserialize_struct(
            "CypherDuration",
            &["months", "days", "seconds", "nanos"],
            V,
        )
    }
}

impl CypherDuration {
    /// Parse an ISO 8601 duration string, e.g. `P1Y2M3DT4H5M6.789S`.
    pub fn from_iso_string(s: &str) -> Result<Self> {
        if !s.starts_with('P') && !s.starts_with('p') {
            return Err(GraphError::Serialization(format!(
                "duration must start with 'P': {s}"
            )));
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

        // Parse date part: nY nM nD
        if !date_part.is_empty() {
            parse_duration_date_part(date_part, &mut months, &mut days)?;
        }

        // Parse time part: nH nM nS
        if let Some(tp) = time_part {
            if !tp.is_empty() {
                parse_duration_time_part(tp, &mut seconds, &mut nanos)?;
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
    pub fn from_map(map: &BTreeMap<String, Value>) -> Result<Self> {
        let years = get_i64(map, "years").unwrap_or(0);
        let mons = get_i64(map, "months").unwrap_or(0);
        let weeks = get_i64(map, "weeks").unwrap_or(0);
        let days = get_i64(map, "days").unwrap_or(0);
        let hours = get_i64(map, "hours").unwrap_or(0);
        let minutes = get_i64(map, "minutes").unwrap_or(0);
        let secs = get_i64(map, "seconds").unwrap_or(0);
        let millis = get_i64(map, "milliseconds").unwrap_or(0);
        let micros = get_i64(map, "microseconds").unwrap_or(0);
        let ns = get_i64(map, "nanoseconds").unwrap_or(0);

        let total_months = years * 12 + mons;
        let total_days = weeks * 7 + days;
        let total_seconds = hours * 3600 + minutes * 60 + secs;
        let total_nanos = millis * 1_000_000 + micros * 1_000 + ns;

        Ok(CypherDuration {
            months: total_months,
            days: total_days,
            seconds: total_seconds,
            nanos: total_nanos,
        })
    }
}

/// Parse the date portion of a duration string (before T).
fn parse_duration_date_part(s: &str, months: &mut i64, days: &mut i64) -> Result<()> {
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
        let num_str = &s[num_start..i];
        let n: i64 = num_str
            .parse()
            .map_err(|e: std::num::ParseIntError| GraphError::Serialization(e.to_string()))?;
        match b {
            b'Y' | b'y' => *months += n * 12,
            b'M' | b'm' => *months += n,
            b'W' | b'w' => *days += n * 7,
            b'D' | b'd' => *days += n,
            _ => {
                return Err(GraphError::Serialization(format!(
                    "unexpected char '{b}' in duration date part"
                )))
            }
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
            b'H' | b'h' => {
                let n: i64 = num_str.parse().map_err(|e: std::num::ParseIntError| {
                    GraphError::Serialization(e.to_string())
                })?;
                *seconds += n * 3600;
            }
            b'M' | b'm' => {
                let n: i64 = num_str.parse().map_err(|e: std::num::ParseIntError| {
                    GraphError::Serialization(e.to_string())
                })?;
                *seconds += n * 60;
            }
            b'S' | b's' => {
                // May be fractional
                if let Some(dot_pos) = num_str.find('.') {
                    let int_part = &num_str[..dot_pos];
                    let frac_part = &num_str[dot_pos + 1..];
                    let negative = int_part.starts_with('-');
                    let int_val: i64 = if int_part.is_empty() || int_part == "-" {
                        0
                    } else {
                        int_part.parse().map_err(|e: std::num::ParseIntError| {
                            GraphError::Serialization(e.to_string())
                        })?
                    };
                    let frac_nanos = parse_frac_nanos(frac_part)? as i64;
                    *seconds += int_val;
                    *nanos += if negative { -frac_nanos } else { frac_nanos };
                } else {
                    let n: i64 = num_str.parse().map_err(|e: std::num::ParseIntError| {
                        GraphError::Serialization(e.to_string())
                    })?;
                    *seconds += n;
                }
            }
            _ => {
                return Err(GraphError::Serialization(format!(
                    "unexpected char '{b}' in duration time part"
                )))
            }
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
