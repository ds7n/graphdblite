//! Temporal arithmetic operations for Cypher eval.
//!
//! Handles Duration +/- Duration, Temporal +/- Duration, Duration * Number,
//! Duration / Number, and temporal property accessors (e.g., `d.year`).
//!
//! All arithmetic is checked: a result outside the representable range
//! (i64 duration components, chrono's calendar range) is a
//! `NumberOutOfRange` error, never a panic or a silently wrong value.

use crate::temporal::{
    CypherDate, CypherDateTime, CypherDuration, CypherLocalDateTime, CypherLocalTime, CypherTime,
};
use crate::types::{ErrorCode, GraphError, QueryPhase, Value};
use chrono::{
    FixedOffset, LocalResult, Months, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeDelta,
    TimeZone,
};

/// Average seconds per month used for duration arithmetic (365.2425 * 86400 / 12).
const AVG_SECONDS_PER_MONTH: i64 = 2_629_746;

const NANOS_PER_SEC: i128 = 1_000_000_000;
const SECONDS_PER_DAY: i64 = 86_400;

/// The `NumberOutOfRange` error for a temporal result that does not fit.
fn out_of_range(what: impl std::fmt::Display) -> GraphError {
    GraphError::number_out_of_range(QueryPhase::Runtime, format!("{what} is out of range"))
}

/// `chrono::Months::new` requires its argument to fit in `i32`; values above
/// `i32::MAX` make the underlying chrono ops panic. Validate up front so a
/// malicious `duration({months: 9999999999999})` returns a structured error
/// rather than aborting the host process. See security finding H4.
fn months_abs_to_u32(months: i64) -> crate::types::Result<u32> {
    let abs = months.unsigned_abs();
    if abs > i32::MAX as u64 {
        return Err(GraphError::query(
            QueryPhase::Runtime,
            ErrorCode::NumberOutOfRange,
            format!(
                "duration months component `{months}` is out of range \
                 (must fit in i32 to apply to a date/datetime)"
            ),
        ));
    }
    Ok(abs as u32)
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
fn shift_date(date: NaiveDate, months: i64, days: i64) -> crate::types::Result<NaiveDate> {
    let mut out = date;
    if months != 0 {
        let m = Months::new(months_abs_to_u32(months)?);
        let shifted = if months > 0 {
            out.checked_add_months(m)
        } else {
            out.checked_sub_months(m)
        };
        out = shifted.ok_or_else(|| out_of_range(format!("{date} shifted by {months} months")))?;
    }
    if days != 0 {
        out = TimeDelta::try_days(days)
            .and_then(|delta| out.checked_add_signed(delta))
            .ok_or_else(|| out_of_range(format!("{date} shifted by {days} days")))?;
    }
    Ok(out)
}

/// Shift a date-time by an exact amount of seconds + nanoseconds.
fn shift_exact(
    ndt: NaiveDateTime,
    seconds: i64,
    nanos: i64,
) -> crate::types::Result<NaiveDateTime> {
    TimeDelta::try_seconds(seconds)
        .and_then(|delta| delta.checked_add(&TimeDelta::nanoseconds(nanos)))
        .and_then(|delta| ndt.checked_add_signed(delta))
        .ok_or_else(|| out_of_range(format!("{ndt} shifted by {seconds}s {nanos}ns")))
}

/// Add a duration's clock part to a time of day. Times wrap around midnight,
/// so only the seconds modulo one day matter (which also keeps a huge
/// `seconds` component from overflowing `TimeDelta`).
fn shift_time_of_day(t: NaiveTime, dur: &CypherDuration) -> NaiveTime {
    let nanos = (dur.seconds % SECONDS_PER_DAY) * 1_000_000_000 + dur.nanos;
    t.overflowing_add_signed(TimeDelta::nanoseconds(nanos)).0
}

/// Shift a local date-time by a duration: months and days are calendar
/// arithmetic on the date, seconds and nanoseconds are exact.
fn shift_local(ndt: NaiveDateTime, dur: &CypherDuration) -> crate::types::Result<NaiveDateTime> {
    let date = shift_date(ndt.date(), dur.months, dur.days)?;
    shift_exact(NaiveDateTime::new(date, ndt.time()), dur.seconds, dur.nanos)
}

/// The UTC instant of `local` in `tz`, with java.time `ZonedDateTime.ofLocal`
/// semantics: in an overlap keep `preferred` if it is one of the two valid
/// offsets (else the earlier one); in a gap, move later by the gap length
/// (interpret `local` with the offset in force before the gap).
fn local_to_utc(tz: &chrono_tz::Tz, local: NaiveDateTime, preferred: FixedOffset) -> NaiveDateTime {
    let offset = match tz.offset_from_local_datetime(&local) {
        LocalResult::Single(o) => o.fix(),
        LocalResult::Ambiguous(earlier, later) => {
            if later.fix() == preferred {
                later.fix()
            } else {
                earlier.fix()
            }
        }
        // A gap: the offset in force a day earlier is the pre-transition one.
        LocalResult::None => tz
            .offset_from_utc_datetime(&(local - TimeDelta::days(1)))
            .fix(),
    };
    local - TimeDelta::seconds(offset.local_minus_utc() as i64)
}

/// Zoned date-time + duration with Neo4j (java.time) semantics: months and
/// days shift the local date-time; seconds and nanoseconds shift the instant.
/// With a named zone the offset is re-resolved after each step, so crossing
/// a DST transition yields the correct wall-clock time and offset.
fn shift_zoned(dt: &CypherDateTime, dur: &CypherDuration) -> crate::types::Result<CypherDateTime> {
    let date = shift_date(dt.0.date(), dur.months, dur.days)?;
    let local = NaiveDateTime::new(date, dt.0.time());
    let tz =
        dt.2.as_deref()
            .and_then(|n| n.parse::<chrono_tz::Tz>().ok());
    let Some(tz) = tz else {
        // Fixed offset: wall-clock and instant arithmetic coincide.
        let ndt = shift_exact(local, dur.seconds, dur.nanos)?;
        return Ok(CypherDateTime(ndt, dt.1, dt.2.clone()));
    };
    let utc = if local == dt.0 {
        // No calendar shift: keep the original offset (it disambiguates an
        // overlap the same way the input did).
        local - TimeDelta::seconds(dt.1.local_minus_utc() as i64)
    } else {
        local_to_utc(&tz, local, dt.1)
    };
    let utc = shift_exact(utc, dur.seconds, dur.nanos)?;
    let aware = tz.from_utc_datetime(&utc);
    Ok(CypherDateTime(
        aware.naive_local(),
        aware.offset().fix(),
        dt.2.clone(),
    ))
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
                .and_then(|days| shift_date(d.0, dur.months, days))
                .map(|date| Value::Date(CypherDate(date)))
        }
        (Value::LocalTime(t), Value::Duration(dur))
        | (Value::Duration(dur), Value::LocalTime(t)) => Ok(Value::LocalTime(CypherLocalTime(
            shift_time_of_day(t.0, dur),
        ))),
        (Value::Time(t), Value::Duration(dur)) | (Value::Duration(dur), Value::Time(t)) => {
            Ok(Value::Time(CypherTime(shift_time_of_day(t.0, dur), t.1)))
        }
        (Value::LocalDateTime(dt), Value::Duration(dur))
        | (Value::Duration(dur), Value::LocalDateTime(dt)) => {
            shift_local(dt.0, dur).map(|ndt| Value::LocalDateTime(CypherLocalDateTime(ndt)))
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
/// `d.nanoseconds` of a ~300-year duration) does not fit in an Integer.
pub(super) fn temporal_accessor(val: &Value, prop: &str) -> Option<crate::types::Result<Value>> {
    if let Value::Duration(d) = val {
        let per_second: i128 = match prop {
            "milliseconds" => 1_000,
            "microseconds" => 1_000_000,
            "nanoseconds" => 1_000_000_000,
            _ => return temporal_component(val, prop).map(Ok),
        };
        let total = d.seconds as i128 * per_second + d.nanos as i128 / (NANOS_PER_SEC / per_second);
        return Some(
            i64::try_from(total)
                .map(Value::I64)
                .map_err(|_| out_of_range(format!("duration {prop}"))),
        );
    }
    temporal_component(val, prop).map(Ok)
}

/// The non-failing accessors behind [`temporal_accessor`].
fn temporal_component(val: &Value, prop: &str) -> Option<Value> {
    use chrono::{Datelike, Timelike};

    match val {
        Value::Date(d) => match prop {
            "year" => Some(Value::I64(d.0.year() as i64)),
            "month" => Some(Value::I64(d.0.month() as i64)),
            "day" => Some(Value::I64(d.0.day() as i64)),
            "ordinalDay" => Some(Value::I64(d.0.ordinal() as i64)),
            "weekYear" => Some(Value::I64(d.0.iso_week().year() as i64)),
            "week" => Some(Value::I64(d.0.iso_week().week() as i64)),
            "dayOfWeek" | "weekDay" => {
                Some(Value::I64(d.0.weekday().num_days_from_monday() as i64 + 1))
            }
            "quarter" => Some(Value::I64(((d.0.month() - 1) / 3 + 1) as i64)),
            "dayOfQuarter" => {
                let q_start_month = ((d.0.month() - 1) / 3) * 3 + 1;
                let q_start =
                    chrono::NaiveDate::from_ymd_opt(d.0.year(), q_start_month, 1).unwrap();
                Some(Value::I64((d.0 - q_start).num_days() + 1))
            }
            _ => None,
        },
        Value::LocalTime(lt) => {
            let t = &lt.0;
            match prop {
                "hour" => Some(Value::I64(t.hour() as i64)),
                "minute" => Some(Value::I64(t.minute() as i64)),
                "second" => Some(Value::I64(t.second() as i64)),
                "nanosecond" => Some(Value::I64(t.nanosecond() as i64 % 1_000_000_000)),
                "microsecond" => Some(Value::I64((t.nanosecond() as i64 % 1_000_000_000) / 1_000)),
                "millisecond" => Some(Value::I64(
                    (t.nanosecond() as i64 % 1_000_000_000) / 1_000_000,
                )),
                _ => None,
            }
        }
        Value::Time(ct) => {
            let t = &ct.0;
            match prop {
                "hour" => Some(Value::I64(t.hour() as i64)),
                "minute" => Some(Value::I64(t.minute() as i64)),
                "second" => Some(Value::I64(t.second() as i64)),
                "nanosecond" => Some(Value::I64(t.nanosecond() as i64 % 1_000_000_000)),
                "microsecond" => Some(Value::I64((t.nanosecond() as i64 % 1_000_000_000) / 1_000)),
                "millisecond" => Some(Value::I64(
                    (t.nanosecond() as i64 % 1_000_000_000) / 1_000_000,
                )),
                "offset" | "timezone" => {
                    Some(Value::String(crate::temporal::fmt_offset_public(&ct.1)))
                }
                "offsetMinutes" => Some(Value::I64(ct.1.local_minus_utc() as i64 / 60)),
                "offsetSeconds" => Some(Value::I64(ct.1.local_minus_utc() as i64)),
                _ => None,
            }
        }
        Value::LocalDateTime(dt) => {
            // Try date accessors first, then time accessors.
            let date_val = Value::Date(CypherDate(dt.0.date()));
            if let Some(v) = temporal_component(&date_val, prop) {
                return Some(v);
            }
            let time_val = Value::LocalTime(CypherLocalTime(dt.0.time()));
            temporal_component(&time_val, prop)
        }
        Value::DateTime(dt) => {
            // DateTime-specific accessors first.
            match prop {
                "timezone" => {
                    // Return named tz if available, otherwise the offset string.
                    return Some(Value::String(
                        dt.2.clone()
                            .unwrap_or_else(|| crate::temporal::fmt_offset_public(&dt.1)),
                    ));
                }
                "epochSeconds" => {
                    let utc_ndt = dt.0 - chrono::Duration::seconds(dt.1.local_minus_utc() as i64);
                    let epoch = utc_ndt.and_utc().timestamp();
                    return Some(Value::I64(epoch));
                }
                "epochMillis" => {
                    let utc_ndt = dt.0 - chrono::Duration::seconds(dt.1.local_minus_utc() as i64);
                    let epoch = utc_ndt.and_utc().timestamp_millis();
                    return Some(Value::I64(epoch));
                }
                _ => {}
            }
            // Try date accessors, then time accessors, then offset accessors.
            let date_val = Value::Date(CypherDate(dt.0.date()));
            if let Some(v) = temporal_component(&date_val, prop) {
                return Some(v);
            }
            let time_val = Value::Time(CypherTime(dt.0.time(), dt.1));
            temporal_component(&time_val, prop)
        }
        Value::Duration(d) => match prop {
            "years" => Some(Value::I64(d.months / 12)),
            "quarters" => Some(Value::I64(d.months / 3)),
            "months" => Some(Value::I64(d.months)),
            "weeks" => Some(Value::I64(d.days / 7)),
            "days" => Some(Value::I64(d.days)),
            "hours" => Some(Value::I64(d.seconds / 3600)),
            "minutes" => Some(Value::I64(d.seconds / 60)),
            "seconds" => Some(Value::I64(d.seconds)),
            "quartersOfYear" => Some(Value::I64((d.months % 12) / 3)),
            "monthsOfQuarter" => Some(Value::I64(d.months % 3)),
            "monthsOfYear" => Some(Value::I64(d.months % 12)),
            "daysOfWeek" => Some(Value::I64(d.days % 7)),
            "minutesOfHour" => Some(Value::I64((d.seconds / 60) % 60)),
            "secondsOfMinute" => Some(Value::I64(d.seconds % 60)),
            "millisecondsOfSecond" => Some(Value::I64(d.nanos / 1_000_000)),
            "microsecondsOfSecond" => Some(Value::I64(d.nanos / 1_000)),
            "nanosecondsOfSecond" => Some(Value::I64(d.nanos)),
            _ => None,
        },
        _ => None,
    }
}
