//! Temporal arithmetic operations for Cypher eval.
//!
//! Handles Duration +/- Duration, Temporal +/- Duration, Duration * Number,
//! Duration / Number, and temporal property accessors (e.g., `d.year`).

use crate::temporal::{
    CypherDate, CypherDateTime, CypherDuration, CypherLocalDateTime, CypherLocalTime, CypherTime,
};
use crate::types::{ErrorCode, GraphError, QueryPhase, Value};
use chrono::{Months, NaiveDateTime};

/// Average seconds per month used for duration arithmetic (365.2425 * 86400 / 12).
const AVG_SECONDS_PER_MONTH: i64 = 2_629_746;

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

/// Temporal + Duration arithmetic. Returns Some if handled, None to fall through.
pub(super) fn eval_temporal_add(
    left: &Value,
    right: &Value,
) -> Option<crate::types::Result<Value>> {
    match (left, right) {
        (Value::Duration(a), Value::Duration(b)) => {
            let mut nanos = a.nanos + b.nanos;
            let mut seconds = a.seconds + b.seconds;
            // Normalize: carry nanos overflow into seconds.
            if nanos >= 1_000_000_000 || nanos <= -1_000_000_000 {
                seconds += nanos / 1_000_000_000;
                nanos %= 1_000_000_000;
            }
            if seconds > 0 && nanos < 0 {
                seconds -= 1;
                nanos += 1_000_000_000;
            } else if seconds < 0 && nanos > 0 {
                seconds += 1;
                nanos -= 1_000_000_000;
            }
            Some(Ok(Value::Duration(CypherDuration {
                months: a.months + b.months,
                days: a.days + b.days,
                seconds,
                nanos,
            })))
        }
        (Value::Date(d), Value::Duration(dur)) | (Value::Duration(dur), Value::Date(d)) => {
            let mut date = d.0;
            if dur.months != 0 {
                let months_u32 = match months_abs_to_u32(dur.months) {
                    Ok(n) => n,
                    Err(e) => return Some(Err(e)),
                };
                if dur.months > 0 {
                    date = date
                        .checked_add_months(Months::new(months_u32))
                        .unwrap_or(date);
                } else {
                    date = date
                        .checked_sub_months(Months::new(months_u32))
                        .unwrap_or(date);
                }
            }
            date += chrono::Duration::days(dur.days);
            date = date
                + chrono::Duration::seconds(dur.seconds)
                + chrono::Duration::nanoseconds(dur.nanos);
            Some(Ok(Value::Date(CypherDate(date))))
        }
        (Value::LocalTime(t), Value::Duration(dur))
        | (Value::Duration(dur), Value::LocalTime(t)) => {
            let time = t.0
                + chrono::Duration::seconds(dur.seconds)
                + chrono::Duration::nanoseconds(dur.nanos);
            Some(Ok(Value::LocalTime(CypherLocalTime(time))))
        }
        (Value::Time(t), Value::Duration(dur)) | (Value::Duration(dur), Value::Time(t)) => {
            let time = t.0
                + chrono::Duration::seconds(dur.seconds)
                + chrono::Duration::nanoseconds(dur.nanos);
            Some(Ok(Value::Time(CypherTime(time, t.1))))
        }
        (Value::LocalDateTime(dt), Value::Duration(dur))
        | (Value::Duration(dur), Value::LocalDateTime(dt)) => {
            let mut date = dt.0.date();
            if dur.months != 0 {
                let months_u32 = match months_abs_to_u32(dur.months) {
                    Ok(n) => n,
                    Err(e) => return Some(Err(e)),
                };
                if dur.months > 0 {
                    date = date
                        .checked_add_months(Months::new(months_u32))
                        .unwrap_or(date);
                } else {
                    date = date
                        .checked_sub_months(Months::new(months_u32))
                        .unwrap_or(date);
                }
            }
            date += chrono::Duration::days(dur.days);
            let ndt = NaiveDateTime::new(date, dt.0.time())
                + chrono::Duration::seconds(dur.seconds)
                + chrono::Duration::nanoseconds(dur.nanos);
            Some(Ok(Value::LocalDateTime(CypherLocalDateTime(ndt))))
        }
        (Value::DateTime(dt), Value::Duration(dur))
        | (Value::Duration(dur), Value::DateTime(dt)) => {
            let mut date = dt.0.date();
            if dur.months != 0 {
                let months_u32 = match months_abs_to_u32(dur.months) {
                    Ok(n) => n,
                    Err(e) => return Some(Err(e)),
                };
                if dur.months > 0 {
                    date = date
                        .checked_add_months(Months::new(months_u32))
                        .unwrap_or(date);
                } else {
                    date = date
                        .checked_sub_months(Months::new(months_u32))
                        .unwrap_or(date);
                }
            }
            date += chrono::Duration::days(dur.days);
            let ndt = NaiveDateTime::new(date, dt.0.time())
                + chrono::Duration::seconds(dur.seconds)
                + chrono::Duration::nanoseconds(dur.nanos);
            Some(Ok(Value::DateTime(CypherDateTime(ndt, dt.1, dt.2.clone()))))
        }
        _ => None,
    }
}

/// Temporal - Duration subtraction. Returns Some if handled.
pub(super) fn eval_temporal_sub(
    left: &Value,
    right: &Value,
) -> Option<crate::types::Result<Value>> {
    match (left, right) {
        (Value::Duration(a), Value::Duration(b)) => {
            let mut nanos = a.nanos - b.nanos;
            let mut seconds = a.seconds - b.seconds;
            if nanos >= 1_000_000_000 || nanos <= -1_000_000_000 {
                seconds += nanos / 1_000_000_000;
                nanos %= 1_000_000_000;
            }
            if seconds > 0 && nanos < 0 {
                seconds -= 1;
                nanos += 1_000_000_000;
            } else if seconds < 0 && nanos > 0 {
                seconds += 1;
                nanos -= 1_000_000_000;
            }
            Some(Ok(Value::Duration(CypherDuration {
                months: a.months - b.months,
                days: a.days - b.days,
                seconds,
                nanos,
            })))
        }
        (_, Value::Duration(dur)) => {
            // Temporal - Duration → negate duration and add.
            let neg = CypherDuration {
                months: -dur.months,
                days: -dur.days,
                seconds: -dur.seconds,
                nanos: -dur.nanos,
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
            let total = duration_to_total_nanos(d);
            let result = (total as f64 * n).round() as i128;
            return Some(Ok(Value::Duration(total_nanos_to_duration(result))));
        }
        _ => return None,
    };
    Some(Ok(Value::Duration(CypherDuration {
        months: dur.months * n,
        days: dur.days * n,
        seconds: dur.seconds * n,
        nanos: dur.nanos * n,
    })))
}

/// Duration / Number — flatten to total nanos, divide, decompose back.
/// Returns None if the operands are not Duration / Number.
pub(super) fn eval_duration_div(
    left: &Value,
    right: &Value,
) -> Option<crate::types::Result<Value>> {
    match (left, right) {
        (Value::Duration(d), Value::I64(n)) => {
            if *n == 0 {
                return Some(Ok(Value::Null));
            }
            let total = duration_to_total_nanos(d);
            let divided = total / *n as i128;
            Some(Ok(Value::Duration(total_nanos_to_duration(divided))))
        }
        (Value::Duration(d), Value::F64(n)) => {
            if *n == 0.0 {
                return Some(Ok(Value::Null));
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
    let nanos_per_sec: i128 = 1_000_000_000;
    let nanos_per_day: i128 = 86_400 * nanos_per_sec;
    let nanos_per_month: i128 = AVG_SECONDS_PER_MONTH as i128 * nanos_per_sec;
    d.months as i128 * nanos_per_month
        + d.days as i128 * nanos_per_day
        + d.seconds as i128 * nanos_per_sec
        + d.nanos as i128
}

/// Decompose total nanoseconds back into a CypherDuration.
pub(super) fn total_nanos_to_duration(total: i128) -> CypherDuration {
    let nanos_per_sec: i128 = 1_000_000_000;
    let nanos_per_day: i128 = 86_400 * nanos_per_sec;
    let nanos_per_month: i128 = AVG_SECONDS_PER_MONTH as i128 * nanos_per_sec;

    let months = total / nanos_per_month;
    let remainder = total % nanos_per_month;
    let days = remainder / nanos_per_day;
    let remainder = remainder % nanos_per_day;
    let seconds = remainder / nanos_per_sec;
    let nanos = remainder % nanos_per_sec;

    CypherDuration {
        months: months as i64,
        days: days as i64,
        seconds: seconds as i64,
        nanos: nanos as i64,
    }
}

/// Extract a temporal component accessor (e.g., `d.year`, `t.hour`).
/// Returns None if the value is not temporal or the property is not a known accessor.
pub(super) fn temporal_accessor(val: &Value, prop: &str) -> Option<Value> {
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
            if let Some(v) = temporal_accessor(&date_val, prop) {
                return Some(v);
            }
            let time_val = Value::LocalTime(CypherLocalTime(dt.0.time()));
            temporal_accessor(&time_val, prop)
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
            if let Some(v) = temporal_accessor(&date_val, prop) {
                return Some(v);
            }
            let time_val = Value::Time(CypherTime(dt.0.time(), dt.1));
            temporal_accessor(&time_val, prop)
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
            "nanoseconds" => Some(Value::I64(d.seconds * 1_000_000_000 + d.nanos)),
            "milliseconds" => Some(Value::I64(d.seconds * 1_000 + d.nanos / 1_000_000)),
            "microseconds" => Some(Value::I64(d.seconds * 1_000_000 + d.nanos / 1_000)),
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
