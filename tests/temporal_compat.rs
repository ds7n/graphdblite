//! Temporal compatibility golden (safety net for the chrono -> jiff migration).
//!
//! Three sections, all compared against `tests/temporal_compat_golden.txt`:
//!
//! - `[wire]`: exact on-disk bytes of every temporal type, both in the node
//!   property blob and in a secondary-index key. Any change here breaks
//!   existing database files (schema version 2).
//! - `[readback]`: values read back from `tests/fixtures/temporal_v0_2.db`, a
//!   file written by the chrono-based v0.2 build and never regenerated. Proves
//!   old files still decode and display identically.
//! - `[semantics]`: query results for calendar/timezone edge cases (month-end
//!   clamping, ISO weeks, DST gaps and overlaps, epoch conversion) that a new
//!   date library must reproduce exactly.
//!
//! `BLESS=1` rewrites the golden. `GEN_FIXTURE=1` rewrites the fixture db; do
//! that only on a build whose wire format is known-good, never to "fix" a
//! failing readback.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use graphdblite::Database;

/// One stored temporal value per case: (key, Cypher expression).
const STORED: &[(&str, &str)] = &[
    ("date_basic", "date('1984-10-11')"),
    ("date_year1", "date('0001-01-01')"),
    ("date_year9999", "date('9999-12-31')"),
    ("date_negative", "date({year: -44, month: 3, day: 15})"),
    ("localtime_basic", "localtime('12:31:14.645876123')"),
    ("localtime_midnight", "localtime('00:00')"),
    ("time_pos_offset", "time('12:31:14.645876+01:00')"),
    ("time_neg_offset", "time('23:59:59.999999999-09:30')"),
    ("time_utc", "time('00:00Z')"),
    (
        "localdatetime_basic",
        "localdatetime('2015-07-21T21:40:32.142')",
    ),
    (
        "datetime_offset",
        "datetime('2015-07-21T21:40:32.142+01:00')",
    ),
    (
        "datetime_neg_offset",
        "datetime('1969-12-31T23:59:59-05:45')",
    ),
    (
        "datetime_tzname",
        "datetime('2015-07-21T21:40:32.142[Europe/Stockholm]')",
    ),
    (
        "datetime_tzname_winter",
        "datetime('2015-01-21T21:40:32[America/New_York]')",
    ),
    ("duration_full", "duration('P1Y2M3DT4H5M6.789S')"),
    ("duration_negative", "duration({days: -3, seconds: -1})"),
];

/// Values the chrono build accepted but newer builds reject at construction
/// (years outside -9999..=9999, leap-second nanoseconds). They exist only in
/// the frozen fixture, which must keep decoding and displaying them.
const LEGACY_ONLY: &[(&str, &str)] = &[
    ("date_year20000", "date({year: 20000, month: 6, day: 1})"),
    (
        "localtime_leapnanos",
        "localtime({hour: 23, minute: 59, second: 59, nanosecond: 1500000000})",
    ),
    (
        "localdatetime_year20000",
        "localdatetime({year: 20000, month: 1, day: 2, hour: 3})",
    ),
];

/// Edge-case queries whose results (or error messages) must not change.
const SEMANTICS: &[(&str, &str)] = &[
    // Month arithmetic clamps to month end.
    ("jan31_plus_1m", "date('2023-01-31') + duration('P1M')"),
    ("jan31_plus_1m_leap", "date('2024-01-31') + duration('P1M')"),
    ("feb29_plus_1y", "date('2024-02-29') + duration('P1Y')"),
    ("mar31_minus_1m", "date('2024-03-31') - duration('P1M')"),
    // ISO week dates and components.
    ("isoweek_53", "date('2020-W53-7')"),
    ("isoweek_jan1", "date('2021-01-01').week"),
    ("isoweekyear_jan1", "date('2021-01-01').weekYear"),
    ("isoweek_dec31", "date('2024-12-31').week"),
    ("weekday_comp", "date('2024-06-02').dayOfWeek"),
    ("ordinal", "date('2024-366')"),
    ("ordinal_comp", "date('2024-12-31').ordinalDay"),
    ("quarter_comp", "date('2024-08-15').quarter"),
    ("week_date_map", "date({year: 2024, week: 1, dayOfWeek: 1})"),
    // Truncation.
    ("trunc_week", "date.truncate('week', date('2024-01-03'))"),
    ("trunc_quarter", "date.truncate('quarter', date('2024-08-15'))"),
    (
        "trunc_dt_tz",
        "datetime.truncate('day', datetime('2024-03-31T12:00[Europe/Stockholm]'))",
    ),
    (
        "trunc_dt_tz_gap",
        "datetime.truncate('hour', localdatetime('2024-03-31T02:30'), {timezone: 'Europe/Stockholm'})",
    ),
    // Named-timezone construction in a DST gap / overlap.
    ("tz_gap_parse", "datetime('2024-03-31T02:30[Europe/Stockholm]')"),
    (
        "tz_gap_map",
        "datetime({year: 2024, month: 3, day: 31, hour: 2, minute: 30, timezone: 'Europe/Stockholm'})",
    ),
    ("tz_overlap_parse", "datetime('2024-10-27T02:30[Europe/Stockholm]')"),
    (
        "tz_overlap_map",
        "datetime({year: 2024, month: 10, day: 27, hour: 2, minute: 30, timezone: 'Europe/Stockholm'})",
    ),
    // Zoned arithmetic landing in a gap / overlap (java.time semantics).
    (
        "zoned_plus_into_gap",
        "datetime('2024-03-30T02:30[Europe/Stockholm]') + duration('P1D')",
    ),
    (
        "zoned_plus_into_overlap",
        "datetime('2024-10-26T02:30[Europe/Stockholm]') + duration('P1D')",
    ),
    (
        "zoned_plus_hours_across_gap",
        "datetime('2024-03-31T01:30[Europe/Stockholm]') + duration('PT1H')",
    ),
    // DST-aware duration functions mixing local and zoned values.
    (
        "between_local_zoned_gap",
        "duration.between(localdatetime('2024-03-31T01:00'), datetime('2024-03-31T04:00[Europe/Stockholm]'))",
    ),
    (
        "inseconds_local_zoned",
        "duration.inSeconds(localdatetime('2024-10-27T01:00'), datetime('2024-10-27T04:00[Europe/Stockholm]'))",
    ),
    (
        "indays_across_dst",
        "duration.inDays(datetime('2024-03-30T12:00[Europe/Stockholm]'), datetime('2024-04-01T11:00[Europe/Stockholm]'))",
    ),
    (
        "inmonths_month_end",
        "duration.inMonths(date('2024-01-31'), date('2024-02-29'))",
    ),
    (
        "between_large",
        "duration.between(date('0001-01-01'), date('9999-12-31'))",
    ),
    // Epoch conversion and components.
    (
        "fromepoch",
        "datetime({epochSeconds: 1700000000, nanosecond: 123})",
    ),
    ("fromepochmillis", "datetime({epochMillis: -1})"),
    (
        "epoch_components",
        "[datetime('2024-01-01T00:00:00.5Z').epochSeconds, datetime('2024-01-01T00:00:00.5Z').epochMillis]",
    ),
    (
        "tz_components",
        "[datetime('2024-07-01T12:00[Europe/Stockholm]').timezone, datetime('2024-07-01T12:00[Europe/Stockholm]').offsetSeconds]",
    ),
    // Conversions between types.
    (
        "dt_to_tz",
        "datetime({datetime: datetime('2024-07-01T12:00Z'), timezone: 'Asia/Kolkata'})",
    ),
    (
        "time_from_dt",
        "time(datetime('2024-07-01T12:00[America/New_York]'))",
    ),
    // Range boundaries (behaviour changes here are deliberate and reviewed).
    ("year_10000", "date('+10000-01-01')"),
    ("year_minus_10000", "date({year: -10000, month: 1, day: 1})"),
    ("leap_second_60", "localtime('23:59:60')"),
    ("bad_tz", "datetime('2024-01-01T00:00[Not/AZone]')"),
];

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture_path() -> PathBuf {
    manifest().join("tests/fixtures/temporal_v0_2.db")
}

/// Write `cases` into a fresh database at `path`, with a btree index on the
/// property so index keys are exercised too.
fn populate(path: &Path, cases: &[(&str, &str)]) {
    let mut db = Database::open(path).expect("open");
    db.execute("CREATE INDEX ON :T(v)").expect("create index");
    for (key, expr) in cases {
        // One property per node: multi-property maps serialize in HashMap
        // order, so only single-property blobs are byte-deterministic.
        db.execute(&format!("CREATE (:T {{v: {expr}}})"))
            .unwrap_or_else(|e| panic!("create {key}: {e}"));
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Raw node blobs and index keys, read straight from SQLite.
fn wire_section(path: &Path) -> String {
    let conn = rusqlite::Connection::open(path).expect("raw open");
    let mut out = String::new();
    let mut stmt = conn
        .prepare("SELECT key, value FROM nodes ORDER BY key")
        .unwrap();
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
        })
        .unwrap();
    for row in rows {
        let (k, v) = row.unwrap();
        writeln!(out, "node {} {}", hex(&k), hex(&v)).unwrap();
    }
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'node_idx$%' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(!tables.is_empty(), "expected a btree index table");
    for t in tables {
        let q = format!("SELECT * FROM \"{t}\" ORDER BY 1");
        let mut stmt = conn.prepare(&q).unwrap();
        let ncols = stmt.column_count();
        let rows = stmt
            .query_map([], |r| {
                (0..ncols)
                    .map(|i| {
                        Ok(match r.get_ref(i)? {
                            rusqlite::types::ValueRef::Blob(b) => hex(b),
                            rusqlite::types::ValueRef::Integer(n) => n.to_string(),
                            other => format!("{other:?}"),
                        })
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap();
        for row in rows {
            writeln!(out, "index {}", row.unwrap().join(" ")).unwrap();
        }
    }
    out
}

/// Every stored value read back through the query engine.
fn readback_section(path: &Path) -> String {
    let mut db = Database::open(path).expect("open fixture copy");
    let rows = db
        .execute("MATCH (n:T) RETURN n.v AS v ORDER BY id(n)")
        .expect("readback query");
    let cases: Vec<_> = STORED.iter().chain(LEGACY_ONLY).collect();
    assert_eq!(rows.len(), cases.len(), "fixture row count");
    let mut out = String::new();
    // Node ids follow insertion order: `STORED` then `LEGACY_ONLY`.
    for ((key, _), r) in cases.into_iter().zip(&rows) {
        writeln!(out, "{key} = {}", r.get("v").expect("v")).unwrap();
    }
    out
}

fn semantics_section() -> String {
    let mut db = Database::open_memory().expect("memory db");
    let mut out = String::new();
    for (name, expr) in SEMANTICS {
        let line = match db.execute(&format!("RETURN {expr} AS v")) {
            Ok(rows) => format!("{}", rows[0].get("v").expect("v")),
            Err(e) => format!("ERROR {e}"),
        };
        writeln!(out, "{name} = {line}").unwrap();
    }
    out
}

#[test]
fn temporal_compat_golden() {
    let dir = tempfile::tempdir().unwrap();

    let fresh = dir.path().join("fresh.db");
    populate(&fresh, STORED);
    let wire = wire_section(&fresh);

    if std::env::var_os("GEN_FIXTURE").is_some() {
        let all: Vec<_> = STORED.iter().chain(LEGACY_ONLY).copied().collect();
        let gen = dir.path().join("gen.db");
        populate(&gen, &all);
        std::fs::create_dir_all(fixture_path().parent().unwrap()).unwrap();
        // Checkpoint the WAL into the main file so the fixture is one file.
        let conn = rusqlite::Connection::open(&gen).unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
            .unwrap();
        drop(conn);
        std::fs::copy(&gen, fixture_path()).unwrap();
    }

    // Read from a copy: opening may write (WAL switch), and the committed
    // fixture must stay byte-identical.
    let copy = dir.path().join("fixture_copy.db");
    std::fs::copy(fixture_path(), &copy).expect("fixture missing: run with GEN_FIXTURE=1 once");
    let readback = readback_section(&copy);

    let actual = format!(
        "[wire]\n{wire}\n[readback]\n{readback}\n[semantics]\n{}",
        semantics_section()
    );
    let golden = manifest().join("tests/temporal_compat_golden.txt");
    if std::env::var_os("BLESS").is_some() {
        std::fs::write(&golden, &actual).unwrap();
        return;
    }
    // Normalize line endings: a Windows checkout may convert the golden to
    // CRLF (see .gitattributes, which pins it to LF).
    let expected = std::fs::read_to_string(&golden)
        .expect("golden missing: run with BLESS=1")
        .replace("\r\n", "\n");
    if actual != expected {
        let diff: Vec<String> = expected
            .lines()
            .zip(actual.lines())
            .filter(|(e, a)| e != a)
            .take(20)
            .map(|(e, a)| format!("- {e}\n+ {a}"))
            .collect();
        panic!(
            "temporal compat golden mismatch ({} vs {} lines):\n{}",
            expected.lines().count(),
            actual.lines().count(),
            diff.join("\n")
        );
    }
}
