//! Regression tests for workstream WS-E of the October 2026 code review:
//! expression evaluation, temporal arithmetic, and planner leftovers.
//!
//! Each test fails on the pre-fix code; ids in comments match the WS-E
//! finding list (E1..E17; `code #N` = code report table row).

use graphdblite::{Database, ErrorCode, GraphError, QueryError, Record, Value};

fn mem() -> Database {
    Database::open_memory().unwrap()
}

fn run(db: &mut Database, q: &str) -> Vec<Record> {
    db.execute(q)
        .unwrap_or_else(|e| panic!("query failed: {e}\n{q}"))
}

/// Run `q` (which must return exactly one row) and return its first column.
fn one(db: &mut Database, q: &str) -> Value {
    let rows = run(db, q);
    assert_eq!(rows.len(), 1, "expected one row from {q}, got {rows:?}");
    let v = rows[0]
        .values()
        .next()
        .cloned()
        .unwrap_or_else(|| panic!("no columns from {q}"));
    v
}

/// Shorthand: evaluate a single expression in a fresh database.
fn eval(expr: &str) -> Value {
    one(&mut mem(), &format!("RETURN {expr} AS v"))
}

/// Run `q` in a fresh database and return the first column of every row.
fn eval_rows(q: &str) -> Vec<Value> {
    run(&mut mem(), q)
        .iter()
        .map(|r| r.values().next().cloned().expect("a column"))
        .collect()
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

fn strs(items: &[&str]) -> Value {
    Value::List(items.iter().map(|x| s(x)).collect())
}

/// Run `q`, which must fail with a `QueryError`; return it.
fn query_err(db: &mut Database, q: &str) -> QueryError {
    match db.execute(q) {
        Ok(rows) => panic!("expected an error, got {rows:?} for {q}"),
        Err(GraphError::Query(e)) => e,
        Err(other) => panic!("expected a QueryError for {q}, got {other:?}"),
    }
}

/// Evaluate `expr`, which must fail with the given openCypher kind + code,
/// and with a message containing `needle`.
fn assert_eval_err(expr: &str, kind: &str, code: ErrorCode, needle: &str) {
    let q = format!("RETURN {expr} AS v");
    let e = query_err(&mut mem(), &q);
    assert_eq!(e.kind(), kind, "{q}: {e}");
    assert_eq!(e.code(), code, "{q}: {e}");
    assert!(
        e.message().contains(needle),
        "{q}: message {e} lacks {needle:?}"
    );
}

// ── E1 (code #18): string length counts characters, not UTF-8 bytes ───────

#[test]
fn e1_size_and_length_count_characters() {
    assert_eq!(eval("size('héllo')"), Value::I64(5));
    assert_eq!(eval("length('héllo')"), Value::I64(5));
    assert_eq!(eval("size('日本')"), Value::I64(2));
    assert_eq!(eval("size('')"), Value::I64(0));
    // ASCII control: byte count and char count agree.
    assert_eq!(eval("size('abc')"), Value::I64(3));
}

// ── E7: round(x, precision [, mode]) ───────────────────────────────────────

#[test]
fn e7_round_with_precision_is_half_up() {
    assert_eq!(eval("round(2.71828, 2)"), Value::F64(2.72));
    assert_eq!(eval("round(3.145, 2)"), Value::F64(3.15));
    // Decimal (not binary) HALF_UP: 2.675 is stored as 2.67499..., Neo4j
    // rounds the shortest decimal representation.
    assert_eq!(eval("round(2.675, 2)"), Value::F64(2.68));
    // HALF_UP rounds ties away from zero.
    assert_eq!(eval("round(-1.55, 1)"), Value::F64(-1.6));
    assert_eq!(eval("round(1234.5678, 0)"), Value::F64(1235.0));
    assert_eq!(eval("round(1234.5678, -2)"), Value::F64(1200.0));
    assert_eq!(eval("round(5, 2)"), Value::F64(5.0));
    assert_eq!(eval("round(0.5, -3)"), Value::F64(0.0));
    assert_eq!(eval("round(null, 2)"), Value::Null);
    assert_eq!(eval("round(1.5, null)"), Value::Null);
    // Single-argument form unchanged.
    assert_eq!(eval("round(2.5)"), Value::F64(3.0));
}

#[test]
fn e7_round_with_mode() {
    assert_eq!(eval("round(1.25, 1, 'HALF_EVEN')"), Value::F64(1.2));
    assert_eq!(eval("round(1.35, 1, 'HALF_EVEN')"), Value::F64(1.4));
    assert_eq!(eval("round(1.25, 1, 'HALF_DOWN')"), Value::F64(1.2));
    assert_eq!(eval("round(1.21, 1, 'UP')"), Value::F64(1.3));
    assert_eq!(eval("round(-1.21, 1, 'UP')"), Value::F64(-1.3));
    assert_eq!(eval("round(1.29, 1, 'DOWN')"), Value::F64(1.2));
    assert_eq!(eval("round(-1.21, 1, 'CEILING')"), Value::F64(-1.2));
    assert_eq!(eval("round(-1.21, 1, 'FLOOR')"), Value::F64(-1.3));
    assert_eq!(eval("round(1.25, 1, 'HALF_UP')"), Value::F64(1.3));
}

#[test]
fn e7_round_rejects_unknown_mode_and_extra_args() {
    assert_eval_err(
        "round(1.25, 1, 'SIDEWAYS')",
        "ArgumentError",
        ErrorCode::InvalidArgumentValue,
        "SIDEWAYS",
    );
    let e = query_err(&mut mem(), "RETURN round(1.25, 1, 'UP', 4) AS v");
    assert_eq!(e.code(), ErrorCode::InvalidNumberOfArguments, "{e}");
}

// ── E10: string / conversion function edge cases ───────────────────────────

#[test]
fn e10_split_on_empty_delimiter_yields_characters() {
    assert_eq!(eval("split('abc', '')"), strs(&["a", "b", "c"]));
    assert_eq!(eval("split('日本', '')"), strs(&["日", "本"]));
    // Non-empty delimiter unchanged.
    assert_eq!(eval("split('a,b,', ',')"), strs(&["a", "b", ""]));
}

#[test]
fn e10_substring_with_null_length_is_null() {
    assert_eq!(eval("substring('abc', 1, null)"), Value::Null);
    assert_eq!(eval("substring('abc', null)"), Value::Null);
    assert_eq!(eval("substring('abc', 1, 1)"), s("b"));
    assert_eq!(eval("substring('abc', 1)"), s("bc"));
}

#[test]
fn e10_to_string_formats_floats_like_neo4j() {
    assert_eq!(eval("toString(1e20)"), s("1.0E20"));
    assert_eq!(eval("toString(1.5e-7)"), s("1.5E-7"));
    assert_eq!(eval("toString(-2.5e10)"), s("-2.5E10"));
    // Plain decimal notation in [1e-3, 1e7).
    assert_eq!(eval("toString(1234567.0)"), s("1234567.0"));
    assert_eq!(eval("toString(0.001)"), s("0.001"));
    assert_eq!(eval("toString(2.3)"), s("2.3"));
    // Boundaries: 1e7 and just below 1e-3 switch to scientific.
    assert_eq!(eval("toString(10000000.0)"), s("1.0E7"));
    assert_eq!(eval("toString(0.0009)"), s("9.0E-4"));
    assert_eq!(eval("toString(0.0)"), s("0.0"));
    assert_eq!(eval("toString(1.0/0.0)"), s("Infinity"));
    assert_eq!(eval("toString(-1.0/0.0)"), s("-Infinity"));
    assert_eq!(eval("toString(0.0/0.0)"), s("NaN"));
}

#[test]
fn e10_to_integer_of_nan_is_null() {
    assert_eq!(eval("toInteger(0.0/0.0)"), Value::Null);
    assert_eq!(eval("toInteger('NaN')"), Value::Null);
    assert_eq!(eval("toInteger(2.9)"), Value::I64(2));
    assert_eq!(eval("toInteger('2.9')"), Value::I64(2));
}

#[test]
fn e10_to_float_accepts_only_java_spellings_of_non_finite() {
    match eval("toFloat('NaN')") {
        Value::F64(f) => assert!(f.is_nan(), "expected NaN, got {f}"),
        other => panic!("expected Float NaN, got {other:?}"),
    }
    assert_eq!(eval("toFloat('Infinity')"), Value::F64(f64::INFINITY));
    assert_eq!(eval("toFloat('-Infinity')"), Value::F64(f64::NEG_INFINITY));
    // Rust-only spellings are not numbers in Cypher.
    assert_eq!(eval("toFloat('inf')"), Value::Null);
    assert_eq!(eval("toFloat('nan')"), Value::Null);
    assert_eq!(eval("toFloat('1.5')"), Value::F64(1.5));
}

// ── E6 / E10: comprehension scoping and null input ─────────────────────────

#[test]
fn e6_comprehension_variable_shadows_outer_node() {
    let mut db = mem();
    run(
        &mut db,
        "CREATE (:P {name: 'outer'})-[:R {w: 9}]->(:P {name: 'b'})",
    );
    assert_eq!(
        one(
            &mut db,
            "MATCH (x:P {name: 'outer'}) RETURN [x IN [{name: 'inner'}] | x.name] AS v"
        ),
        strs(&["inner"])
    );
    assert_eq!(
        one(
            &mut db,
            "MATCH ()-[r:R]->() RETURN [r IN [{w: 1}] | r.w] AS v"
        ),
        Value::List(vec![Value::I64(1)])
    );
    // Filter predicate sees the inner binding too.
    assert_eq!(
        one(
            &mut db,
            "MATCH (x:P {name: 'outer'}) RETURN [x IN [{name: 'inner'}] WHERE x.name = 'inner' | 1] AS v"
        ),
        Value::List(vec![Value::I64(1)])
    );
    // The outer binding is untouched after the comprehension.
    assert_eq!(
        one(
            &mut db,
            "MATCH (x:P {name: 'outer'}) WITH x, [x IN [1] | x] AS l RETURN x.name AS v"
        ),
        s("outer")
    );
}

#[test]
fn e6_quantifier_variable_shadows_outer_node() {
    let mut db = mem();
    run(&mut db, "CREATE (:P {name: 'a'}), (:P {name: 'b'})");
    assert_eq!(
        one(
            &mut db,
            "MATCH (x:P {name: 'a'}), (y:P {name: 'b'}) RETURN any(x IN [y] WHERE x.name = 'b') AS v"
        ),
        Value::Bool(true)
    );
    assert_eq!(
        one(
            &mut db,
            "MATCH (x:P {name: 'a'}) RETURN all(x IN [{name: 'z'}] WHERE x.name = 'z') AS v"
        ),
        Value::Bool(true)
    );
}

#[test]
fn e10_list_comprehension_over_null_is_null() {
    assert_eq!(eval("[x IN null | x]"), Value::Null);
    assert_eq!(eval("[x IN null WHERE x > 1]"), Value::Null);
    assert_eq!(eval("[x IN [] | x]"), Value::List(vec![]));
}

// ── E2: temporal arithmetic overflows error instead of panicking ───────────

/// `toString(expr)`, which must succeed.
fn tstr(expr: &str) -> Value {
    eval(&format!("toString({expr})"))
}

fn assert_out_of_range(expr: &str) {
    assert_eval_err(
        expr,
        "ArithmeticError",
        ErrorCode::NumberOutOfRange,
        "out of range",
    );
}

#[test]
fn e2_date_plus_huge_duration_errors() {
    assert_out_of_range("date('2024-01-01') + duration({days: 100000000})");
    assert_out_of_range("date('2024-01-01') - duration({days: 100000000})");
    assert_out_of_range("localdatetime('2024-01-01T00:00') + duration({days: 100000000})");
    assert_out_of_range("datetime('2024-01-01T00:00Z') + duration({days: 100000000})");
    assert_out_of_range("datetime('2024-01-01T00:00Z') + duration({seconds: 9223372036854775807})");
    // A month shift past the calendar range used to return the date unchanged.
    assert_out_of_range("date('2024-01-01') + duration('P300000Y')");
    assert_out_of_range("localdatetime('2024-01-01T00:00') + duration('P300000Y')");
    assert_out_of_range("datetime('2024-01-01T00:00Z') - duration('P300000Y')");
}

#[test]
fn e2_date_arithmetic_at_calendar_boundary() {
    // Lowest representable date: one day above it is fine, two is not.
    assert_eq!(
        tstr("date({year: -9999, month: 1, day: 2}) - duration({days: 1})"),
        s("-9999-01-01")
    );
    assert_out_of_range("date({year: -9999, month: 1, day: 2}) - duration({days: 2})");
    // Highest representable date, likewise.
    assert_eq!(
        tstr("date({year: 9999, month: 12, day: 30}) + duration({days: 1})"),
        s("9999-12-31")
    );
    assert_out_of_range("date({year: 9999, month: 12, day: 30}) + duration({days: 2})");
    // Ordinary arithmetic unchanged.
    assert_eq!(
        tstr("date('2024-01-31') + duration({months: 1})"),
        s("2024-02-29")
    );
    assert_eq!(
        tstr("date('2024-01-01') + duration({days: 1})"),
        s("2024-01-02")
    );
    assert_eq!(
        tstr("localtime('23:00') + duration({hours: 2})"),
        s("01:00")
    );
    // Time-of-day arithmetic wraps; a huge seconds component must not panic.
    assert_eq!(
        tstr("localtime('00:00') + duration({seconds: 9223372036854775807})"),
        s("15:30:07")
    );
}

#[test]
fn e2_day_of_quarter_overflow_errors() {
    let e = mem()
        .execute("RETURN date({year: 2024, quarter: 1, dayOfQuarter: 9223372036854775807}) AS v")
        .unwrap_err();
    match e {
        GraphError::Query(q) => {
            assert_eq!(q.code(), graphdblite::ErrorCode::InvalidArgumentValue);
            assert_eq!(q.message(), "temporal: dayOfQuarter out of range")
        }
        other => panic!("expected an InvalidArgumentValue error, got {other:?}"),
    }
    assert_eq!(
        tstr("date({year: 2024, quarter: 2, dayOfQuarter: 2})"),
        s("2024-04-02")
    );
}

#[test]
fn e2_duration_between_spans_over_292_years() {
    // 1700-01-01 .. 2024-01-01 is 118338 days; the old code overflowed i64
    // nanoseconds past ~292 years.
    assert_eq!(
        tstr("duration.inSeconds(date('1700-01-01'), date('2024-01-01'))"),
        s("PT2840112H")
    );
    assert_eq!(
        tstr("duration.inDays(date('1700-01-01'), date('2024-01-01'))"),
        s("P118338D")
    );
    assert_eq!(
        tstr("duration.inSeconds(date('2024-01-01'), date('1700-01-01'))"),
        s("PT-2840112H")
    );
}

#[test]
fn e2_from_epoch_full_range_and_overflow() {
    assert_eq!(
        tstr("datetime.fromepoch(253402300799, 0)"),
        s("9999-12-31T23:59:59Z")
    );
    assert_eq!(
        tstr("datetime.fromepochmillis(253402300799999)"),
        s("9999-12-31T23:59:59.999Z")
    );
    // Out of range used to panic (fromepoch) or silently give 1970.
    assert_out_of_range("datetime.fromepoch(9223372036854775807, 0)");
    assert_out_of_range("datetime.fromepochmillis(9223372036854775807)");
    assert_eq!(tstr("datetime.fromepoch(0, 0)"), s("1970-01-01T00:00Z"));
    assert_eq!(
        tstr("datetime.fromepoch(-1, 500000000)"),
        s("1969-12-31T23:59:59.5Z")
    );
}

#[test]
fn e2_duration_string_component_overflow_errors() {
    assert_out_of_range("duration('P9223372036854775807Y')");
    assert_out_of_range("duration('PT9223372036854775807H')");
    assert_out_of_range("duration('P9223372036854775807W')");
    // i64::MAX / 12 years still fits in months; one more year does not.
    assert_eq!(
        tstr("duration('P768614336404564650Y')"),
        s("P768614336404564650Y")
    );
    assert_out_of_range("duration('P768614336404564651Y')");
    // Exact integers (no f64 rounding) for large components.
    assert_eq!(
        tstr("duration('P9007199254740993D')"),
        s("P9007199254740993D")
    );
}

#[test]
fn e2_duration_addition_overflow_errors() {
    assert_out_of_range("duration({days: 9223372036854775807}) + duration({days: 1})");
    assert_out_of_range("duration({days: -9223372036854775807}) - duration({days: 2})");
    assert_out_of_range("duration({months: 9223372036854775807}) + duration({months: 1})");
    assert_out_of_range("duration({seconds: 9223372036854775807}) + duration({seconds: 1})");
    assert_out_of_range("duration({days: 1000000000000000000}) * 10");
    assert_out_of_range("-duration({days: -9223372036854775807 - 1})");
    // Normal arithmetic keeps working.
    assert_eq!(
        tstr("duration({days: 1, seconds: 1.5}) + duration({days: 2, seconds: 0.75})"),
        s("P3DT2.25S")
    );
    assert_eq!(tstr("duration({days: 2}) * 3"), s("P6D"));
    assert_eq!(
        tstr("duration({seconds: 1}) - duration({seconds: 1.5})"),
        s("PT-0.5S")
    );
}

#[test]
fn e2_time_map_components_out_of_range_error() {
    let e = mem()
        .execute("RETURN localtime({hour: 1, millisecond: 4295}) AS v")
        .unwrap_err();
    match e {
        GraphError::Query(q) => {
            assert_eq!(q.code(), graphdblite::ErrorCode::InvalidArgumentValue);
            assert_eq!(q.message(), "temporal: invalid time components")
        }
        other => panic!("expected an InvalidArgumentValue error, got {other:?}"),
    }
    assert_eq!(
        tstr("localtime({hour: 1, millisecond: 42})"),
        s("01:00:00.042")
    );
}

// ── E3: duration maps keep integer components exact ────────────────────────

#[test]
fn e3_duration_map_integer_components_are_exact() {
    assert_eq!(
        tstr("duration({seconds: 20000000, nanoseconds: 1})"),
        s("PT5555H33M20.000000001S")
    );
    assert_eq!(tstr("duration({hours: 3000000})"), s("PT3000000H"));
    assert_eq!(
        tstr("duration({hours: 2562047788015215})"),
        s("PT2562047788015215H")
    );
    assert_out_of_range("duration({hours: 2562047788015216})");
    // Fractional values still cascade.
    assert_eq!(tstr("duration({days: 1.5})"), s("P1DT12H"));
    assert_eq!(tstr("duration({seconds: 1.5})"), s("PT1.5S"));
    assert_eq!(tstr("duration({months: 0.5})"), s("P15DT5H14M33S"));
    assert_eq!(
        tstr("duration({minutes: 1.5, seconds: 1, nanoseconds: 2})"),
        s("PT1M31.000000002S")
    );
}

#[test]
fn e5_duration_map_rejects_unknown_keys() {
    assert_eval_err(
        "duration({dayz: 1})",
        "ArgumentError",
        ErrorCode::InvalidArgumentValue,
        "dayz",
    );
}

// ── E4: zoned datetime arithmetic re-resolves the DST offset ──────────────

#[test]
fn e4_zoned_datetime_plus_hours_crosses_dst_gap() {
    assert_eq!(
        tstr("datetime('2024-03-10T00:30[America/New_York]') + duration({hours: 3})"),
        s("2024-03-10T04:30-04:00[America/New_York]")
    );
    assert_eq!(
        tstr("datetime('2024-03-10T04:30[America/New_York]') - duration({hours: 3})"),
        s("2024-03-10T00:30-05:00[America/New_York]")
    );
    // Fall back: 00:30 EDT + 2h is 01:30 EST (the second 01:30).
    assert_eq!(
        tstr("datetime('2024-11-03T00:30[America/New_York]') + duration({hours: 2})"),
        s("2024-11-03T01:30-05:00[America/New_York]")
    );
    // Fixed offsets are unaffected.
    assert_eq!(
        tstr("datetime('2024-03-10T00:30-05:00') + duration({hours: 3})"),
        s("2024-03-10T03:30-05:00")
    );
}

#[test]
fn e4_zoned_datetime_plus_days_keeps_local_time() {
    // Days and months are calendar arithmetic on the local date-time.
    assert_eq!(
        tstr("datetime('2024-03-09T12:00[America/New_York]') + duration({days: 1})"),
        s("2024-03-10T12:00-04:00[America/New_York]")
    );
    assert_eq!(
        tstr("datetime('2024-02-10T12:00[America/New_York]') + duration({months: 1})"),
        s("2024-03-10T12:00-04:00[America/New_York]")
    );
    // A local time that falls in the spring-forward gap shifts forward by
    // the gap length (java.time semantics).
    assert_eq!(
        tstr("datetime('2024-03-09T02:30[America/New_York]') + duration({days: 1})"),
        s("2024-03-10T03:30-04:00[America/New_York]")
    );
}

// ── E5: temporal maps: epoch keys, unknown keys, missing year, narrowing ──

#[test]
fn e5_datetime_from_epoch_keys() {
    assert_eq!(
        tstr("datetime({epochSeconds: 1700000000})"),
        s("2023-11-14T22:13:20Z")
    );
    assert_eq!(
        tstr("datetime({epochMillis: 1700000000123})"),
        s("2023-11-14T22:13:20.123Z")
    );
    assert_eq!(
        tstr("datetime({epochSeconds: 1700000000, nanosecond: 5})"),
        s("2023-11-14T22:13:20.000000005Z")
    );
    assert_eq!(
        tstr("datetime({epochSeconds: 1700000000, timezone: '+01:00'})"),
        s("2023-11-14T23:13:20+01:00")
    );
    assert_eq!(
        tstr("datetime({epochSeconds: 1700000000, timezone: 'Europe/Stockholm'})"),
        s("2023-11-14T23:13:20+01:00[Europe/Stockholm]")
    );
    assert_out_of_range("datetime({epochSeconds: 9223372036854775807})");
    assert_eval_err(
        "datetime({epochSeconds: 1, year: 2024})",
        "ArgumentError",
        ErrorCode::InvalidArgumentValue,
        "epochSeconds",
    );
}

#[test]
fn e5_temporal_maps_reject_unknown_keys() {
    assert_eval_err(
        "date({yaer: 2024})",
        "ArgumentError",
        ErrorCode::InvalidArgumentValue,
        "yaer",
    );
    assert_eval_err(
        "localtime({hour: 1, minuet: 2})",
        "ArgumentError",
        ErrorCode::InvalidArgumentValue,
        "minuet",
    );
    // epoch keys are only meaningful for datetime().
    assert_eval_err(
        "date({epochSeconds: 1})",
        "ArgumentError",
        ErrorCode::InvalidArgumentValue,
        "epochSeconds",
    );
    // A timezone makes no sense on local types.
    assert_eval_err(
        "localdatetime({year: 2024, timezone: '+01:00'})",
        "ArgumentError",
        ErrorCode::InvalidArgumentValue,
        "timezone",
    );
}

#[test]
fn e5_temporal_maps_require_a_year() {
    assert_eval_err(
        "date({})",
        "ArgumentError",
        ErrorCode::InvalidArgumentValue,
        "year",
    );
    assert_eval_err(
        "date({month: 2, day: 3})",
        "ArgumentError",
        ErrorCode::InvalidArgumentValue,
        "year",
    );
    assert_eval_err(
        "localdatetime({hour: 12})",
        "ArgumentError",
        ErrorCode::InvalidArgumentValue,
        "year",
    );
    // A base date supplies the year.
    assert_eq!(
        tstr("date({date: date('2020-05-06'), day: 1})"),
        s("2020-05-01")
    );
    assert_eq!(tstr("date({year: 2024})"), s("2024-01-01"));
}

#[test]
fn e5_no_argument_constructors_return_the_current_instant() {
    let mut db = mem();
    // date() is today's UTC date: compare with the datetime() it derives from.
    assert_eq!(
        one(
            &mut db,
            "WITH datetime() AS now RETURN date() = date(now) OR date() > date(now) AS v"
        ),
        Value::Bool(true)
    );
    // One statement sees one clock reading.
    assert_eq!(
        one(
            &mut db,
            "UNWIND range(1, 50) AS i WITH collect(localdatetime()) AS ts \
             RETURN all(t IN ts WHERE t = ts[0]) AS v"
        ),
        Value::Bool(true)
    );
    assert_eq!(
        one(
            &mut db,
            "RETURN toString(duration.inSeconds(datetime(), datetime())) AS v"
        ),
        s("PT0S")
    );
    match one(&mut db, "RETURN date().year AS v") {
        Value::I64(y) => assert!(y >= 2026, "current year looks wrong: {y}"),
        other => panic!("expected an Integer year, got {other:?}"),
    }
}

#[test]
fn e5_temporal_map_narrowing_casts_error() {
    assert_eval_err(
        "date({year: 4294967297, month: 1, day: 1})",
        "ArgumentError",
        ErrorCode::InvalidArgumentValue,
        "year",
    );
    assert_eval_err(
        "date({year: 2024, ordinalDay: 4294967297})",
        "ArgumentError",
        ErrorCode::InvalidArgumentValue,
        "ordinalDay",
    );
    assert_eval_err(
        "date({year: 2024, month: 4294967297, day: 1})",
        "ArgumentError",
        ErrorCode::InvalidArgumentValue,
        "month",
    );
    assert_eval_err(
        "localtime({hour: 4294967297})",
        "ArgumentError",
        ErrorCode::InvalidArgumentValue,
        "hour",
    );
    assert_eval_err(
        "date({year: 2024, quarter: 9223372036854775807})",
        "ArgumentError",
        ErrorCode::InvalidArgumentValue,
        "quarter",
    );
    // In-range values still build.
    assert_eq!(tstr("date({year: 2024, ordinalDay: 60})"), s("2024-02-29"));
    assert_eq!(tstr("localtime({hour: 23, minute: 59})"), s("23:59"));
}

// ── E8 (D8): DISTINCT / grouping follow Cypher equivalence ────────────────

#[test]
fn e8_count_distinct_treats_integral_floats_as_integers() {
    assert_eq!(
        eval_rows("UNWIND [1, 1.0, 2, 2.5, -0.0, 0] AS x RETURN count(DISTINCT x) AS v"),
        vec![Value::I64(4)]
    );
    assert_eq!(
        eval_rows("UNWIND [[1], [1.0], [1, 2]] AS x RETURN count(DISTINCT x) AS v"),
        vec![Value::I64(2)]
    );
    assert_eq!(
        eval_rows("UNWIND [{a: 1}, {a: 1.0}, {a: 2}] AS x RETURN count(DISTINCT x) AS v"),
        vec![Value::I64(2)]
    );
    // collect(DISTINCT) keeps the first representative.
    assert_eq!(
        eval_rows("UNWIND [1, 1.0, 2.0, 2] AS x RETURN collect(DISTINCT x) AS v"),
        vec![Value::List(vec![Value::I64(1), Value::F64(2.0)])]
    );
}

#[test]
fn e8_distinct_rows_and_grouping_merge_int_and_float() {
    assert_eq!(
        eval_rows("UNWIND [1, 1.0, 2] AS x RETURN DISTINCT x AS v"),
        vec![Value::I64(1), Value::I64(2)]
    );
    assert_eq!(
        eval_rows("UNWIND [1, 1.0, 2] AS x WITH DISTINCT x RETURN x AS v"),
        vec![Value::I64(1), Value::I64(2)]
    );
    let rows = run(
        &mut mem(),
        "UNWIND [1, 1.0, 2] AS x RETURN x AS k, count(*) AS c ORDER BY c DESC",
    );
    let got: Vec<(Value, Value)> = rows
        .iter()
        .map(|r| (r.get("k").unwrap().clone(), r.get("c").unwrap().clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            (Value::I64(1), Value::I64(2)),
            (Value::I64(2), Value::I64(1))
        ]
    );
    assert_eq!(
        eval_rows("RETURN 1 AS v UNION RETURN 1.0 AS v"),
        vec![Value::I64(1)]
    );
    // Not every float merges: 2^53 + 1 is not equal to the float 2^53.
    assert_eq!(
        eval_rows(
            "UNWIND [9007199254740993, 9007199254740992.0] AS x RETURN count(DISTINCT x) AS v"
        ),
        vec![Value::I64(2)]
    );
}

// ── E10: exact Integer/Float comparison ────────────────────────────────────

#[test]
fn e10_int_float_comparison_is_exact_beyond_2_pow_53() {
    assert_eq!(
        eval("9007199254740993 = 9007199254740992.0"),
        Value::Bool(false)
    );
    assert_eq!(
        eval("9007199254740993 > 9007199254740992.0"),
        Value::Bool(true)
    );
    assert_eq!(
        eval("9007199254740992.0 < 9007199254740993"),
        Value::Bool(true)
    );
    assert_eq!(
        eval("9007199254740992 = 9007199254740992.0"),
        Value::Bool(true)
    );
    assert_eq!(eval("1 = 1.0"), Value::Bool(true));
    assert_eq!(eval("1 < 1.5"), Value::Bool(true));
    assert_eq!(eval("-1 > -1.5"), Value::Bool(true));
    // i64::MAX is below the float 2^63.
    assert_eq!(
        eval("9223372036854775807 < 9223372036854775808.0"),
        Value::Bool(true)
    );
    assert_eq!(
        eval("9223372036854775807 IN [9223372036854775808.0]"),
        Value::Bool(false)
    );
}

// ── E9 (D8): division by zero ──────────────────────────────────────────────

fn assert_div_by_zero(expr: &str) {
    assert_eval_err(
        expr,
        "ArithmeticError",
        ErrorCode::DivisionByZero,
        "by zero",
    );
}

#[test]
fn e9_integer_division_or_modulo_by_zero_errors() {
    assert_div_by_zero("1 / 0");
    assert_div_by_zero("0 / 0");
    assert_div_by_zero("-7 % 0");
    assert_div_by_zero("duration({days: 1}) / 0");
    assert_div_by_zero("duration({days: 1}) / 0.0");
    // Null still propagates before the zero check.
    assert_eq!(eval("null / 0"), Value::Null);
    assert_eq!(eval("7 / 2"), Value::I64(3));
    assert_eq!(eval("-7 % 3"), Value::I64(-1));
}

/// Evaluate `expr`, which must produce a Float, and return it.
fn float(expr: &str) -> f64 {
    match eval(expr) {
        Value::F64(f) => f,
        other => panic!("expected a Float from {expr}, got {other:?}"),
    }
}

#[test]
fn e9_float_division_by_zero_follows_ieee() {
    assert_eq!(float("1.0 / 0.0"), f64::INFINITY);
    assert_eq!(float("1.0 / 0"), f64::INFINITY);
    assert_eq!(float("1 / 0.0"), f64::INFINITY);
    assert_eq!(float("-1 / 0.0"), f64::NEG_INFINITY);
    assert!(float("0 / 0.0").is_nan());
    assert!(float("0.0 / 0").is_nan());
    assert!(float("1.5 % 0").is_nan());
    assert!(float("1 % 0.0").is_nan());
    assert!(float("1.0 % 0.0").is_nan());
    assert_eq!(float("7.5 % 2"), 1.5);
}

// ── E10: unary minus on a duration ─────────────────────────────────────────

#[test]
fn e10_unary_minus_negates_durations() {
    assert_eq!(tstr("-duration({days: 1})"), s("P-1D"));
    assert_eq!(
        tstr("-duration({months: 1, seconds: -1.5})"),
        s("P-1MT1.5S")
    );
    assert_eq!(eval("-(1)"), Value::I64(-1));
}

// ── E11: aggregates ────────────────────────────────────────────────────────

#[test]
fn e11_integer_sum_overflow_errors() {
    assert_eq!(
        query_err(
            &mut mem(),
            "UNWIND [9223372036854775807, 1] AS x RETURN sum(x) AS v"
        )
        .code(),
        ErrorCode::NumberOutOfRange
    );
    assert_eq!(
        query_err(
            &mut mem(),
            "UNWIND [-9223372036854775807, -2] AS x RETURN sum(x) AS v"
        )
        .code(),
        ErrorCode::NumberOutOfRange
    );
    // Boundary: exactly i64::MAX is fine.
    assert_eq!(
        eval_rows("UNWIND [9223372036854775806, 1] AS x RETURN sum(x) AS v"),
        vec![Value::I64(i64::MAX)]
    );
    // A Float makes the sum a Float; no Integer overflow applies.
    assert_eq!(
        eval_rows("UNWIND [9223372036854775807, 1, 0.5] AS x RETURN sum(x) AS v"),
        vec![Value::F64(9223372036854775807.0 + 1.5)]
    );
}

#[test]
fn e11_percentile_disc_returns_the_element_type() {
    assert_eq!(
        eval_rows("UNWIND [1, 2, 3] AS x RETURN percentileDisc(x, 0.5) AS v"),
        vec![Value::I64(2)]
    );
    assert_eq!(
        eval_rows("UNWIND [3, 1, 2] AS x RETURN percentileDisc(x, 1.0) AS v"),
        vec![Value::I64(3)]
    );
    assert_eq!(
        eval_rows("UNWIND [1.5, 2.5] AS x RETURN percentileDisc(x, 0.0) AS v"),
        vec![Value::F64(1.5)]
    );
    // percentileCont interpolates, so it stays a Float.
    assert_eq!(
        eval_rows("UNWIND [1, 2] AS x RETURN percentileCont(x, 0.5) AS v"),
        vec![Value::F64(1.5)]
    );
}

#[test]
fn e11_group_key_errors_propagate() {
    let e = query_err(
        &mut mem(),
        "UNWIND [1, 0] AS x RETURN 1 / x AS k, count(*) AS c",
    );
    assert_eq!(e.code(), ErrorCode::DivisionByZero, "{e}");
    let e = query_err(
        &mut mem(),
        "UNWIND [1, 0] AS x WITH 1 / x AS k, count(*) AS c RETURN k",
    );
    assert_eq!(e.code(), ErrorCode::DivisionByZero, "{e}");
}

#[test]
fn e11_sort_key_errors_propagate() {
    let e = query_err(&mut mem(), "UNWIND [1, 0] AS x RETURN x ORDER BY 1 / x");
    assert_eq!(e.code(), ErrorCode::DivisionByZero, "{e}");
    let e = query_err(
        &mut mem(),
        "UNWIND [1, 0] AS x WITH x ORDER BY 1 / x RETURN x",
    );
    assert_eq!(e.code(), ErrorCode::DivisionByZero, "{e}");
    let mut db = mem();
    run(&mut db, "CREATE (:N {v: 1}), (:N {v: 0})");
    let e = query_err(&mut db, "MATCH (n:N) RETURN n.v ORDER BY 1 / n.v");
    assert_eq!(e.code(), ErrorCode::DivisionByZero, "{e}");
}

// ── E12: aggregates inside index / slice / property / CASE expressions ────

#[test]
fn e12_aggregate_inside_subscript_is_planned_as_aggregate() {
    assert_eq!(
        eval_rows("UNWIND [1, 2, 3] AS x RETURN collect(x)[0] AS v"),
        vec![Value::I64(1)]
    );
    assert_eq!(
        eval_rows("UNWIND [1, 2, 3] AS x WITH collect(x)[2] AS v RETURN v"),
        vec![Value::I64(3)]
    );
    assert_eq!(
        eval_rows("UNWIND [1, 2, 3] AS x RETURN collect(x)[1..] AS v"),
        vec![Value::List(vec![Value::I64(2), Value::I64(3)])]
    );
    assert_eq!(
        eval_rows("UNWIND [{a: 1}, {a: 2}] AS m RETURN collect(m)[1].a AS v"),
        vec![Value::I64(2)]
    );
    assert_eq!(
        eval_rows(
            "UNWIND [1, 2] AS x RETURN CASE WHEN count(x) > 1 THEN 'many' ELSE 'one' END AS v"
        ),
        vec![s("many")]
    );
    let mut db = mem();
    run(
        &mut db,
        "CREATE (:A)-[:R {w: 1}]->(:B), (:A)-[:R {w: 2}]->(:B)",
    );
    assert_eq!(
        one(
            &mut db,
            "MATCH ()-[r:R]->() WITH r ORDER BY r.w WITH collect(r)[0] AS x RETURN x.w AS v"
        ),
        Value::I64(1)
    );
}

#[test]
fn e12_grouped_subscript_aggregate_and_ambiguity() {
    assert_eq!(
        eval_rows("UNWIND [1, 1, 2] AS x RETURN x AS k, collect(x)[0] AS v ORDER BY k"),
        vec![Value::I64(1), Value::I64(2)]
    );
    let e = query_err(&mut mem(), "UNWIND [[1]] AS l RETURN l[0] + count(*) AS v");
    assert_eq!(e.code(), ErrorCode::AmbiguousAggregationExpression, "{e}");
    // ...but fine when the subscript itself is a grouping key.
    assert_eq!(
        eval_rows("UNWIND [[1], [1]] AS l RETURN l[0] AS k, l[0] + count(*) AS v"),
        vec![Value::I64(1)]
    );
}

// ── E13: UNION inside EXISTS { } ───────────────────────────────────────────

#[test]
fn e13_union_inside_exists_subquery() {
    let mut db = mem();
    run(
        &mut db,
        "CREATE (:P {name: 'a'})-[:X]->(:Q), (:P {name: 'b'})-[:Y]->(:Q), (:P {name: 'c'})",
    );
    let rows = run(
        &mut db,
        "MATCH (p:P) WHERE EXISTS { MATCH (p)-[:X]->() UNION MATCH (p)-[:Y]->() } \
         RETURN p.name AS name ORDER BY name",
    );
    let names: Vec<Value> = rows
        .iter()
        .map(|r| r.get("name").unwrap().clone())
        .collect();
    assert_eq!(names, vec![s("a"), s("b")]);
    let rows = run(
        &mut db,
        "MATCH (p:P) WHERE EXISTS { MATCH (p)-[:X]->() RETURN 1 AS x \
         UNION ALL MATCH (p)-[:Y]->() RETURN 1 AS x } \
         RETURN p.name AS name ORDER BY name",
    );
    let names: Vec<Value> = rows
        .iter()
        .map(|r| r.get("name").unwrap().clone())
        .collect();
    assert_eq!(names, vec![s("a"), s("b")]);
    // Branches must agree on their columns, as for a top-level UNION.
    let e = query_err(
        &mut db,
        "MATCH (p:P) WHERE EXISTS { MATCH (p)-[:X]->() RETURN 1 AS x \
         UNION MATCH (p)-[:Y]->() RETURN 1 AS y } RETURN p",
    );
    assert_eq!(e.code(), ErrorCode::DifferentColumnsInUnion, "{e}");
}

// ── E14: parsing is linear in the query length ─────────────────────────────

#[test]
fn e14_long_list_literal_parses_quickly() {
    // Each span used to recompute its line/column by rescanning the input
    // from the start: quadratic, ~21 s for 40k elements in release and many
    // minutes in debug. Linear parsing takes a few seconds here even in
    // debug, so the bound is generous.
    let n = 50_000;
    let q = format!("RETURN size([{}]) AS v", vec!["1"; n].join(","));
    let start = std::time::Instant::now();
    assert_eq!(one(&mut mem(), &q), Value::I64(n as i64));
    let elapsed = start.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "parsing a {n}-element list took {elapsed:?}"
    );
}

#[test]
fn e14_error_positions_survive_the_line_index() {
    // Line/column still point at the offending token, with multi-byte
    // characters counted as one column each.
    let e = query_err(&mut mem(), "RETURN 'héllo' AS a,\n  nope AS b");
    assert_eq!(e.code(), ErrorCode::UndefinedVariable, "{e}");
    let span = e.span().expect("semantic errors carry the AST span");
    assert_eq!((span.line, span.col), (2, 3), "{e}");
    let e = query_err(&mut mem(), "WITH 'é' AS a RETURN a, nope");
    let span = e.span().expect("semantic errors carry the AST span");
    assert_eq!((span.line, span.col), (1, 25), "{e}");
}

// ── E15: SizeLimit messages name the right unit ────────────────────────────

#[test]
fn e15_size_limit_message_units() {
    // A depth limit is not measured in bytes.
    let q = format!("RETURN {}1{}", "(".repeat(300), ")".repeat(300));
    let e = mem().execute(&q).unwrap_err();
    assert!(matches!(e, GraphError::SizeLimit { .. }), "{e:?}");
    assert_eq!(
        e.to_string(),
        "bracket nesting depth (257) exceeds maximum of 256\n  hint: simplify the query or split it into multiple statements"
    );
    // Byte limits say so.
    let big = format!("RETURN '{}'", "x".repeat(1 << 20));
    let e = mem().execute(&big).unwrap_err();
    assert!(
        e.to_string()
            .starts_with("Cypher query size in bytes (1048585) exceeds maximum of 1048576"),
        "{e}"
    );
}

// ── E10: a regex is validated on its own before anchoring ─────────────────

#[test]
fn e10_unbalanced_regex_is_rejected() {
    // Anchored naively as `^(?:a)|(b)$` this compiled and matched 'xb'.
    assert_eval_err(
        "'xb' =~ 'a)|(b'",
        "SemanticError",
        ErrorCode::InvalidArgumentValue,
        "invalid regex pattern `a)|(b`",
    );
    assert_eq!(eval("'ab' =~ 'a|ab'"), Value::Bool(true));
    assert_eq!(eval("'xb' =~ 'a|b'"), Value::Bool(false));
}
