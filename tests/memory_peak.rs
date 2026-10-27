//! Peak-memory bounds for queries that used to build their whole
//! intermediate result before any limit fired (review findings R2, R3).
//!
//! Each scenario runs a query that is small to write but large to execute
//! and asserts that the process-wide heap peak (dhat `max_bytes`) stays
//! under a bound a few times the configured limits imply. Scenarios run in
//! one process, so the bound applies to the peak so far; they are ordered
//! to make a regression point at the query that caused it.
//!
//! Run: `cargo test --test memory_peak --features dhat-heap --release`

#![cfg(feature = "dhat-heap")]

use graphdblite::{Config, Database, Value};

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

const MIB: u64 = 1 << 20;

fn open(max_rows: usize, budget: u64) -> Database {
    Database::open_with_config(
        ":memory:",
        Config {
            max_result_rows: max_rows,
            max_materialized_bytes: budget,
            ..Config::default()
        },
    )
    .unwrap()
}

fn peak() -> u64 {
    dhat::HeapStats::get().max_bytes as u64
}

fn assert_peak_under(label: &str, bound: u64) {
    let p = peak();
    eprintln!("memory_peak: {label}: peak so far {} KiB", p / 1024);
    assert!(
        p < bound,
        "{label}: heap peak {p} bytes reached the {bound}-byte bound"
    );
}

fn expect_err(db: &mut Database, q: &str, needle: &str) {
    let e = db.execute(q).expect_err(q).to_string();
    assert!(e.contains(needle), "{q}: unexpected error: {e}");
}

fn main() {
    let _profiler = dhat::Profiler::builder().testing().build();

    // Row cap 1000: the work stops at row 1001, not after the full product.
    let mut db = open(1000, 1 << 30);
    db.execute("UNWIND range(1,40) AS i CREATE (:N {id:i})")
        .unwrap();
    let cap = "result set exceeded maximum of 1000 rows";
    expect_err(
        &mut db,
        "MATCH (n:N) UNWIND range(1,20000) AS x RETURN x",
        cap,
    );
    assert_peak_under("unwind over matches", 16 * MIB);
    expect_err(&mut db, "UNWIND range(1,2000000) AS x RETURN x", cap);
    assert_peak_under("unwind of a long range", 16 * MIB);
    expect_err(&mut db, "MATCH (a:N),(b:N),(c:N),(d:N) RETURN a.id", cap);
    assert_peak_under("cartesian product", 16 * MIB);

    // Aggregation streams: 800000 input rows, a handful of groups.
    let rows = db
        .execute("MATCH (n:N) UNWIND range(1,20000) AS x RETURN count(*)")
        .unwrap();
    assert_eq!(rows[0].get("count(*)"), Some(&Value::I64(800_000)));
    assert_peak_under("count(*) over unwind", 16 * MIB);

    // Lists: eight 200000-element lists would be held by buffered rows; the
    // budget stops the third.
    let mut db = open(1_000_000, 64 * MIB);
    expect_err(
        &mut db,
        "UNWIND range(1,8) AS i WITH i, range(1,200000) AS l RETURN i, size(l)",
        "materialized query data in bytes",
    );
    assert_peak_under("lists held by buffered rows", 128 * MIB);

    // Width: 2^17 rows of 17 bound nodes, stopped by the byte budget.
    db.execute("UNWIND range(1,2) AS i CREATE (:N {id:i})")
        .unwrap();
    let mut q = String::from("MATCH (n1:N)");
    for i in 2..=17 {
        q.push_str(&format!(" MATCH (n{i}:N)"));
    }
    q.push_str(" RETURN n1.id");
    expect_err(&mut db, &q, "materialized query data in bytes");
    assert_peak_under("17 chained MATCH", 128 * MIB);
}
