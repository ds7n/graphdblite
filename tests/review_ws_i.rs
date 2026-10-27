//! WS-I review findings: bounded memory for queries (R2, R3) and the
//! foreign-file check at open (R4). Each test fails on the pre-fix code.
//!
//! R2/R3 behavior is asserted through the configured limits
//! (`max_result_rows`, `max_materialized_bytes`) using small values so the
//! tests are fast; the peak-memory claim itself is measured under dhat in
//! `tests/memory_peak.rs` (`--features dhat-heap`).

use graphdblite::{Config, Database, GraphError, Record, Value};

const ROW_CAP_MSG: &str = "result set exceeded maximum of";
const BUDGET_WHAT: &str = "materialized query data in bytes";

fn open(max_rows: usize, budget_bytes: u64) -> Database {
    Database::open_with_config(
        ":memory:",
        Config {
            max_result_rows: max_rows,
            max_materialized_bytes: budget_bytes,
            ..Config::default()
        },
    )
    .unwrap()
}

fn with_nodes(db: &mut Database, n: i64) {
    db.execute(&format!("UNWIND range(1,{n}) AS i CREATE (:N {{id:i}})"))
        .unwrap();
}

fn run(db: &mut Database, q: &str) -> Vec<Record> {
    db.execute(q)
        .unwrap_or_else(|e| panic!("query failed: {e}\n{q}"))
}

fn err(db: &mut Database, q: &str) -> GraphError {
    match db.execute(q) {
        Ok(rows) => panic!("expected an error from {q}, got {} rows", rows.len()),
        Err(e) => e,
    }
}

fn assert_row_cap(e: &GraphError, cap: usize) {
    let msg = e.to_string();
    assert!(
        msg.contains(&format!("{ROW_CAP_MSG} {cap} rows")),
        "expected the {cap}-row cap error, got: {msg}"
    );
}

fn assert_budget_error(e: &GraphError, budget: u64) {
    match e {
        GraphError::SizeLimit { what, limit, .. } => {
            assert_eq!(what, BUDGET_WHAT);
            assert_eq!(*limit as u64, budget);
        }
        other => panic!("expected the materialization SizeLimit, got {other:?}"),
    }
}

fn first_int(rows: &[Record]) -> i64 {
    match rows[0].values().next() {
        Some(Value::I64(n)) => *n,
        other => panic!("expected an integer, got {other:?}"),
    }
}

const MIB: u64 = 1 << 20;

// ---------------------------------------------------------------- R2: rows

#[test]
fn r2_unwind_row_cap_boundary_on_the_named_path() {
    // `RETURN x` of a bare variable runs the named executor.
    let mut db = open(100, 0);
    assert_eq!(run(&mut db, "UNWIND range(1,100) AS x RETURN x").len(), 100);
    assert_row_cap(&err(&mut db, "UNWIND range(1,101) AS x RETURN x"), 100);
}

#[test]
fn r2_unwind_row_cap_boundary_on_the_slot_path() {
    // A non-variable projection runs on the slot path.
    let mut db = open(100, 0);
    assert_eq!(
        run(&mut db, "UNWIND range(1,100) AS x RETURN x + 0 AS y").len(),
        100
    );
    assert_row_cap(
        &err(&mut db, "UNWIND range(1,101) AS x RETURN x + 0 AS y"),
        100,
    );
}

#[test]
fn r2_unwind_over_matched_rows_hits_the_cap_not_memory() {
    let mut db = open(100, 0);
    with_nodes(&mut db, 4);
    // 4 x 50 = 200 rows > 100, and exactly 100 when the list has 25.
    assert_row_cap(
        &err(&mut db, "MATCH (n:N) UNWIND range(1,50) AS x RETURN x"),
        100,
    );
    assert_eq!(
        run(&mut db, "MATCH (n:N) UNWIND range(1,25) AS x RETURN x").len(),
        100
    );
}

#[test]
fn r2_unwind_output_is_charged_to_the_materialization_budget() {
    // Rows are buffered by the named executor; with the row cap off, only
    // the byte budget stops a runaway UNWIND, before the rows exist.
    let mut db = open(0, MIB);
    with_nodes(&mut db, 3);
    let e = err(&mut db, "MATCH (n:N) UNWIND range(1,20000) AS x RETURN x");
    assert_budget_error(&e, MIB);
    // Below the budget the same shape succeeds with the exact row count.
    assert_eq!(
        run(&mut db, "MATCH (n:N) UNWIND range(1,100) AS x RETURN x").len(),
        300
    );
}

#[test]
fn r2_count_over_unwind_streams_past_the_row_cap_and_the_budget() {
    // count(*) needs O(groups) memory: 3 x 20000 = 60000 input rows exceed
    // both the 100-row cap and the 1 MiB budget without error.
    let mut db = open(100, MIB);
    with_nodes(&mut db, 3);
    let rows = run(
        &mut db,
        "MATCH (n:N) UNWIND range(1,20000) AS x RETURN count(*)",
    );
    assert_eq!(first_int(&rows), 60_000);
}

#[test]
fn r2_streaming_aggregates_match_the_materialized_results() {
    let mut db = open(100, MIB);
    with_nodes(&mut db, 2);
    let mut rows = run(
        &mut db,
        "UNWIND range(1,10000) AS x RETURN x % 2 AS g, count(*) AS c, sum(x) AS s, \
         avg(x) AS a, min(x) AS lo, max(x) AS hi, count(DISTINCT x % 7) AS d",
    );
    assert_eq!(rows.len(), 2);
    rows.sort_by_key(|r| match r.get("g") {
        Some(Value::I64(n)) => *n,
        other => panic!("expected an integer group, got {other:?}"),
    });
    let get = |r: &Record, k: &str| r.get(k).cloned().unwrap();
    assert_eq!(get(&rows[0], "g"), Value::I64(0));
    assert_eq!(get(&rows[0], "c"), Value::I64(5000));
    assert_eq!(get(&rows[0], "s"), Value::I64(25_005_000));
    assert_eq!(get(&rows[0], "lo"), Value::I64(2));
    assert_eq!(get(&rows[0], "hi"), Value::I64(10_000));
    assert_eq!(get(&rows[0], "d"), Value::I64(7));
    assert_eq!(get(&rows[1], "g"), Value::I64(1));
    assert_eq!(get(&rows[1], "s"), Value::I64(25_000_000));
    assert_eq!(get(&rows[1], "a"), Value::F64(5000.0));
}

#[test]
fn r2_aggregate_over_empty_input_still_yields_one_row() {
    let mut db = open(100, MIB);
    with_nodes(&mut db, 1);
    let rows = run(
        &mut db,
        "MATCH (n:Missing) RETURN count(*), sum(n.id), collect(n.id)",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("count(*)"), Some(&Value::I64(0)));
    assert_eq!(rows[0].get("sum(n.id)"), Some(&Value::I64(0)));
    assert_eq!(rows[0].get("collect(n.id)"), Some(&Value::List(vec![])));
    let grouped = run(&mut db, "MATCH (n:Missing) RETURN n.id, count(*)");
    assert!(grouped.is_empty());
}

#[test]
fn r2_group_state_is_charged_to_the_budget() {
    // One group per input row: the group table is real memory.
    let mut db = open(0, MIB);
    let e = err(
        &mut db,
        "UNWIND range(1,100000) AS x RETURN x AS g, count(*) AS c",
    );
    assert_budget_error(&e, MIB);
}

#[test]
fn r2_collect_is_charged_to_the_budget() {
    let mut db = open(0, MIB);
    let e = err(&mut db, "UNWIND range(1,100000) AS x RETURN collect(x)");
    assert_budget_error(&e, MIB);
    let rows = run(&mut db, "UNWIND range(1,100) AS x RETURN size(collect(x))");
    assert_eq!(first_int(&rows), 100);
}

#[test]
fn r2_sort_and_distinct_buffers_are_charged_to_the_budget() {
    let mut db = open(0, MIB);
    let e = err(
        &mut db,
        "UNWIND range(1,100000) AS x RETURN x + 0 AS y ORDER BY y DESC",
    );
    assert_budget_error(&e, MIB);
    let e = err(
        &mut db,
        "UNWIND range(1,100000) AS x RETURN DISTINCT x + 0 AS y",
    );
    assert_budget_error(&e, MIB);
}

#[test]
fn r2_cartesian_product_stops_at_the_row_cap() {
    let mut db = open(1000, 0);
    with_nodes(&mut db, 40);
    assert_row_cap(
        &err(&mut db, "MATCH (a:N),(b:N),(c:N),(d:N) RETURN a.id"),
        1000,
    );
    // 10 x 10 x 10 = 1000 rows is exactly at the cap.
    let mut db = open(1000, 0);
    with_nodes(&mut db, 10);
    assert_eq!(
        run(&mut db, "MATCH (a:N),(b:N),(c:N) RETURN a.id").len(),
        1000
    );
}

#[test]
fn r2_wide_rows_are_bounded_by_bytes_not_row_count() {
    // 17 chained MATCH clauses over 2 nodes make 2^17 = 131072 rows of 17
    // bound nodes each. The row cap alone (1M here) lets the rows grow
    // until the process runs out of memory; the byte budget stops it.
    let mut db = open(1_000_000, 8 * MIB);
    with_nodes(&mut db, 2);
    let mut q = String::from("MATCH (n1:N)");
    for i in 2..=17 {
        q.push_str(&format!(" MATCH (n{i}:N)"));
    }
    q.push_str(" RETURN n1.id");
    let e = err(&mut db, &q);
    assert_budget_error(&e, 8 * MIB);
}

// ------------------------------------------------------------ R3: lists

#[test]
fn r3_lists_held_by_buffered_rows_count_against_one_query_budget() {
    // Each 50000-element list is ~4.8 MB. Eight of them held by the
    // buffered WITH rows exceed a 20 MiB budget even though each list is
    // far below the per-list cap.
    let mut db = open(0, 20 * MIB);
    let e = err(
        &mut db,
        "UNWIND range(1,8) AS i WITH i, range(1,50000) AS l RETURN i, size(l)",
    );
    assert_budget_error(&e, 20 * MIB);
    // Three of them fit.
    let rows = run(
        &mut db,
        "UNWIND range(1,3) AS i WITH i, range(1,50000) AS l RETURN i, size(l)",
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[2].get("size(l)"), Some(&Value::I64(50_000)));
}

#[test]
fn r3_range_is_checked_against_the_budget_before_allocating() {
    let mut db = open(0, MIB);
    let e = err(&mut db, "RETURN size(range(1,2000000)) AS n");
    assert_budget_error(&e, MIB);
    // 5000 values * 96 bytes = 480 KB fits.
    let rows = run(&mut db, "RETURN size(range(1,5000)) AS n");
    assert_eq!(first_int(&rows), 5000);
}

#[test]
fn r3_streamed_lists_are_not_double_counted() {
    // Each row's list is dropped before the next is built, so 200 rows of a
    // 5000-element list (~480 KB each) pass a 1 MiB budget on the slot path.
    let mut db = open(0, MIB);
    let rows = run(
        &mut db,
        "UNWIND range(1,200) AS i RETURN sum(size(range(1,5000))) AS n",
    );
    assert_eq!(first_int(&rows), 1_000_000);
}

#[test]
fn zero_budget_means_unlimited() {
    let mut db = open(0, 0);
    let rows = run(&mut db, "UNWIND range(1,50000) AS x RETURN collect(x) AS l");
    match rows[0].get("l") {
        Some(Value::List(items)) => assert_eq!(items.len(), 50_000),
        other => panic!("expected a list, got {other:?}"),
    }
}

// ------------------------------------------------- R4: lookalike tables

fn open_err(path: &std::path::Path) -> GraphError {
    match Database::open(path) {
        Ok(_) => panic!("expected {} to be refused", path.display()),
        Err(e) => e,
    }
}

fn assert_notadb(e: GraphError, needle: &str) {
    match e {
        GraphError::Storage {
            source: rusqlite::Error::SqliteFailure(code, Some(msg)),
            ..
        } => {
            assert_eq!(code.code, rusqlite::ErrorCode::NotADatabase, "{msg}");
            assert!(msg.contains(needle), "unexpected message: {msg}");
        }
        other => panic!("expected Storage(NOTADB), got {other:?}"),
    }
}

fn journal_mode(path: &std::path::Path) -> String {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap()
}

/// Create `sql` in a fresh foreign (application_id 0) file; the bytes and
/// journal mode must survive a refused open unchanged.
fn assert_lookalike_refused(sql: &str, needle: &str) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foreign.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(sql).unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    assert_eq!(journal_mode(&path), "delete");
    assert_notadb(open_err(&path), needle);
    assert_eq!(std::fs::read(&path).unwrap(), before, "file was modified");
    assert_eq!(journal_mode(&path), "delete", "journal mode was switched");
    assert!(
        !dir.path().join("foreign.db-wal").exists(),
        "a WAL file was created"
    );
}

#[test]
fn r4_nodes_table_with_other_columns_is_refused_untouched() {
    assert_lookalike_refused(
        "CREATE TABLE nodes(id, title);",
        "table `nodes` does not have the columns graphdblite creates",
    );
}

#[test]
fn r4_edge_props_and_metadata_with_other_columns_are_refused_untouched() {
    assert_lookalike_refused(
        "CREATE TABLE edge_props(k, v, extra); CREATE TABLE metadata(name TEXT, val TEXT);",
        "does not have the columns graphdblite creates",
    );
}

#[test]
fn r4_right_columns_wrong_type_or_key_is_refused() {
    // Same names, but `key` is not a BLOB primary key.
    assert_lookalike_refused(
        "CREATE TABLE adj_out(key TEXT, value TEXT);",
        "table `adj_out` does not have the columns graphdblite creates",
    );
}

#[test]
fn r4_genuine_pre_stamp_graphdblite_file_is_still_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    {
        let mut db = Database::open(&path).unwrap();
        run(&mut db, "CREATE (:Person {name: 'Ann'})");
    }
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA application_id = 0; PRAGMA user_version = 0;")
            .unwrap();
    }
    let mut db = Database::open(&path).unwrap();
    assert_eq!(
        first_int(&run(&mut db, "MATCH (n:Person) RETURN count(n)")),
        1
    );
}

// ------------------------------------------- incremental aggregate semantics

#[test]
fn streaming_aggregates_keep_their_edge_semantics() {
    let mut db = open(100, 0);
    // Integer sum overflow is still an error, a float turns it into a float.
    let e = err(
        &mut db,
        "UNWIND [9223372036854775807, 1] AS x RETURN sum(x + 0) AS s",
    );
    assert!(e.to_string().contains("integer overflow in sum()"), "{e}");
    let rows = run(&mut db, "UNWIND [1, 2.5, null] AS x RETURN sum(x + 0) AS s");
    assert_eq!(rows[0].get("s"), Some(&Value::F64(3.5)));
    // Nulls are skipped by count(expr), min and max; avg of nothing is null.
    let rows = run(
        &mut db,
        "UNWIND [3, null, 1, 2] AS x RETURN count(x + 0) AS c, min(x + 0) AS lo, max(x + 0) AS hi",
    );
    assert_eq!(rows[0].get("c"), Some(&Value::I64(3)));
    assert_eq!(rows[0].get("lo"), Some(&Value::I64(1)));
    assert_eq!(rows[0].get("hi"), Some(&Value::I64(3)));
    let rows = run(&mut db, "UNWIND [null] AS x RETURN avg(x + 0) AS a");
    assert_eq!(rows[0].get("a"), Some(&Value::Null));
    // DISTINCT treats 1 and 1.0 as one value.
    let rows = run(
        &mut db,
        "UNWIND [1, 1.0, 2, 2, null] AS x RETURN count(DISTINCT x + 0) AS c",
    );
    assert_eq!(rows[0].get("c"), Some(&Value::I64(2)));
}

#[test]
fn streaming_percentiles_and_stdev_match_the_definitions() {
    let mut db = open(100, 0);
    let rows = run(
        &mut db,
        "UNWIND [10, 20, 30, 40] AS x RETURN percentileDisc(x + 0, 0.5) AS d, \
         percentileCont(x + 0, 0.5) AS c, stDev(x + 0) AS s, stDevP(x + 0) AS p",
    );
    assert_eq!(rows[0].get("d"), Some(&Value::I64(30)));
    assert_eq!(rows[0].get("c"), Some(&Value::F64(25.0)));
    let near = |v: Option<&Value>, want: f64| match v {
        Some(Value::F64(f)) => assert!((f - want).abs() < 1e-9, "{f} != {want}"),
        other => panic!("expected a float, got {other:?}"),
    };
    near(rows[0].get("s"), 12.909944487358056);
    near(rows[0].get("p"), 11.180339887498949);
    // The percentile argument is still validated, even with no rows.
    let e = err(&mut db, "UNWIND [] AS x RETURN percentileDisc(x, 1.5) AS d");
    assert!(
        e.to_string()
            .contains("percentile must be between 0.0 and 1.0, got 1.5"),
        "{e}"
    );
}

#[test]
fn grouping_treats_integer_and_integral_float_keys_as_one_group() {
    let mut db = open(100, 0);
    let rows = run(
        &mut db,
        "UNWIND [1, 1.0, 2] AS x RETURN x + 0 AS k, count(*) AS c",
    );
    assert_eq!(rows.len(), 2);
    let mut counts: Vec<i64> = rows
        .iter()
        .map(|r| match r.get("c") {
            Some(Value::I64(n)) => *n,
            other => panic!("expected a count, got {other:?}"),
        })
        .collect();
    counts.sort();
    assert_eq!(counts, vec![1, 2]);
}

#[test]
fn unwind_range_matches_range_for_every_step_shape() {
    let mut db = open(0, 0);
    for (args, want) in [
        ("1, 5", vec![1, 2, 3, 4, 5]),
        ("5, 1, -2", vec![5, 3, 1]),
        ("0, 10, 5", vec![0, 5, 10]),
        ("1, 0", vec![]),
        ("3, 3", vec![3]),
    ] {
        let rows = run(&mut db, &format!("UNWIND range({args}) AS x RETURN x"));
        let got: Vec<i64> = rows
            .iter()
            .map(|r| match r.get("x") {
                Some(Value::I64(n)) => *n,
                other => panic!("expected an integer, got {other:?}"),
            })
            .collect();
        assert_eq!(got, want, "range({args})");
        let listed = run(&mut db, &format!("RETURN range({args}) AS l"));
        let want_list = Value::List(want.iter().map(|n| Value::I64(*n)).collect());
        assert_eq!(listed[0].get("l"), Some(&want_list), "range({args}) list");
    }
    let e = err(&mut db, "UNWIND range(1, 5, 0) AS x RETURN x");
    assert!(
        e.to_string()
            .contains("step argument to range() cannot be zero"),
        "{e}"
    );
    let e = err(&mut db, "UNWIND range('a', 5) AS x RETURN x");
    assert!(
        e.to_string()
            .contains("range() start argument must be an integer"),
        "{e}"
    );
    assert!(run(&mut db, "UNWIND range(null, 5) AS x RETURN x").is_empty());
}

#[test]
fn unwind_range_with_limit_does_not_build_the_list() {
    // A 10M-element range under a 1 MiB budget: lazily unwound, only the 5
    // rows that LIMIT asks for are produced.
    let mut db = open(0, MIB);
    let rows = run(
        &mut db,
        "UNWIND range(1, 10000000) AS x RETURN x + 0 AS y LIMIT 5",
    );
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[4].get("y"), Some(&Value::I64(5)));
}
