//! Temporal arithmetic operations for Cypher eval.
//!
//! Handles Duration +/- Duration, Temporal +/- Duration, Duration * Number,
//! Duration / Number, and temporal property accessors (e.g., `d.year`).
//!
//! All arithmetic is checked: a result outside the representable range
//! (i64 duration components, the calendar years -9999..=9999) is a
//! `NumberOutOfRange` error, never a panic or a silently wrong value.

use crate::temporal::{
    lookup_tz, tz_local_offsets, tz_offset_at_utc, tz_utc_to_local, CypherDate, CypherDateTime,
    CypherDuration, CypherLocalDateTime, CypherLocalTime, CypherTime,
};
use crate::types::{GraphError, QueryPhase, Value};
use jiff::tz::{AmbiguousOffset, TimeZone};
use jiff::SignedDuration;

/// Average seconds per month used for duration arithmetic (365.2425 * 86400 / 12).
const AVG_SECONDS_PER_MONTH: i64 = 2_629_746;

const NANOS_PER_SEC: i128 = 1_000_000_000;
const SECONDS_PER_DAY: i64 = 86_400;

/// The `NumberOutOfRange` error for a temporal result that does not fit.
fn out_of_range(what: impl std::fmt::Display) -> GraphError {
    GraphError::number_out_of_range(QueryPhase::Runtime, format!("{what} is out of range"))
}

/// The seconds + nanoseconds part of a duration as total nanoseconds.
fn sub_day_nanos(d: &CypherDuration) -> i128 {
    d.seconds as i128 * NANOS_PER_SEC + d.nanos as i128
}

/// Split total nanoseconds into `(seconds, nanos)` with matching signs.
fn split_nanos(total: i128) -> crate::types::Result<(i64, i64)> {
    let seconds = i64::try_from(total / NANOS_PER_SEC)
        .map_err(|_| out_of_range("duration seconds component"))?;
    Ok((seconds, (total % NANOS_PER_SEC) as i64))
}

/// `a + b` (or `a - b` when `subtract`), component-wise and checked.
fn combine_durations(
    a: &CypherDuration,
    b: &CypherDuration,
    subtract: bool,
) -> crate::types::Result<CypherDuration> {
    let op = |x: i64, y: i64| {
        if subtract {
            x.checked_sub(y)
        } else {
            x.checked_add(y)
        }
    };
    let months = op(a.months, b.months).ok_or_else(|| out_of_range("duration months"))?;
    let days = op(a.days, b.days).ok_or_else(|| out_of_range("duration days"))?;
    let sub_day = if subtract {
        sub_day_nanos(a) - sub_day_nanos(b)
    } else {
        sub_day_nanos(a) + sub_day_nanos(b)
    };
    let (seconds, nanos) = split_nanos(sub_day)?;
    Ok(CypherDuration {
        months,
        days,
        seconds,
        nanos,
    })
}

/// `-d`, checked (negating an `i64::MIN` component overflows).
pub(super) fn negate_duration(d: &CypherDuration) -> crate::types::Result<CypherDuration> {
    let neg = |x: i64| {
        x.checked_neg()
            .ok_or_else(|| out_of_range("negated duration"))
    };
    Ok(CypherDuration {
        months: neg(d.months)?,
        days: neg(d.days)?,
        seconds: neg(d.seconds)?,
        nanos: neg(d.nanos)?,
    })
}

/// Shift a calendar date by whole months (day clamped to the month's end)
/// and then whole days.
fn shift_date(date: CypherDate, months: i64, days: i64) -> crate::types::Result<CypherDate> {
    let mut out = date;
    if months != 0 {
        out = out.add_months(months)?;
    }
    if days != 0 {
        out = out.add_days(days)?;
    }
    Ok(out)
}

/// Shift a date-time by an exact amount of seconds + nanoseconds.
fn shift_exact(
    ndt: CypherLocalDateTime,
    seconds: i64,
    nanos: i64,
) -> crate::types::Result<CypherLocalDateTime> {
    ndt.add_exact(seconds, nanos)
}

/// Add a duration's clock part to a time of day. Times wrap around midnight,
/// so only the seconds modulo one day matter (which also keeps a huge
/// `seconds` component from overflowing).
fn shift_time_of_day(
    t: CypherLocalTime,
    dur: &CypherDuration,
) -> crate::types::Result<CypherLocalTime> {
    let nanos = (dur.seconds % SECONDS_PER_DAY) * 1_000_000_000 + dur.nanos;
    let shifted = t.to_jiff()?.wrapping_add(SignedDuration::from_nanos(nanos));
    Ok(CypherLocalTime::from_jiff(shifted))
}

/// Shift a local date-time by a duration: months and days are calendar
/// arithmetic on the date, seconds and nanoseconds are exact.
fn shift_local(
    ndt: CypherLocalDateTime,
    dur: &CypherDuration,
) -> crate::types::Result<CypherLocalDateTime> {
    let date = shift_date(ndt.date, dur.months, dur.days)?;
    shift_exact(
        CypherLocalDateTime {
            date,
            time: ndt.time,
        },
        dur.seconds,
        dur.nanos,
    )
}

/// The UTC instant (as a civil date-time) of `local` in `tz`, with java.time
/// `ZonedDateTime.ofLocal` semantics: in an overlap keep `preferred` (seconds)
/// if it is one of the two valid offsets (else the earlier one); in a gap,
/// move later by the gap length (interpret `local` with the offset in force
/// before the gap).
fn local_to_utc(
    tz: &TimeZone,
    local: CypherLocalDateTime,
    preferred: i32,
) -> crate::types::Result<CypherLocalDateTime> {
    let offset = match tz_local_offsets(tz, &local)? {
        AmbiguousOffset::Unambiguous { offset } => offset.seconds(),
        AmbiguousOffset::Fold { before, after } => {
            if after.seconds() == preferred {
                after.seconds()
            } else {
                before.seconds()
            }
        }
        // A gap: the offset in force a day earlier is the pre-transition one
        // (the shifted local time is read as a UTC instant, as java.time does
        // for its probe).
        AmbiguousOffset::Gap { .. } => {
            tz_offset_at_utc(tz, &local.add_exact(-SECONDS_PER_DAY, 0)?)?
        }
    };
    local.add_exact(-i64::from(offset), 0)
}

/// Zoned date-time + duration with Neo4j (java.time) semantics: months and
/// days shift the local date-time; seconds and nanoseconds shift the instant.
/// With a named zone the offset is re-resolved after each step, so crossing
/// a DST transition yields the correct wall-clock time and offset.
fn shift_zoned(dt: &CypherDateTime, dur: &CypherDuration) -> crate::types::Result<CypherDateTime> {
    let date = shift_date(dt.local.date, dur.months, dur.days)?;
    let local = CypherLocalDateTime {
        date,
        time: dt.local.time,
    };
    let Some(tz) = dt.tz_name.as_deref().and_then(lookup_tz) else {
        // Fixed offset: wall-clock and instant arithmetic coincide.
        return Ok(CypherDateTime {
            local: shift_exact(local, dur.seconds, dur.nanos)?,
            offset_seconds: dt.offset_seconds,
            tz_name: dt.tz_name.clone(),
        });
    };
    let utc = if local == dt.local {
        // No calendar shift: keep the original offset (it disambiguates an
        // overlap the same way the input did).
        local.add_exact(-i64::from(dt.offset_seconds), 0)?
    } else {
        local_to_utc(&tz, local, dt.offset_seconds)?
    };
    let utc = shift_exact(utc, dur.seconds, dur.nanos)?;
    let (local, offset_seconds) = tz_utc_to_local(&tz, &utc)?;
    Ok(CypherDateTime {
        local,
        offset_seconds,
        tz_name: dt.tz_name.clone(),
    })
}

/// Temporal + Duration arithmetic. Returns Some if handled, None to fall through.
pub(super) fn eval_temporal_add(
    left: &Value,
    right: &Value,
) -> Option<crate::types::Result<Value>> {
    Some(match (left, right) {
        (Value::Duration(a), Value::Duration(b)) => {
            combine_durations(a, b, false).map(Value::Duration)
        }
        (Value::Date(d), Value::Duration(dur)) | (Value::Duration(dur), Value::Date(d)) => {
            // Dates have no clock: whole days of the seconds part count,
            // the sub-day remainder is dropped.
            dur.days
                .checked_add(dur.seconds / SECONDS_PER_DAY)
                .ok_or_else(|| out_of_range("duration days"))
                .and_then(|days| shift_date(*d, dur.months, days))
                .map(Value::Date)
        }
        (Value::LocalTime(t), Value::Duration(dur))
        | (Value::Duration(dur), Value::LocalTime(t)) => {
            shift_time_of_day(*t, dur).map(Value::LocalTime)
        }
        (Value::Time(t), Value::Duration(dur)) | (Value::Duration(dur), Value::Time(t)) => {
            shift_time_of_day(t.time, dur).map(|time| {
                Value::Time(CypherTime {
                    time,
                    offset_seconds: t.offset_seconds,
                })
            })
        }
        (Value::LocalDateTime(dt), Value::Duration(dur))
        | (Value::Duration(dur), Value::LocalDateTime(dt)) => {
            shift_local(*dt, dur).map(Value::LocalDateTime)
        }
        (Value::DateTime(dt), Value::Duration(dur))
        | (Value::Duration(dur), Value::DateTime(dt)) => shift_zoned(dt, dur).map(Value::DateTime),
        _ => return None,
    })
}

/// Temporal - Duration subtraction. Returns Some if handled.
pub(super) fn eval_temporal_sub(
    left: &Value,
    right: &Value,
) -> Option<crate::types::Result<Value>> {
    match (left, right) {
        (Value::Duration(a), Value::Duration(b)) => {
            Some(combine_durations(a, b, true).map(Value::Duration))
        }
        // The parser rewrites unary minus `-x` as `0 - x`, so this is how
        // `-duration(...)` arrives.
        (Value::I64(0), Value::Duration(dur)) => Some(negate_duration(dur).map(Value::Duration)),
        (_, Value::Duration(dur)) => {
            // Temporal - Duration: negate the duration and add.
            let neg = match negate_duration(dur) {
                Ok(n) => n,
                Err(e) => return Some(Err(e)),
            };
            eval_temporal_add(left, &Value::Duration(neg))
        }
        _ => None,
    }
}

/// Duration * Number. Returns Some if handled.
pub(super) fn eval_duration_mul(
    left: &Value,
    right: &Value,
) -> Option<crate::types::Result<Value>> {
    let (dur, n) = match (left, right) {
        (Value::Duration(d), Value::I64(n)) | (Value::I64(n), Value::Duration(d)) => (d, *n),
        (Value::Duration(d), Value::F64(n)) | (Value::F64(n), Value::Duration(d)) => {
            let product = duration_to_total_nanos(d) as f64 * n;
            if !product.is_finite() || product.abs() >= i128::MAX as f64 {
                return Some(Err(out_of_range("duration product")));
            }
            return Some(total_nanos_to_duration(product.round() as i128).map(Value::Duration));
        }
        _ => return None,
    };
    let mul = |x: i64| {
        x.checked_mul(n)
            .ok_or_else(|| out_of_range(format!("duration multiplied by {n}")))
    };
    let result = (|| {
        let (seconds, nanos) = split_nanos(sub_day_nanos(dur) * n as i128)?;
        Ok(CypherDuration {
            months: mul(dur.months)?,
            days: mul(dur.days)?,
            seconds,
            nanos,
        })
    })();
    Some(result.map(Value::Duration))
}

/// Duration / Number: flatten to total nanos, divide, decompose back.
/// Returns None if the operands are not Duration / Number.
pub(super) fn eval_duration_div(
    left: &Value,
    right: &Value,
) -> Option<crate::types::Result<Value>> {
    match (left, right) {
        (Value::Duration(d), Value::I64(n)) => {
            if *n == 0 {
                return Some(Err(super::division_by_zero(left, "/", right)));
            }
            let total = duration_to_total_nanos(d);
            let divided = total / *n as i128;
            Some(total_nanos_to_duration(divided).map(Value::Duration))
        }
        (Value::Duration(d), Value::F64(n)) => {
            // A duration has no infinity, so a zero Float divisor errors too.
            if *n == 0.0 {
                return Some(Err(super::division_by_zero(left, "/", right)));
            }
            // Component-wise division with fractional remainder cascade.
            let nanos_per_sec: f64 = 1_000_000_000.0;
            let nanos_per_day: f64 = 86_400.0 * nanos_per_sec;
            let nanos_per_month: f64 = AVG_SECONDS_PER_MONTH as f64 * nanos_per_sec;

            // Months: divide, cascade fractional remainder to days.
            let months_f = d.months as f64 / n;
            let months_i = months_f.trunc() as i64;
            let month_remainder_nanos = (months_f - months_f.trunc()) * nanos_per_month;

            // Days: divide, adding cascaded month remainder, cascade fractional remainder to sub-day.
            let days_f = d.days as f64 / n + month_remainder_nanos / nanos_per_day;
            let days_i = days_f.trunc() as i64;
            let day_remainder_nanos = (days_f - days_f.trunc()) * nanos_per_day;

            // Sub-day: seconds + nanos divided, plus cascaded day remainder.
            let sub_nanos_f =
                (d.seconds as f64 * nanos_per_sec + d.nanos as f64) / n + day_remainder_nanos;
            let seconds_i = (sub_nanos_f / nanos_per_sec).trunc() as i64;
            let nanos_i = (sub_nanos_f % nanos_per_sec).round() as i64;

            Some(Ok(Value::Duration(CypherDuration {
                months: months_i,
                days: days_i,
                seconds: seconds_i,
                nanos: nanos_i,
            })))
        }
        _ => None,
    }
}

/// Flatten a duration to total nanoseconds using average month length.
pub(super) fn duration_to_total_nanos(d: &CypherDuration) -> i128 {
    let nanos_per_day: i128 = 86_400 * NANOS_PER_SEC;
    let nanos_per_month: i128 = AVG_SECONDS_PER_MONTH as i128 * NANOS_PER_SEC;
    d.months as i128 * nanos_per_month + d.days as i128 * nanos_per_day + sub_day_nanos(d)
}

/// Decompose total nanoseconds back into a CypherDuration.
pub(super) fn total_nanos_to_duration(total: i128) -> crate::types::Result<CypherDuration> {
    let nanos_per_day: i128 = 86_400 * NANOS_PER_SEC;
    let nanos_per_month: i128 = AVG_SECONDS_PER_MONTH as i128 * NANOS_PER_SEC;

    let months = i64::try_from(total / nanos_per_month)
        .map_err(|_| out_of_range("duration months component"))?;
    let remainder = total % nanos_per_month;
    // |remainder| < one month, so days/seconds/nanos always fit.
    let days = (remainder / nanos_per_day) as i64;
    let (seconds, nanos) = split_nanos(remainder % nanos_per_day)?;

    Ok(CypherDuration {
        months,
        days,
        seconds,
        nanos,
    })
}

/// Extract a temporal component accessor (e.g., `d.year`, `t.hour`).
/// Returns None if the value is not temporal or the property is not a known
/// accessor, and a `NumberOutOfRange` error when a duration total (e.g.
/// `d.nanoseconds` of a ~300-year duration) does not fit in an Integer, or
/// when a calendar accessor (week, weekday, ordinal) is asked of a legacy
/// stored date outside the supported years.
pub(super) fn temporal_accessor(val: &Value, prop: &str) -> Option<crate::types::Result<Value>> {
    if let Value::Duration(d) = val {
        let per_second: i128 = match prop {
            "milliseconds" => 1_000,
            "microseconds" => 1_000_000,
            "nanoseconds" => 1_000_000_000,
            _ => return duration_component(d, prop).map(|v| Ok(Value::I64(v))),
        };
        let total = d.seconds as i128 * per_second + d.nanos as i128 / (NANOS_PER_SEC / per_second);
        return Some(
            i64::try_from(total)
                .map(Value::I64)
                .map_err(|_| out_of_range(format!("duration {prop}"))),
        );
    }
    temporal_component(val, prop)
}

/// A date accessor.
fn date_component(d: CypherDate, prop: &str) -> Option<crate::types::Result<i64>> {
    Some(match prop {
        "year" => Ok(i64::from(d.year)),
        "month" => Ok(i64::from(d.month)),
        "day" => Ok(i64::from(d.day)),
        "ordinalDay" => d.ordinal().map(i64::from),
        "weekYear" => d.iso_week().map(|(year, _)| i64::from(year)),
        "week" => d.iso_week().map(|(_, week)| i64::from(week)),
        "dayOfWeek" | "weekDay" => d.weekday().map(i64::from),
        "quarter" => Ok(i64::from(d.quarter())),
        "dayOfQuarter" => (|| {
            let quarter_start = CypherDate::new(d.year, (d.quarter() - 1) * 3 + 1, 1)?;
            Ok(quarter_start.days_until(d)? + 1)
        })(),
        _ => return None,
    })
}

/// A local-time accessor.
fn time_component(t: &CypherLocalTime, prop: &str) -> Option<i64> {
    let nanos = i64::from(t.nanosecond) % 1_000_000_000;
    match prop {
        "hour" => Some(i64::from(t.hour)),
        "minute" => Some(i64::from(t.minute)),
        "second" => Some(i64::from(t.second)),
        "nanosecond" => Some(nanos),
        "microsecond" => Some(nanos / 1_000),
        "millisecond" => Some(nanos / 1_000_000),
        _ => None,
    }
}

/// The accessors behind [`temporal_accessor`] that are not duration totals.
fn temporal_component(val: &Value, prop: &str) -> Option<crate::types::Result<Value>> {
    match val {
        Value::Date(d) => date_component(*d, prop).map(|r| r.map(Value::I64)),
        Value::LocalTime(lt) => time_component(lt, prop).map(|v| Ok(Value::I64(v))),
        Value::Time(ct) => {
            if let Some(v) = time_component(&ct.time, prop) {
                return Some(Ok(Value::I64(v)));
            }
            let off = ct.offset_seconds;
            match prop {
                "offset" | "timezone" => {
                    Some(Ok(Value::String(crate::temporal::fmt_offset_public(off))))
                }
                "offsetMinutes" => Some(Ok(Value::I64(i64::from(off) / 60))),
                "offsetSeconds" => Some(Ok(Value::I64(i64::from(off)))),
                _ => None,
            }
        }
        Value::LocalDateTime(dt) => {
            // Try date accessors first, then time accessors.
            date_component(dt.date, prop)
                .map(|r| r.map(Value::I64))
                .or_else(|| time_component(&dt.time, prop).map(|v| Ok(Value::I64(v))))
        }
        Value::DateTime(dt) => {
            // DateTime-specific accessors first.
            match prop {
                "timezone" => {
                    // Return named tz if available, otherwise the offset string.
                    return Some(Ok(Value::String(dt.tz_name.clone().unwrap_or_else(|| {
                        crate::temporal::fmt_offset_public(dt.offset_seconds)
                    }))));
                }
                "epochSeconds" => return Some(Ok(Value::I64(dt.epoch_seconds()))),
                "epochMillis" => return Some(Ok(Value::I64(dt.epoch_millis()))),
                _ => {}
            }
            // Try date accessors, then time accessors, then offset accessors.
            if let Some(r) = date_component(dt.local.date, prop) {
                return Some(r.map(Value::I64));
            }
            temporal_component(
                &Value::Time(CypherTime {
                    time: dt.local.time,
                    offset_seconds: dt.offset_seconds,
                }),
                prop,
            )
        }
        Value::Duration(d) => duration_component(d, prop).map(|v| Ok(Value::I64(v))),
        _ => None,
    }
}

/// The non-failing duration accessors.
fn duration_component(d: &CypherDuration, prop: &str) -> Option<i64> {
    match prop {
        "years" => Some(d.months / 12),
        "quarters" => Some(d.months / 3),
        "months" => Some(d.months),
        "weeks" => Some(d.days / 7),
        "days" => Some(d.days),
        "hours" => Some(d.seconds / 3600),
        "minutes" => Some(d.seconds / 60),
        "seconds" => Some(d.seconds),
        "quartersOfYear" => Some((d.months % 12) / 3),
        "monthsOfQuarter" => Some(d.months % 3),
        "monthsOfYear" => Some(d.months % 12),
        "daysOfWeek" => Some(d.days % 7),
        "minutesOfHour" => Some((d.seconds / 60) % 60),
        "secondsOfMinute" => Some(d.seconds % 60),
        "millisecondsOfSecond" => Some(d.nanos / 1_000_000),
        "microsecondsOfSecond" => Some(d.nanos / 1_000),
        "nanosecondsOfSecond" => Some(d.nanos),
        _ => None,
    }
}
